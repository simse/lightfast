//! The scanner worker: a dedicated thread that owns the open scanner.
//!
//! The UI talks to it with [`Command`]s and receives [`Event`]s. Commands run
//! one at a time in the order sent; [`Scanner::cancel`] interrupts the one in
//! progress. Full scans use a second, encoder thread fed through a bounded
//! channel, so slow compression or disk writes back-pressure the scanner
//! rather than piling rows up in memory, and USB reads aren't stalled by
//! encoder bursts.
//!
//! The worker exits once every [`Scanner`] clone has been dropped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use scan::{CancelToken, Channels, Device, DeviceInfo, Parameters, ScanSettings, SourceKind};

use crate::job::{Area, Estimate, Job, ScanMode};
use crate::raster::{PixelFormat, RgbImage, RowAssembler, Thumbnail, to_rgb8};
use crate::sink;

/// Resolution used for the full-bed preview.
///
/// Sharp enough to place frames when zoomed in. Flatbeds like the `LiDE` 400
/// move the head at the same speed up to 300 DPI, so it costs no time over
/// lower resolutions. The image (≈2550×3510 px for A4, ~27 MB) stays well
/// under GPU texture limits.
pub const PREVIEW_DPI: u32 = 300;

/// Rough width of live thumbnails sent while scanning. Wide enough to look
/// sharp in the preview pane on a Retina display, small enough to convert
/// to a GPU texture every [`THUMB_INTERVAL`].
const THUMB_WIDTH: usize = 1200;
const THUMB_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Rows per batch sent to the encoder. At 40k px wide that's ~8 MB.
const BATCH_ROWS: usize = 64;
/// Batches in flight between reader and encoder (~128 MB worst case).
const BATCH_QUEUE: usize = 16;
/// Size of each device read.
const READ_BUF: usize = 1 << 20;

/// Bytes of free space to keep on top of the raw image size estimate.
const DISK_HEADROOM: u64 = 256 << 20;

/// What the connected scanner offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    pub device: DeviceInfo,
    /// Every usable scan source; never empty.
    pub sources: Vec<SourceCaps>,
}

impl Caps {
    /// The source named `name`, or the first one for `None`.
    #[must_use]
    pub fn source(&self, name: Option<&str>) -> Option<&SourceCaps> {
        match name {
            None => self.sources.first(),
            Some(n) => self.sources.iter().find(|s| s.name == n),
        }
    }
}

/// What one scan source (flatbed, transparency unit, feeder) offers.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceCaps {
    /// The backend's name for the source, e.g. `Flatbed`.
    pub name: String,
    /// Bed width in millimetres.
    pub bed_w: f64,
    /// Bed height in millimetres.
    pub bed_h: f64,
    /// Supported resolutions, ascending.
    pub dpis: Vec<u32>,
    /// Supported modes, in the backend's order; never empty.
    pub modes: Vec<ScanMode>,
    /// The gamma range, if this source's gamma is adjustable.
    pub gamma: Option<GammaRange>,
    /// Whether this source feeds sheets, so a preview would use up a page.
    pub feeder: bool,
}

impl SourceCaps {
    /// The mode to start with: 8-bit colour if offered.
    #[must_use]
    pub fn default_mode(&self) -> &ScanMode {
        self.mode_for(PixelFormat::Rgb8)
    }

    /// The first mode producing `pixel`, falling back to 8-bit colour, then
    /// 8-bit gray, then whatever comes first.
    #[must_use]
    pub fn mode_for(&self, pixel: PixelFormat) -> &ScanMode {
        [pixel, PixelFormat::Rgb8, PixelFormat::Gray8]
            .into_iter()
            .find_map(|p| self.modes.iter().find(|m| m.pixel == p))
            .unwrap_or(&self.modes[0])
    }
}

/// The values the gamma setting accepts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GammaRange {
    pub min: f64,
    pub max: f64,
    /// The backend's default.
    pub default: f64,
}

/// A request for the worker thread.
#[derive(Debug)]
pub enum Command {
    /// Finds the first attached scanner and opens it, replacing any open one.
    Discover,
    /// Scans the whole bed of `source` at [`PREVIEW_DPI`] in 8-bit colour
    /// (or gray) and returns it in memory.
    Preview { source: Option<String>, gamma: Option<f64> },
    /// Scans the bounding box of `Job::outputs` in one pass and writes
    /// each output's area to its own file.
    Scan(Job),
}

/// A notification from the worker thread.
#[derive(Debug)]
pub enum Event {
    /// Discovery started; it can take several seconds.
    Discovering,
    /// A scanner is open and ready for commands.
    Ready(Caps),
    /// Discovery found nothing, or the scanner disappeared.
    NoScanner,
    /// A frame started; the dimensions are in output pixels.
    Started { width: usize, height: usize, pixel: PixelFormat },
    /// Rows read so far out of the expected total.
    Progress { rows: usize, total: usize },
    /// Live low-res view of the scan in progress.
    Thumb(Arc<RgbImage>),
    /// The finished full-bed preview.
    PreviewDone(Arc<RgbImage>),
    /// A scan was written to these paths, one per [`crate::job::Output`].
    Saved(Vec<PathBuf>),
    /// The running command was stopped by [`Scanner::cancel`].
    Cancelled,
    /// The running command failed; the message is meant for the user.
    Failed(String),
}

/// Handle to the worker thread.
#[derive(Debug, Clone)]
pub struct Scanner {
    commands: mpsc::Sender<Command>,
    cancel: CancelToken,
}

impl Scanner {
    /// Spawns the worker; events are delivered on the returned receiver.
    ///
    /// # Errors
    /// Fails if the OS refuses to create the thread.
    pub fn spawn() -> std::io::Result<(Self, async_channel::Receiver<Event>)> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        thread::Builder::new().name("scanner".into()).spawn(move || worker(&cmd_rx, &ev_tx, &worker_cancel))?;
        Ok((Self { commands: cmd_tx, cancel }, ev_rx))
    }

    /// Queues `cmd`, clearing any earlier cancel request.
    pub fn send(&self, cmd: Command) {
        self.cancel.reset();
        if self.commands.send(cmd).is_err() {
            log::warn!("scanner worker has exited; command dropped");
        }
    }

    /// Requests that the running preview or scan stop as soon as possible.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// An open scanner and what it offers.
type Open = (Box<dyn Device>, Caps);

fn worker(commands: &mpsc::Receiver<Command>, events: &async_channel::Sender<Event>, cancel: &CancelToken) {
    let emit = |e| {
        // The receiver is gone only when the UI is shutting down.
        if events.send_blocking(e).is_err() {
            log::debug!("event receiver dropped");
        }
    };
    let mut open: Option<Open> = None;

    for cmd in commands {
        let result = match cmd {
            Command::Discover => {
                open = None;
                emit(Event::Discovering);
                match discover() {
                    Ok(Some((device, caps))) => {
                        emit(Event::Ready(caps.clone()));
                        open = Some((device, caps));
                    }
                    Ok(None) => emit(Event::NoScanner),
                    Err(e) => emit(Event::Failed(format!("{e:#}"))),
                }
                continue;
            }
            Command::Preview { source, gamma } => match open.as_mut() {
                Some((device, caps)) => preview(device.as_mut(), caps, source.as_deref(), gamma, cancel, &emit),
                None => Err(anyhow!("No scanner connected.")),
            },
            Command::Scan(job) => match open.as_mut() {
                Some((device, caps)) => scan(device.as_mut(), caps, &job, cancel, &emit),
                None => Err(anyhow!("No scanner connected.")),
            },
        };
        let Err(e) = result else { continue };
        if cancel.is_cancelled() {
            log::info!(reason:% = e; "operation cancelled");
            emit(Event::Cancelled);
            continue;
        }
        log::error!(error:% = format!("{e:#}"); "scan failed");
        emit(Event::Failed(format!("{e:#}")));
        if open.is_some() {
            open = reopen(open.take());
            if open.is_none() {
                emit(Event::NoScanner);
            }
        }
    }
}

/// Closes and reopens a device after a failed frame, so a transfer error
/// can't leave the connection in a bad state.
fn reopen(open: Option<Open>) -> Option<Open> {
    let (device, caps) = open?;
    drop(device);
    match scan::open(&caps.device.name) {
        Ok(device) => Some((device, caps)),
        Err(e) => {
            log::error!(device = caps.device.name.as_str(), error:% = e; "reopening scanner failed");
            None
        }
    }
}

fn discover() -> Result<Option<Open>> {
    let devices = scan::list_devices().context("listing scanners")?;
    log::info!(devices:? = devices; "found devices");
    let Some(info) = devices.into_iter().next() else { return Ok(None) };
    let device = scan::open(&info.name).with_context(|| format!("opening {} {}", info.vendor, info.model))?;
    let sources: Vec<SourceCaps> = device
        .capabilities()
        .sources
        .iter()
        .filter_map(|s| match source_caps(s) {
            Ok(caps) => Some(caps),
            Err(e) => {
                log::warn!(source = s.name.as_str(), error:% = format!("{e:#}"); "skipping unusable scan source");
                None
            }
        })
        .collect();
    if sources.is_empty() {
        bail!("{} {} offers no scan source this app can use", info.vendor, info.model);
    }
    let caps = Caps { device: device.info().clone(), sources };
    log::info!(caps:? = caps; "scanner ready");
    Ok(Some((device, caps)))
}

/// The pixel format of a frame, if this crate can handle it.
fn pixel_format(layout: scan::PixelLayout) -> Option<PixelFormat> {
    PixelFormat::new(layout.channels == Channels::Rgb, layout.depth)
}

/// Converts a backend source description, keeping the modes this crate can
/// decode.
fn source_caps(src: &scan::SourceCaps) -> Result<SourceCaps> {
    let mut modes = Vec::new();
    let mut dpis = Vec::new();
    for m in &src.modes {
        let Some(pixel) = pixel_format(m.layout) else {
            log::info!(mode = m.name.as_str(), layout:? = m.layout; "skipping mode with unsupported frames");
            continue;
        };
        modes.push(ScanMode { label: String::new(), backend_mode: m.name.clone(), pixel });
        dpis.extend(&m.resolutions);
    }
    if modes.is_empty() {
        bail!("scanner offers no mode with 1-, 8- or 16-bit gray or RGB frames");
    }
    dpis.sort_unstable();
    dpis.dedup();
    label_modes(&mut modes);
    Ok(SourceCaps {
        name: src.name.clone(),
        bed_w: src.width_mm,
        bed_h: src.height_mm,
        dpis,
        modes,
        gamma: src.gamma.map(|(r, default)| GammaRange { min: r.min, max: r.max, default }),
        feeder: src.kind == SourceKind::Feeder,
    })
}

/// Names modes by their pixel format, or by the backend's own name where
/// two modes share a format (e.g. "Lineart" and "Halftone").
fn label_modes(modes: &mut [ScanMode]) {
    for i in 0..modes.len() {
        let shared = modes.iter().filter(|m| m.pixel == modes[i].pixel).count() > 1;
        let m = &modes[i];
        modes[i].label = if shared { m.backend_mode.clone() } else { m.pixel.label().to_owned() };
    }
}

/// The scan settings for one frame.
fn settings(source: &SourceCaps, mode: &ScanMode, gamma: Option<f64>, area: Area, dpi: u32) -> ScanSettings {
    ScanSettings {
        source: source.name.clone(),
        mode: mode.backend_mode.clone(),
        resolution: dpi,
        area: scan::Area { x: area.x, y: area.y, width: area.w, height: area.h },
        gamma: gamma.filter(|_| source.gamma.is_some()),
        extras: Vec::new(),
    }
}

/// Reads one frame, handing packed row batches to `on_rows`.
///
/// `on_start` receives `(width, height, pixel)` once the frame geometry is
/// known and returns the batch handler. Always ends the frame.
fn acquire<F>(
    device: &mut dyn Device,
    settings: &ScanSettings,
    cancel: &CancelToken,
    emit: &impl Fn(Event),
    on_start: impl FnOnce(usize, usize, PixelFormat) -> Result<F>,
) -> Result<()>
where
    F: FnMut(Vec<u8>) -> Result<()>,
{
    let result = device.start(settings, cancel).context("starting scan").and_then(|params| {
        log::info!(params:? = params; "frame started");
        read_frame(device, params, cancel, emit, on_start)
    });
    device.finish();
    result
}

/// The body of [`acquire`] between starting and finishing the frame.
fn read_frame<F>(
    device: &mut dyn Device,
    params: Parameters,
    cancel: &CancelToken,
    emit: &impl Fn(Event),
    on_start: impl FnOnce(usize, usize, PixelFormat) -> Result<F>,
) -> Result<()>
where
    F: FnMut(Vec<u8>) -> Result<()>,
{
    let Some(pixel) = pixel_format(params.layout) else {
        bail!("unsupported frame: {:?}", params.layout);
    };
    let (width, height) = (params.pixels_per_line, params.lines);
    emit(Event::Started { width, height, pixel });
    let mut on_rows = on_start(width, height, pixel)?;

    let mut asm = RowAssembler::new(params.bytes_per_line, width, pixel)?;
    let batch_bytes = asm.out_row_bytes() * BATCH_ROWS;
    let mut thumb = Thumbnail::new(width, height, THUMB_WIDTH, pixel);
    let mut buf = vec![0u8; READ_BUF];
    let mut batch = Vec::with_capacity(batch_bytes);
    let mut rows = 0;
    let (mut last_progress, mut last_thumb) = (Instant::now(), Instant::now());

    loop {
        cancel.check()?;
        let n = device.read(&mut buf).context("reading from scanner")?;
        if n == 0 {
            break;
        }
        rows += asm.push(&buf[..n], &mut batch);
        if batch.len() >= batch_bytes {
            thumb.push_rows(&batch);
            on_rows(std::mem::replace(&mut batch, Vec::with_capacity(batch_bytes)))?;
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            emit(Event::Progress { rows, total: height });
            last_progress = Instant::now();
        }
        if last_thumb.elapsed() >= THUMB_INTERVAL {
            emit(Event::Thumb(Arc::new(thumb.image().clone())));
            last_thumb = Instant::now();
        }
    }
    if !batch.is_empty() {
        thumb.push_rows(&batch);
        on_rows(batch)?;
    }
    emit(Event::Progress { rows, total: height });
    log::info!(rows, expected = height; "frame complete");
    Ok(())
}

fn preview(
    device: &mut dyn Device,
    caps: &Caps,
    source: Option<&str>,
    gamma: Option<f64>,
    cancel: &CancelToken,
    emit: &impl Fn(Event),
) -> Result<()> {
    let src = caps.source(source).with_context(|| format!("unknown scan source {source:?}"))?;
    if src.feeder {
        bail!("Preview isn't available for a document feeder.");
    }
    let area = Area::new(0.0, 0.0, src.bed_w, src.bed_h);
    let mode = src.mode_for(PixelFormat::Rgb8);
    let settings = settings(src, mode, gamma, area, PREVIEW_DPI);
    // The preview is small enough (≈2550×3510 at 300 DPI) to keep whole.
    let mut image: Option<RgbImage> = None;
    acquire(device, &settings, cancel, emit, |width, height, pixel| {
        let img = image.insert(RgbImage { width, height, data: Vec::new() });
        img.data.reserve_exact(img.byte_len().context("preview too large")?);
        Ok(move |rows: Vec<u8>| {
            to_rgb8(pixel, width, &rows, &mut img.data);
            Ok(())
        })
    })?;
    let mut img = image.context("preview produced no image")?;
    // Pad a short frame with white, as if the bed were empty there.
    let len = img.byte_len().context("preview too large")?;
    img.data.resize(len, 0xFF);
    emit(Event::PreviewDone(Arc::new(img)));
    Ok(())
}

/// The temporary sibling a scan is written to before being renamed into place.
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// The encoder thread and the channel feeding it.
type Encoder = (mpsc::SyncSender<Result<Vec<u8>>>, thread::JoinHandle<Result<()>>);

fn check_disk_space(dir: &Path, bytes: u64) -> Result<()> {
    // If free space can't be queried, let the write itself report problems.
    let Ok(free) = fs4::available_space(dir) else { return Ok(()) };
    // Compressed formats are usually smaller, but plan for the worst.
    if free < bytes.saturating_add(DISK_HEADROOM) {
        bail!(
            "Not enough disk space: need up to {}, {} free.",
            crate::job::human_bytes(bytes),
            crate::job::human_bytes(free)
        );
    }
    Ok(())
}

/// A rectangle of a frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Maps `area` to pixels of a `width` × `height` frame covering `scan`.
///
/// Scales by the frame's real size rather than the DPI, since backends
/// round the scan area to their own steps. The result lies within the
/// frame and is at least 1 px on each side.
fn pixel_span(area: Area, scan: Area, width: usize, height: usize) -> Span {
    let axis = |pos: f64, len: f64, origin: f64, extent: f64, px: usize| {
        let to_px = |mm: f64| {
            let v =
                ((mm - origin) / extent.max(f64::EPSILON) * f64::from(u32::try_from(px).unwrap_or(u32::MAX))).round();
            num_traits::ToPrimitive::to_usize(&v).unwrap_or(0).min(px)
        };
        let start = to_px(pos).min(px.saturating_sub(1));
        let end = to_px(pos + len).max(start + 1).min(px.max(1));
        (start, end - start)
    };
    let (x, w) = axis(area.x, area.w, scan.x, scan.w, width);
    let (y, h) = axis(area.y, area.h, scan.y, scan.h, height);
    Span { x, y, w, h }
}

/// Where one output's rows go while the frame is read.
struct Route {
    span: Span,
    /// Whether the span is the whole frame width, so rows need no cropping.
    full_width: bool,
    tx: mpsc::SyncSender<Result<Vec<u8>>>,
}

impl Route {
    /// Sends the part of `rows`, which start at frame row `first`, that this
    /// output covers. `take` lets the last route reuse the batch.
    fn send(&self, rows: &mut Vec<u8>, first: usize, in_row: usize, pixel: PixelFormat, take: bool) -> Result<()> {
        let count = rows.len() / in_row;
        let lo = self.span.y.max(first);
        let hi = (self.span.y + self.span.h).min(first + count);
        if lo >= hi {
            return Ok(());
        }
        let batch = if self.full_width && lo == first && hi == first + count {
            if take { std::mem::take(rows) } else { rows.clone() }
        } else if self.full_width {
            rows[(lo - first) * in_row..(hi - first) * in_row].to_vec()
        } else {
            let out_row = pixel.row_bytes(self.span.w).context("frame too wide")?;
            let mut out = Vec::with_capacity((hi - lo) * out_row);
            for row in rows[(lo - first) * in_row..(hi - first) * in_row].chunks_exact(in_row) {
                pixel.crop_row(row, self.span.x, self.span.w, &mut out);
            }
            out
        };
        // A send error means the encoder already failed; its error is
        // reported when joined.
        self.tx.send(Ok(batch)).context("encoder stopped")
    }
}

fn scan(device: &mut dyn Device, caps: &Caps, job: &Job, cancel: &CancelToken, emit: &impl Fn(Event)) -> Result<()> {
    let area = job.scan_area();
    let areas: Vec<Area> = job.outputs.iter().map(|o| o.area).collect();
    let dir = job.outputs[0].dest.parent().context("invalid destination")?;
    check_disk_space(dir, crate::job::total_raw_bytes(&areas, job.dpi, job.mode.pixel))?;
    let src = caps.source(job.source.as_deref()).with_context(|| format!("unknown scan source {:?}", job.source))?;
    let settings = settings(src, &job.mode, job.gamma, area, job.dpi);
    let expected = Estimate::new(area, job.dpi, job.mode.pixel);
    log::debug!(width = expected.width, height = expected.height; "expected frame size");

    let parts: Vec<PathBuf> = job.outputs.iter().map(|o| part_path(&o.dest)).collect();
    let mut encoders: Vec<Encoder> = Vec::with_capacity(parts.len());
    let started = Instant::now();

    let read_result = acquire(device, &settings, cancel, emit, |width, height, pixel| {
        if pixel != job.mode.pixel {
            log::warn!(expected:% = job.mode.pixel, actual:% = pixel; "scanner sent a different pixel format");
        }
        let mut routes = Vec::with_capacity(job.outputs.len());
        for (output, part) in job.outputs.iter().zip(&parts) {
            let span = pixel_span(output.area, area, width, height);
            let spec =
                sink::Spec { width: u32::try_from(span.w)?, height: u32::try_from(span.h)?, dpi: job.dpi, pixel };
            log::info!(
                format:% = job.format, pixel:% = pixel, x = span.x, y = span.y, width = span.w, height = span.h,
                path:% = part.display();
                "writing scan"
            );
            let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>>>(BATCH_QUEUE);
            let (format, path, rows) = (job.format, part.clone(), span.h);
            let join = thread::Builder::new().name("encoder".into()).spawn(move || {
                let src = sink::fit_height(rx.into_iter(), spec.row_bytes(), rows, pixel.white_byte());
                sink::write(format, &path, spec, src)
            })?;
            encoders.push((tx.clone(), join));
            routes.push(Route { span, full_width: span.x == 0 && span.w == width, tx });
        }
        let in_row = pixel.row_bytes(width).context("frame too wide")?;
        let mut first = 0;
        Ok(move |mut rows: Vec<u8>| {
            // Counted first: the last route may take the batch.
            let count = rows.len() / in_row;
            let last = routes.len() - 1;
            for (i, route) in routes.iter().enumerate() {
                route.send(&mut rows, first, in_row, pixel, i == last)?;
            }
            first += count;
            Ok(())
        })
    });

    let mut encode_result = Ok(());
    for (tx, join) in encoders {
        if read_result.is_err() {
            // Abort the encoder rather than letting it pad gigabytes. If
            // it already stopped, its own error is what we report.
            if tx.send(Err(anyhow!("cancelled"))).is_err() {
                log::debug!("encoder already stopped");
            }
        }
        drop(tx);
        // Nothing recovers from panics (release builds abort), so an
        // encoder panic is re-raised here rather than reported as an error.
        let joined = join.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        if encode_result.is_ok() {
            encode_result = joined;
        }
    }

    let result = match (read_result, encode_result) {
        (Ok(()), Ok(())) => Ok(()),
        // An encoder failure explains a reader "encoder stopped" error.
        (_, Err(e)) if !cancel.is_cancelled() => Err(e.context("writing image")),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    if let Err(e) = result {
        for part in &parts {
            match std::fs::remove_file(part) {
                Err(rm) if rm.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!(path:% = part.display(), error:% = rm; "couldn't remove partial file");
                }
                _ => {}
            }
        }
        return Err(e);
    }
    let mut saved = Vec::with_capacity(parts.len());
    for (part, output) in parts.iter().zip(&job.outputs) {
        std::fs::rename(part, &output.dest).with_context(|| format!("renaming to {}", output.dest.display()))?;
        saved.push(output.dest.clone());
    }
    log::info!(files = saved.len(), elapsed:? = started.elapsed(); "scan saved");
    emit(Event::Saved(saved));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(backend_mode: &str, pixel: PixelFormat) -> ScanMode {
        ScanMode { label: String::new(), backend_mode: backend_mode.into(), pixel }
    }

    #[test]
    fn modes_are_named_by_format_unless_formats_repeat() {
        let mut modes = [
            mode("Color", PixelFormat::Rgb8),
            mode("48 bits color", PixelFormat::Rgb16),
            mode("Lineart", PixelFormat::Lineart),
            mode("Halftone", PixelFormat::Lineart),
            mode("Gray", PixelFormat::Gray8),
        ];
        label_modes(&mut modes);
        let labels: Vec<_> = modes.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Color", "Color 16-bit", "Lineart", "Halftone", "Gray"]);
    }

    #[test]
    fn mode_for_falls_back_to_8_bit_color_then_gray() {
        let source = |modes| SourceCaps {
            name: "Flatbed".into(),
            bed_w: 1.0,
            bed_h: 1.0,
            dpis: vec![75],
            modes,
            gamma: None,
            feeder: false,
        };
        let full = source(vec![mode("Gray", PixelFormat::Gray8), mode("Color", PixelFormat::Rgb8)]);
        assert_eq!(full.default_mode().backend_mode, "Color");
        assert_eq!(full.mode_for(PixelFormat::Gray8).backend_mode, "Gray");
        assert_eq!(full.mode_for(PixelFormat::Rgb16).backend_mode, "Color");
        let gray_only = source(vec![mode("Lineart", PixelFormat::Lineart), mode("Gray", PixelFormat::Gray8)]);
        assert_eq!(gray_only.default_mode().backend_mode, "Gray");
        let lineart_only = source(vec![mode("Lineart", PixelFormat::Lineart)]);
        assert_eq!(lineart_only.default_mode().backend_mode, "Lineart");
    }

    fn backend_mode(name: &str, channels: Channels, depth: u32, resolutions: &[u32]) -> scan::ModeCaps {
        scan::ModeCaps {
            name: name.into(),
            layout: scan::PixelLayout { channels, depth },
            resolutions: resolutions.to_vec(),
        }
    }

    #[test]
    fn backend_sources_convert_to_app_caps() {
        let src = scan::SourceCaps {
            name: "ADF".into(),
            kind: SourceKind::Feeder,
            width_mm: 216.0,
            height_mm: 356.0,
            modes: vec![
                backend_mode("Color", Channels::Rgb, 8, &[300, 75, 150]),
                backend_mode("Weird", Channels::Rgb, 12, &[9600]),
                backend_mode("Gray", Channels::Gray, 16, &[600, 300]),
            ],
            gamma: Some((scan::Range { min: 0.3, max: 5.0 }, 2.2)),
            extras: Vec::new(),
        };
        let caps = source_caps(&src).unwrap();
        assert!(caps.feeder);
        assert_eq!(caps.dpis, [75, 150, 300, 600], "unusable modes' resolutions are dropped");
        let modes: Vec<_> = caps.modes.iter().map(|m| (m.backend_mode.as_str(), m.pixel)).collect();
        assert_eq!(modes, [("Color", PixelFormat::Rgb8), ("Gray", PixelFormat::Gray16)]);
        assert_eq!(caps.gamma, Some(GammaRange { min: 0.3, max: 5.0, default: 2.2 }));
        let none = scan::SourceCaps { modes: vec![backend_mode("Weird", Channels::Rgb, 12, &[75])], ..src };
        source_caps(&none).unwrap_err();
    }

    #[test]
    fn pixel_span_scales_by_the_real_frame_size() {
        let scan = Area::new(10.0, 20.0, 100.0, 50.0);
        // The backend returned 1000 × 500 px for 100 × 50 mm.
        let inner = pixel_span(Area::new(30.0, 20.0, 50.0, 25.0), scan, 1000, 500);
        assert_eq!(inner, Span { x: 200, y: 0, w: 500, h: 250 });
        assert_eq!(pixel_span(scan, scan, 999, 501), Span { x: 0, y: 0, w: 999, h: 501 });
    }

    #[test]
    fn pixel_span_stays_on_the_frame() {
        let scan = Area::new(0.0, 0.0, 100.0, 100.0);
        let edge = pixel_span(Area::new(100.0, 99.99, 0.0, 0.0), scan, 10, 10);
        assert_eq!(edge, Span { x: 9, y: 9, w: 1, h: 1 });
        let past = pixel_span(Area::new(-5.0, 50.0, 200.0, 80.0), scan, 10, 10);
        assert_eq!(past, Span { x: 0, y: 5, w: 10, h: 5 });
    }

    fn route(span: Span, full_width: bool) -> (Route, mpsc::Receiver<Result<Vec<u8>>>) {
        let (tx, rx) = mpsc::sync_channel(16);
        (Route { span, full_width, tx }, rx)
    }

    #[test]
    fn routes_crop_their_rows_and_columns() {
        // 4 × 4 gray frame, rows 2..4 arriving after rows 0..2.
        let frame: Vec<u8> = (0..16).collect();
        let (r, rx) = route(Span { x: 1, y: 1, w: 2, h: 2 }, false);
        let mut first = frame[..8].to_vec();
        r.send(&mut first, 0, 4, PixelFormat::Gray8, false).unwrap();
        let mut second = frame[8..].to_vec();
        r.send(&mut second, 2, 4, PixelFormat::Gray8, false).unwrap();
        let got: Vec<u8> = rx.try_iter().flat_map(Result::unwrap).collect();
        assert_eq!(got, [5, 6, 9, 10]);
    }

    #[test]
    fn full_width_routes_forward_rows() {
        let mut rows: Vec<u8> = (0..12).collect();
        let (r, rx) = route(Span { x: 0, y: 1, w: 4, h: 5 }, true);
        r.send(&mut rows, 0, 4, PixelFormat::Gray8, true).unwrap();
        assert_eq!(rx.try_recv().unwrap().unwrap(), (4..12).collect::<Vec<u8>>());
        assert_eq!(rows.len(), 12, "a partial batch is copied, not taken");
        let (whole, rx) = route(Span { x: 0, y: 0, w: 4, h: 3 }, true);
        whole.send(&mut rows, 0, 4, PixelFormat::Gray8, true).unwrap();
        assert_eq!(rx.try_recv().unwrap().unwrap().len(), 12);
        assert!(rows.is_empty(), "the last route takes a whole batch");
    }

    #[test]
    fn part_path_is_a_sibling() {
        assert_eq!(part_path(Path::new("/a/scan.tif")), Path::new("/a/scan.tif.part"));
    }
}

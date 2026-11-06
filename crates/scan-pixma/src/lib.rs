//! Canon PIXMA and `CanoScan` `LiDE` driver over USB.
//!
//! A port of SANE's `pixma` backend for the models in [`models::MODELS`]:
//! the mp150 subdriver's generation 5 protocol, used by the `LiDE` 300 and
//! 400. The firmware calibrates and controls the motor; the host sends
//! commands and a gamma table and post-processes the image lines.
//!
//! Behaviour matches C SANE transfer for transfer and byte for byte; the
//! tests replay USB sessions recorded from C SANE to check that.

#![forbid(unsafe_code)]

mod image;
pub mod models;
mod mp150;
mod protocol;

use scan_core::{
    Area, CancelToken, Capabilities, Channels, Device, DeviceInfo, Error, ErrorKind, ExtraOption, ModeCaps, OptionKind,
    OptionValue, Parameters, PixelLayout, Range, Result, ScanSettings, SourceCaps, SourceKind,
};
use scan_usb::UsbTransport;

use crate::image::Lineart;
use crate::models::Model;
use crate::mp150::{Calibrate, Mode, Mp150, ScanParam};
use crate::protocol::{Io, PixmaError};

/// Prefix of pixma device names.
pub const BACKEND: &str = "pixma";

/// Default gamma, as in SANE.
const DEFAULT_GAMMA: f64 = 2.2;
/// Accepted gamma range.
const GAMMA_RANGE: Range = Range { min: 0.3, max: 5.0 };
/// Lineart threshold in percent, as in SANE.
const DEFAULT_THRESHOLD: i32 = 50;

const SOURCE_FLATBED: &str = "Flatbed";
const CALIBRATE_CHOICES: [&str; 3] = ["Once", "Always", "Never"];

/// Rounds to SANE's 16.16 fixed point and back, truncating like `SANE_FIX`.
/// Settings go through this so pixel maths and gamma tables match C SANE
/// exactly.
#[expect(clippy::cast_possible_truncation, reason = "SANE_FIX truncates")]
fn fixed(v: f64) -> f64 {
    f64::from((v * 65536.0) as i32) / 65536.0
}

/// Whether `vid`:`pid` is a scanner this driver supports.
#[must_use]
pub fn supports(vid: u16, pid: u16) -> bool {
    models::find(vid, pid).is_some()
}

/// SANE-compatible device name for a scanner: `pixma:<VID><PID>_<serial>`.
#[must_use]
pub fn device_name(vid: u16, pid: u16, serial: Option<&str>) -> String {
    let mut name = format!("{BACKEND}:{vid:04X}{pid:04X}");
    if let Some(serial) = serial {
        name.push('_');
        name.push_str(serial);
    }
    name
}

fn mode_list(model: &Model) -> Vec<(&'static str, Mode)> {
    let mut list = vec![("Color", Mode::Color), ("Gray", Mode::Gray)];
    if model.high_depth {
        list.push(("48 bits color", Mode::Color48));
        list.push(("16 bits gray", Mode::Gray16));
    }
    list.push(("Lineart", Mode::Lineart));
    list
}

fn layout(mode: Mode) -> PixelLayout {
    match mode {
        Mode::Color => PixelLayout { channels: Channels::Rgb, depth: 8 },
        Mode::Gray => PixelLayout { channels: Channels::Gray, depth: 8 },
        Mode::Color48 => PixelLayout { channels: Channels::Rgb, depth: 16 },
        Mode::Gray16 => PixelLayout { channels: Channels::Gray, depth: 16 },
        Mode::Lineart => PixelLayout { channels: Channels::Gray, depth: 1 },
    }
}

/// Resolutions from 75 dpi doubling up to the model's maximum
/// (`create_dpi_list` for the flatbed).
fn resolutions(model: &Model) -> Vec<u32> {
    std::iter::successors(Some(75u32), |&d| (d < model.xdpi).then_some(d * 2)).collect()
}

/// Bed size in mm, as SANE's `fixed` option range reports it.
fn bed_mm(units: u32) -> f64 {
    fixed(f64::from(units) / 75.0 * 25.4)
}

fn capabilities(model: &Model) -> Capabilities {
    let dpis = resolutions(model);
    let mode_caps = mode_list(model)
        .into_iter()
        .map(|(name, mode)| ModeCaps { name: name.into(), layout: layout(mode), resolutions: dpis.clone() })
        .collect();
    let extras = vec![
        ExtraOption {
            name: "calibrate".into(),
            title: "Calibration".into(),
            kind: OptionKind::Choice(CALIBRATE_CHOICES.iter().map(|s| (*s).to_owned()).collect()),
            default: OptionValue::Choice("Once".into()),
        },
        ExtraOption {
            name: "threshold".into(),
            title: "Lineart threshold (%)".into(),
            kind: OptionKind::Int { min: 0, max: 100 },
            default: OptionValue::Int(DEFAULT_THRESHOLD),
        },
        ExtraOption {
            name: "threshold-curve".into(),
            title: "Lineart threshold curve".into(),
            kind: OptionKind::Int { min: 0, max: 127 },
            default: OptionValue::Int(0),
        },
    ];
    Capabilities {
        sources: vec![SourceCaps {
            name: SOURCE_FLATBED.into(),
            kind: SourceKind::Flatbed,
            width_mm: bed_mm(model.width),
            height_mm: bed_mm(model.height),
            modes: mode_caps,
            gamma: Some((GAMMA_RANGE, DEFAULT_GAMMA)),
            extras,
        }],
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Invalid, msg)
}

/// `pixma_check_dpi`: 75 dpi times a power of two, up to `max`.
fn check_dpi(dpi: u32, max: u32) -> Result<()> {
    let ratio = dpi / 75;
    if dpi > max || dpi < 75 || 75 * ratio != dpi || !ratio.is_power_of_two() {
        return Err(invalid(format!("{dpi} dpi is not supported")));
    }
    Ok(())
}

/// Clamps an origin and length to the bed (`CLAMP2`), all in pixels.
fn clamp2(x: &mut u32, w: &mut u32, min: u32, max_units: u32, dpi: u32) {
    let m = max_units * dpi / 75;
    *x = (*x).min(m.saturating_sub(min));
    *w = (*w).min(m - *x);
    if *w < min {
        *w = min;
    }
}

/// The open-device state that resolving a scan depends on.
#[derive(Debug, Clone, Copy)]
struct Resolver<'a> {
    model: &'a Model,
    caps: &'a Capabilities,
}

impl Resolver<'_> {
    /// `mp150_check_param`, which the C code runs twice per scan.
    fn check_param(&self, sp: &mut ScanParam, scale: u32) {
        sp.channels = 3;
        sp.software_lineart = false;
        match sp.mode {
            Mode::Gray => (sp.channels, sp.depth) = (1, 8),
            Mode::Color => sp.depth = 8,
            Mode::Gray16 => (sp.channels, sp.depth) = (1, 16),
            Mode::Color48 => (sp.channels, sp.depth) = (3, 16),
            Mode::Lineart => (sp.software_lineart, sp.channels, sp.depth) = (true, 1, 1),
        }
        // Software lineart packs 8 pixels per byte.
        if sp.software_lineart && !sp.w.is_multiple_of(8) {
            sp.w += 8 - sp.w % 8;
            let mut w_max = self.model.width * self.model.xdpi / 75;
            w_max -= w_max % 8;
            sp.w = sp.w.min(w_max);
        }
        sp.xs = (sp.x * scale) % 32;
        sp.wx = (sp.w * scale + sp.xs).div_ceil(32) * 32;
        let bytes = if sp.software_lineart { 8 } else { sp.depth } / 8;
        sp.line_size = u64::from(sp.w) * u64::from(sp.channels) * u64::from(bytes);
    }

    /// Shrink factor for `xdpi` below the hardware minimum.
    fn scale_for(&self, xdpi: u32) -> u32 {
        if self.model.min_xdpi != 0 && xdpi < self.model.min_xdpi { self.model.min_xdpi / xdpi } else { 1 }
    }

    /// Resolves settings into a scan (`calc_scan_param` and
    /// `pixma_check_scan_param`). Returns the scan and its shrink factor.
    fn resolve(&self, s: &ScanSettings) -> Result<(ScanParam, u32)> {
        let source =
            self.caps.source(&s.source).ok_or_else(|| invalid(format!("no scan source named '{}'", s.source)))?;
        let mode = mode_list(self.model)
            .into_iter()
            .find(|(name, _)| *name == s.mode)
            .map(|(_, m)| m)
            .ok_or_else(|| invalid(format!("no scan mode named '{}'", s.mode)))?;
        let dpi = s.resolution;
        check_dpi(dpi, self.model.xdpi)?;
        check_dpi(dpi, self.model.ydpi)?;

        // Corners in fixed point, clamped to the bed like SANE's range check.
        let corners = |pos: f64, len: f64, max: f64| {
            let lo = fixed(pos.clamp(0.0, max));
            let hi = fixed((pos + len).clamp(0.0, max));
            (lo.min(hi), lo.max(hi))
        };
        let Area { x, y, width, height } = s.area;
        let (x1, x2) = corners(x, width, source.width_mm);
        let (y1, y2) = corners(y, height, source.height_mm);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "C's (int) cast of a positive value"
        )]
        let px = |mm: f64| (mm / 25.4 * f64::from(dpi) + 0.5) as u32;
        let (px1, px2, py1, py2) = (px(x1), px(x2), px(y1), px(y2));

        let gamma = fixed(s.gamma.unwrap_or(DEFAULT_GAMMA).clamp(GAMMA_RANGE.min, GAMMA_RANGE.max));
        let mut gamma_table = vec![0u8; 2048];
        image::fill_gamma_table(gamma, &mut gamma_table);

        let calibrate = match s.extra("calibrate") {
            None => Calibrate::Once,
            Some(OptionValue::Choice(c)) if c == "Once" => Calibrate::Once,
            Some(OptionValue::Choice(c)) if c == "Always" => Calibrate::Always,
            Some(OptionValue::Choice(c)) if c == "Never" => Calibrate::Never,
            Some(other) => return Err(invalid(format!("calibrate: invalid value {other:?}"))),
        };
        let int_extra = |name: &str, default: i32, max: i32| match s.extra(name) {
            None => Ok(default),
            Some(OptionValue::Int(v)) if (0..=max).contains(v) => Ok(*v),
            Some(other) => Err(invalid(format!("{name}: invalid value {other:?}"))),
        };
        let threshold_pct = int_extra("threshold", DEFAULT_THRESHOLD, 100)?;
        let threshold_curve = int_extra("threshold-curve", 0, 127)?;
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "C's unsigned = 2.55 * int")]
        let threshold = (2.55 * f64::from(threshold_pct)) as u32;
        let mut lut = [0u8; 256];
        if mode == Mode::Lineart {
            image::load_lut(&mut lut, 50, 205, threshold_curve, i32::try_from(threshold).unwrap_or(0) - 127);
        }

        let mut sp = ScanParam {
            mode,
            channels: 0,
            depth: 0,
            software_lineart: false,
            xdpi: dpi,
            ydpi: dpi,
            x: px1,
            y: py1,
            w: (px2 - px1).max(1),
            h: (py2 - py1).max(1),
            xs: 0,
            wx: 0,
            line_size: 0,
            image_size: 0,
            gamma_table,
            calibrate,
            lineart: Lineart { threshold, threshold_curve: threshold_curve.unsigned_abs(), lut, xdpi: dpi },
        };
        let scale = self.scale_for(dpi);
        self.check_param(&mut sp, scale);
        clamp2(&mut sp.x, &mut sp.w, 16, self.model.width, sp.xdpi);
        clamp2(&mut sp.y, &mut sp.h, 16, self.model.height, sp.ydpi);
        self.check_param(&mut sp, scale);
        sp.image_size = sp.line_size * u64::from(sp.h);
        if sp.software_lineart {
            sp.image_size /= 8;
        }
        Ok((sp, scale))
    }
}

fn parameters_of(sp: &ScanParam) -> Parameters {
    let channels = if sp.channels == 3 { Channels::Rgb } else { Channels::Gray };
    let w = sp.w as usize;
    Parameters {
        layout: PixelLayout { channels, depth: sp.depth },
        pixels_per_line: w,
        bytes_per_line: w * channels.count() * sp.depth as usize / 8,
        lines: sp.h as usize,
    }
}

/// The frame being read.
#[derive(Debug)]
struct Frame {
    param: ScanParam,
    cancel: CancelToken,
    /// Processed bytes not yet handed out.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Bytes produced so far, padding included.
    produced: u64,
    /// The device stopped early; the rest of the image is padded with white.
    underrun: bool,
}

/// An open pixma scanner.
#[derive(Debug)]
pub struct Pixma {
    info: DeviceInfo,
    caps: Capabilities,
    driver: Mp150,
    /// Source of the last completed scan, for calibrate = Once.
    last_source: Option<String>,
    frame: Option<Frame>,
}

impl Pixma {
    /// Opens the scanner behind `usb`.
    ///
    /// # Errors
    /// [`ErrorKind::NotFound`] if the device isn't a supported model.
    pub fn open(usb: Box<dyn UsbTransport>) -> Result<Self> {
        let ids = usb.ids().clone();
        let model = models::find(ids.vendor_id, ids.product_id).ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("{:04x}:{:04x} is not a supported pixma scanner", ids.vendor_id, ids.product_id),
            )
        })?;
        let info = DeviceInfo {
            name: device_name(ids.vendor_id, ids.product_id, ids.serial.as_deref()),
            vendor: "CANON".into(),
            model: model.name.into(),
        };
        log::info!(device = info.name.as_str(), model = model.name; "opened pixma scanner");
        Ok(Self {
            info,
            caps: capabilities(model),
            driver: Mp150::new(model, Io { usb }),
            last_source: None,
            frame: None,
        })
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver { model: self.driver.model, caps: &self.caps }
    }

    /// `pixma_calc_calibrate` for a flatbed scan.
    fn calibrate_flag(&self, sp: &ScanParam, source: &str) -> u8 {
        match sp.calibrate {
            Calibrate::Always => 1,
            Calibrate::Never => 0,
            Calibrate::Once => u8::from(self.last_source.as_deref() != Some(source)),
        }
    }

    /// The most recent front-panel button press seen while scanning, as
    /// `(button, target)`: button 1 starts a scan, 2 ends one.
    #[must_use]
    pub fn last_button(&self) -> Option<(u8, u8)> {
        self.driver.last_button.map(|b| (b.button, b.target))
    }

    /// Ends the frame after an error or cancellation.
    fn abort_frame(&mut self) {
        self.driver.finish_scan();
        self.frame = None;
    }
}

impl Device for Pixma {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        Ok(parameters_of(&self.resolver().resolve(settings)?.0))
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        if self.frame.is_some() {
            return Err(Error::new(ErrorKind::Busy, "a scan is already in progress"));
        }
        let (sp, scale) = self.resolver().resolve(settings)?;
        log::debug!(
            mode:? = sp.mode, dpi = sp.xdpi, x = sp.x, y = sp.y, w = sp.w, h = sp.h, xs = sp.xs, wx = sp.wx, scale;
            "starting scan"
        );
        self.driver.scale = scale;
        let calibrate = self.calibrate_flag(&sp, &settings.source);
        self.driver.scan(&sp, calibrate, cancel).map_err(|e| Error::from(e).context("starting scan"))?;
        let params = parameters_of(&sp);
        self.frame = Some(Frame {
            param: sp,
            cancel: cancel.clone(),
            pending: Vec::new(),
            pending_pos: 0,
            produced: 0,
            underrun: false,
        });
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(frame) = self.frame.as_mut() else { return Ok(0) };
        if frame.cancel.is_cancelled() {
            self.abort_frame();
            return Err(Error::cancelled());
        }
        let image_size = frame.param.image_size;
        let mut written = 0;
        if frame.underrun {
            let left = usize::try_from(image_size - frame.produced).unwrap_or(usize::MAX).min(buf.len());
            buf[..left].fill(0xff);
            frame.produced += left as u64;
            if left == 0 {
                self.frame = None;
            }
            return Ok(left);
        }
        while written < buf.len() {
            if frame.pending_pos == frame.pending.len() {
                frame.pending.clear();
                frame.pending_pos = 0;
                match self.driver.fill_buffer(&frame.param, &mut frame.pending, &frame.cancel) {
                    Ok(0) => {
                        // End of image: the session is over either way.
                        self.driver.finish_scan();
                        self.last_source = Some(SOURCE_FLATBED.into());
                        if frame.produced != image_size {
                            log::warn!(expected = image_size, received = frame.produced; "image size mismatch");
                        }
                        if frame.produced < image_size {
                            frame.underrun = true;
                            let left = usize::try_from(image_size - frame.produced)
                                .unwrap_or(usize::MAX)
                                .min(buf.len() - written);
                            buf[written..written + left].fill(0xff);
                            frame.produced += left as u64;
                            written += left;
                        } else {
                            self.frame = None;
                        }
                        return Ok(written);
                    }
                    Ok(n) => frame.produced += n as u64,
                    Err(e) => {
                        let cancelled = matches!(e, PixmaError::Cancelled) && frame.cancel.is_cancelled();
                        self.abort_frame();
                        return Err(if cancelled {
                            Error::cancelled()
                        } else {
                            Error::from(e).context("reading image")
                        });
                    }
                }
            }
            let n = (frame.pending.len() - frame.pending_pos).min(buf.len() - written);
            buf[written..written + n].copy_from_slice(&frame.pending[frame.pending_pos..frame.pending_pos + n]);
            frame.pending_pos += n;
            written += n;
        }
        Ok(written)
    }

    fn finish(&mut self) {
        if !self.driver.is_idle() {
            self.driver.finish_scan();
        }
        self.frame = None;
    }
}

impl Drop for Pixma {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lide400() -> &'static Model {
        models::find(0x04a9, 0x1912).unwrap()
    }

    fn settings(mode: &str, dpi: u32, area: Area) -> ScanSettings {
        ScanSettings { source: "Flatbed".into(), mode: mode.into(), resolution: dpi, area, gamma: None, extras: vec![] }
    }

    #[test]
    fn names_match_sane() {
        assert_eq!(device_name(0x04a9, 0x1912, Some("508624")), "pixma:04A91912_508624");
        assert_eq!(device_name(0x04a9, 0x1912, None), "pixma:04A91912");
    }

    #[test]
    fn lide400_offers_what_sane_does() {
        let caps = capabilities(lide400());
        let src = &caps.sources[0];
        let names: Vec<_> = src.modes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Color", "Gray", "48 bits color", "16 bits gray", "Lineart"]);
        assert_eq!(src.modes[0].resolutions, [75, 150, 300, 600, 1200, 2400, 4800]);
        assert!((src.width_mm - 216.069_33).abs() < 1e-4);
        assert!((src.height_mm - 297.010_66).abs() < 1e-4);
    }

    #[test]
    fn resolves_like_sane() {
        let model = lide400();
        let caps = capabilities(model);
        let r = Resolver { model, caps: &caps };
        // scanimage --mode Gray --resolution 150 -x 50 -y 50 → 295×295.
        let (sp, scale) =
            r.resolve(&settings("Gray", 150, Area { x: 0.0, y: 0.0, width: 50.0, height: 50.0 })).unwrap();
        assert_eq!((sp.w, sp.h, scale, sp.wx), (295, 295, 2, 608));
        // Lineart widths round up to whole bytes: 591 → 592.
        let (sp, _) = r.resolve(&settings("Lineart", 300, Area { x: 0.0, y: 0.0, width: 50.0, height: 50.0 })).unwrap();
        assert_eq!((sp.w, sp.h, sp.image_size), (592, 591, 592 * 591 / 8));
        // An offset origin needs a left crop to stay 32-pixel aligned.
        let (sp, _) =
            r.resolve(&settings("Color", 300, Area { x: 100.0, y: 150.0, width: 30.0, height: 30.0 })).unwrap();
        assert_eq!((sp.x, sp.xs, sp.w), (1181, 1181 % 32, 354));
        assert!(r.resolve(&settings("Color", 100, Area { x: 0.0, y: 0.0, width: 1.0, height: 1.0 })).is_err());
        assert!(r.resolve(&settings("Sepia", 75, Area { x: 0.0, y: 0.0, width: 1.0, height: 1.0 })).is_err());
    }

    #[test]
    fn tiny_areas_grow_to_the_minimum() {
        let model = lide400();
        let caps = capabilities(model);
        let r = Resolver { model, caps: &caps };
        let (sp, _) = r.resolve(&settings("Color", 75, Area { x: 300.0, y: 300.0, width: 0.0, height: 0.0 })).unwrap();
        assert_eq!((sp.w, sp.h), (16, 16));
        assert_eq!((sp.x, sp.y), (638 - 16, 877 - 16));
    }
}

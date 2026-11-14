//! The main window: title bar, settings sidebar, preview canvas and status bar.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, Corners, Entity, FocusHandle, FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathPromptOptions, PinchEvent, Pixels, RenderImage, ScrollEvent, SharedString, Subscription, Task,
    TextAlign, TextRun, Window, actions, canvas, div, fill, outline, prelude::*, px, relative,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IndexPath, Selectable as _, Sizable as _, TitleBar, h_flex, v_flex,
};
use lightfast::job::{Area, Estimate, Format, Job, Output, ScanMode, human_bytes, total_raw_bytes};
use lightfast::raster::{PixelFormat, RgbImage};
use lightfast::scanner::{Caps, Command, Event, GammaRange, Scanner, SourceCaps};

use super::glass::{self, ButtonRole, Control, ControlEvent, ControlSize, Fills, Material, Pane, Spec};
use super::menu;
use super::preview::{MAX_ZOOM, MIN_ZOOM, Preview};
use super::theme::CanvasColors;

actions!(lightfast, [ZoomIn, ZoomOut, ZoomToFit, SingleAreaMode, MultipleAreaMode, DeleteFrame, Quit, CheckForUpdates]);

/// Bed size used for the outline before a scanner is found (A4).
const DEFAULT_BED: (f64, f64) = (216.0, 297.0);
/// Initial resolution: the native maximum of common photo flatbeds.
const DEFAULT_DPI: u32 = 4800;
/// Resolution chosen when the scanner reports none at all.
const FALLBACK_DPI: u32 = 300;
/// Gap between the canvas edge and the fitted bed, so the bed outline and
/// corner handles never touch the panels.
const CANVAS_INSET: f32 = 24.0;
/// Zoom change per scrolled pixel with ⌘ held: `exp(rate * delta)`, so equal
/// scroll distances give equal zoom ratios in both directions.
const SCROLL_ZOOM_RATE: f64 = 0.01;
/// Side length of the selection's corner handles.
const HANDLE_SIZE: f32 = 8.0;
/// Gamma slider increment; finer than backends usually honour.
const GAMMA_STEP: f32 = 0.05;
/// Width of the settings sidebar's content.
const SIDEBAR_WIDTH: f32 = 260.0;
/// Height of the glass toolbar: its centre lines up with the traffic
/// lights, which sit `TRAFFIC_LIGHTS.1` down and are 14 pt tall.
const GLASS_TOOLBAR_HEIGHT: f32 = 2.0 * (glass::TRAFFIC_LIGHTS.1 + 7.0);
/// Strength of the sidebar colour laid over the sidebar material, darkening
/// it to Finder's depth while the desktop's colour still shows through.
const GLASS_SIDEBAR_SHADE: f32 = 0.45;
/// Width of the native zoom control in the glass toolbar.
const GLASS_ZOOM_WIDTH: f32 = 150.0;
/// Height and corner radius of the docked glass status bar.
const GLASS_STATUS_HEIGHT: f32 = 32.0;
/// Side of the square each frame's preview crop is fitted into.
const FRAME_THUMB: f32 = 56.0;
/// Text size of the frame numbers drawn on the canvas.
const FRAME_LABEL_SIZE: f32 = 11.0;
/// Opacity of the outlines of unselected frames, so the selected one stands out.
const FRAME_OUTLINE_OPACITY: f32 = 0.55;

/// Named scan sizes in millimetres (portrait, anchored at the bed origin).
const PRESETS: &[(&str, Option<(f64, f64)>)] = &[
    ("Full bed", None),
    ("A4 (210 × 297 mm)", Some((210.0, 297.0))),
    ("US Letter (8.5 × 11 in)", Some((215.9, 279.4))),
    ("A5 (148 × 210 mm)", Some((148.0, 210.0))),
    ("10 × 8 in", Some((203.2, 254.0))),
    ("7 × 5 in", Some((127.0, 177.8))),
    ("6 × 4 in", Some((101.6, 152.4))),
    ("Custom", None),
];
const CUSTOM_PRESET: usize = PRESETS.len() - 1;

/// Keys of the native controls used on glass; each names one control.
mod key {
    pub const RESCAN: &str = "rescan";
    pub const SOURCE: &str = "source";
    pub const SIZE: &str = "size";
    pub const DPI: &str = "dpi";
    pub const MODE: &str = "mode";
    pub const FORMAT: &str = "format";
    pub const GAMMA: &str = "gamma";
    pub const CHOOSE: &str = "choose";
    pub const PREVIEW: &str = "preview";
    pub const SCAN: &str = "scan";
    pub const CANCEL: &str = "cancel";
    pub const REVEAL: &str = "reveal";
    pub const ZOOM: &str = "zoom";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Discovering,
    Idle,
    Previewing,
    Scanning,
}

/// The root view: owns the scanner worker and all UI state.
pub struct AppView {
    focus: FocusHandle,
    scanner: Scanner,
    caps: Option<Caps>,
    activity: Activity,
    status: SharedString,
    error: Option<SharedString>,

    size_select: Entity<SelectState<Vec<SharedString>>>,
    source_select: Entity<SelectState<Vec<SharedString>>>,
    dpi_select: Entity<SelectState<Vec<SharedString>>>,
    mode_select: Entity<SelectState<Vec<SharedString>>>,
    /// Index into `PRESETS` of the chosen scan size.
    preset: usize,
    /// Index into `caps.sources`.
    source: usize,
    /// Index into the current source's modes.
    mode: usize,
    dpis: Vec<u32>,
    dpi: u32,
    /// Chosen gamma, when the source offers the option.
    gamma: Option<f64>,
    /// Rebuilt per source, since its range is fixed at creation.
    gamma_slider: Option<(Entity<SliderState>, Subscription)>,
    format: Format,
    save_dir: PathBuf,
    last_saved: Option<PathBuf>,

    preview: Preview,
    /// Full-bed preview image.
    bed_image: Option<Arc<RenderImage>>,
    /// Live thumbnail of the scan in progress.
    scan_image: Option<Arc<RenderImage>>,
    /// The bed area the scan in progress reads, which `scan_image` covers.
    scan_area: Option<Area>,
    /// Images replaced since the last frame; their GPU textures are freed on render.
    stale_images: Vec<Arc<RenderImage>>,

    started: Option<Instant>,
    /// Re-renders once a second while work runs, to tick the elapsed/ETA readout.
    ticker: Option<Task<()>>,
    progress: (usize, usize),
    scan_px: Option<(usize, usize)>,
}

impl AppView {
    /// Creates the view and starts looking for a scanner.
    ///
    /// # Panics
    /// If the scanner worker thread can't be spawned; the app is useless
    /// without it.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (scanner, events) = Scanner::spawn().expect("failed to spawn the scanner worker thread");
        scanner.send(Command::Discover);

        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_event(event, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let titles: Vec<SharedString> = PRESETS.iter().map(|(n, _)| SharedString::from(*n)).collect();
        let size_select = cx.new(|cx| SelectState::new(titles, Some(IndexPath::new(0)), window, cx));
        cx.subscribe_in(&size_select, window, |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            let SelectEvent::Confirm(Some(title)) = event else { return };
            if let Some(i) = PRESETS.iter().position(|(n, _)| *n == title.as_ref()) {
                this.apply_preset(i);
                cx.notify();
            }
        })
        .detach();

        let source_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        cx.subscribe_in(&source_select, window, |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
            let SelectEvent::Confirm(Some(title)) = event else { return };
            let found = this.caps.as_ref().and_then(|c| c.sources.iter().position(|s| source_title(s) == *title));
            if let Some(i) = found.filter(|&i| i != this.source) {
                this.apply_source(i, window, cx);
                cx.notify();
            }
        })
        .detach();

        let mode_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        cx.subscribe_in(&mode_select, window, |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            let SelectEvent::Confirm(Some(title)) = event else { return };
            if let Some(i) = this.source_caps().and_then(|s| s.modes.iter().position(|m| m.label == title.as_ref())) {
                this.mode = i;
                cx.notify();
            }
        })
        .detach();

        let dpi_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        cx.subscribe_in(&dpi_select, window, |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            let SelectEvent::Confirm(Some(title)) = event else { return };
            if let Some(dpi) = title.split_whitespace().next().and_then(|d| d.parse().ok()) {
                this.dpi = dpi;
                cx.notify();
            }
        })
        .detach();

        if glass::active(cx) {
            let controls = glass::control_events();
            cx.spawn_in(window, async move |this, cx| {
                while let Ok((key, event)) = controls.recv().await {
                    if this.update_in(cx, |this, window, cx| this.on_control(key, event, window, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let save_dir = dirs::picture_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));

        Self {
            focus,
            scanner,
            caps: None,
            activity: Activity::Discovering,
            status: "Looking for scanners…".into(),
            error: None,
            size_select,
            source_select,
            dpi_select,
            mode_select,
            preset: 0,
            source: 0,
            mode: 0,
            dpis: Vec::new(),
            dpi: DEFAULT_DPI,
            gamma: None,
            gamma_slider: None,
            format: Format::Tiff,
            save_dir,
            last_saved: None,
            preview: Preview::new(DEFAULT_BED.0, DEFAULT_BED.1),
            bed_image: None,
            scan_image: None,
            scan_area: None,
            stale_images: Vec::new(),
            started: None,
            ticker: None,
            progress: (0, 0),
            scan_px: None,
        }
    }

    fn on_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Discovering => {
                self.activity = Activity::Discovering;
                self.status = "Looking for scanners…".into();
            }
            Event::Ready(caps) => {
                self.status = format!("{} {} ready", caps.device.vendor, caps.device.model).into();
                let titles: Vec<SharedString> = caps.sources.iter().map(source_title).collect();
                self.caps = Some(caps);
                set_select(&self.source_select, titles, 0, window, cx);
                self.mode = 0;
                self.apply_source(0, window, cx);
                self.activity = Activity::Idle;
            }
            Event::NoScanner => {
                self.caps = None;
                self.activity = Activity::Idle;
                self.status = "No scanner found. Connect one and press Rescan.".into();
            }
            Event::Started { width, height, .. } => {
                self.scan_px = Some((width, height));
                self.progress = (0, height);
            }
            Event::Progress { rows, total } => self.progress = (rows, total),
            Event::Thumb(img) => self.set_image(&img, self.activity == Activity::Previewing),
            Event::PreviewDone(img) => {
                self.set_image(&img, true);
                self.finish("Preview ready. Drag on the image to choose the scan area (⌥-drag to move it).".into());
            }
            Event::Saved(paths) => {
                let msg = match paths.as_slice() {
                    [one] => format!("Saved {}", one.display()),
                    many => format!("Saved {} files to {}", many.len(), self.save_dir.display()),
                };
                self.last_saved = paths.into_iter().next();
                self.finish(msg.into());
            }
            Event::Cancelled => self.finish("Cancelled.".into()),
            Event::Failed(msg) => {
                self.error = Some(msg.into());
                self.finish("Failed.".into());
            }
        }
        cx.notify();
    }

    /// Handles an action on one of the native controls used on glass.
    fn on_control(&mut self, key: &'static str, event: ControlEvent, window: &mut Window, cx: &mut Context<Self>) {
        match (key, event) {
            (key::SIZE, ControlEvent::Selected(i)) if i < PRESETS.len() => self.apply_preset(i),
            (key::SOURCE, ControlEvent::Selected(i)) if i != self.source => self.apply_source(i, window, cx),
            (key::DPI, ControlEvent::Selected(i)) => {
                if let Some(&dpi) = self.dpis.get(i) {
                    self.dpi = dpi;
                }
            }
            (key::MODE, ControlEvent::Selected(i)) if self.source_caps().is_some_and(|s| i < s.modes.len()) => {
                self.mode = i;
            }
            (key::FORMAT, ControlEvent::Selected(i)) => {
                if let Some(&f) = Format::ALL.get(i) {
                    self.format = f;
                }
            }
            (key::GAMMA, ControlEvent::Value(v)) => self.gamma = Some((v * 100.0).round() / 100.0),
            (key::RESCAN, ControlEvent::Clicked) if !self.busy() => self.scanner.send(Command::Discover),
            (key::CHOOSE, ControlEvent::Clicked) => Self::choose_dir(window, cx),
            (key::PREVIEW, ControlEvent::Clicked) => self.start_preview(cx),
            (key::SCAN, ControlEvent::Clicked) => self.start_scan(cx),
            (key::CANCEL, ControlEvent::Clicked) => self.scanner.cancel(),
            (key::REVEAL, ControlEvent::Clicked) => {
                if let Some(path) = &self.last_saved {
                    cx.reveal_path(path);
                }
            }
            (key::ZOOM, ControlEvent::Selected(0)) => self.preview.zoom_step(false),
            (key::ZOOM, ControlEvent::Selected(1)) => self.preview.reset_zoom(),
            (key::ZOOM, ControlEvent::Selected(2)) => self.preview.zoom_step(true),
            _ => return,
        }
        cx.notify();
    }

    fn finish(&mut self, status: SharedString) {
        if self.activity == Activity::Scanning {
            self.scan_area = None;
            if let Some(old) = self.scan_image.take() {
                self.stale_images.push(old);
            }
        }
        self.activity = Activity::Idle;
        self.started = None;
        self.ticker = None;
        self.status = status;
    }

    fn set_image(&mut self, img: &RgbImage, bed: bool) {
        let image = to_render_image(img);
        let slot = if bed { &mut self.bed_image } else { &mut self.scan_image };
        if let Some(old) = slot.replace(image) {
            self.stale_images.push(old);
        }
    }

    /// The selected source, once a scanner is ready.
    fn source_caps(&self) -> Option<&SourceCaps> {
        self.caps.as_ref()?.sources.get(self.source)
    }

    /// The selected mode, once a scanner is ready.
    fn scan_mode(&self) -> Option<&ScanMode> {
        self.source_caps()?.modes.get(self.mode)
    }

    /// Switches to source `index`: its bed, resolutions, modes and gamma.
    ///
    /// Keeps the current mode's pixel format and resolution when the new
    /// source offers them.
    fn apply_source(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let pixel = self.scan_mode().map_or(PixelFormat::Rgb8, |m| m.pixel);
        let changed = index != self.source;
        let Some(src) = self.caps.as_ref().and_then(|c| c.sources.get(index)).cloned() else { return };
        self.source = index;
        if changed && let Some(old) = self.bed_image.take() {
            // The old preview shows a different bed.
            self.stale_images.push(old);
        }

        self.preview.set_bed(src.bed_w, src.bed_h);
        if self.preview.is_multi() {
            // Frames keep their places on the new bed.
            self.select_custom(window, cx);
        } else {
            self.apply_preset(0);
            set_select(&self.size_select, PRESETS.iter().map(|(n, _)| SharedString::from(*n)).collect(), 0, window, cx);
        }

        self.dpis.clone_from(&src.dpis);
        if !self.dpis.contains(&self.dpi) {
            self.dpi = if self.dpis.contains(&DEFAULT_DPI) {
                DEFAULT_DPI
            } else {
                self.dpis.last().copied().unwrap_or(FALLBACK_DPI)
            };
        }
        let titles = self.dpis.iter().map(|d| format!("{d} DPI").into()).collect();
        let selected = self.dpis.iter().position(|&d| d == self.dpi).unwrap_or(0);
        set_select(&self.dpi_select, titles, selected, window, cx);

        let keep = src.mode_for(pixel);
        self.mode = src.modes.iter().position(|m| m == keep).unwrap_or(0);
        let titles = src.modes.iter().map(|m| SharedString::from(m.label.clone())).collect();
        set_select(&self.mode_select, titles, self.mode, window, cx);

        self.gamma = src.gamma.map(|g| g.default);
        self.gamma_slider = src.gamma.map(|g| Self::gamma_slider(g, cx));
    }

    fn gamma_slider(range: GammaRange, cx: &mut Context<Self>) -> (Entity<SliderState>, Subscription) {
        let state = cx.new(|_| {
            SliderState::new()
                .min(to_f32(range.min))
                .max(to_f32(range.max))
                .step(GAMMA_STEP)
                .default_value(to_f32(range.default))
        });
        let sub = cx.subscribe(&state, |this, _, event: &SliderEvent, cx| {
            let SliderEvent::Change(value) = event else { return };
            this.gamma = Some((f64::from(value.start()) * 100.0).round() / 100.0);
            cx.notify();
        });
        (state, sub)
    }

    /// Resizes the selected frame to preset `index`.
    ///
    /// In single mode sizes are anchored at the bed origin; in multiple mode
    /// the frame keeps its top-left corner.
    fn apply_preset(&mut self, index: usize) {
        let Some(sel) = self.preview.selection() else { return };
        self.preset = index;
        let (bed_w, bed_h) = (self.preview.bed_w, self.preview.bed_h);
        let (x, y) = if self.preview.is_multi() { (sel.x, sel.y) } else { (0.0, 0.0) };
        match PRESETS[index].1 {
            Some((w, h)) => self.preview.set_selection(Area::new(x, y, w, h)),
            None if index == 0 => self.preview.set_selection(Area::new(0.0, 0.0, bed_w, bed_h)),
            None => {} // Custom keeps the current selection.
        }
    }

    /// Switches between single-area and multiple-area scanning.
    fn set_multi(&mut self, multi: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || multi == self.preview.is_multi() {
            return;
        }
        self.preview.set_multi(multi);
        self.select_custom(window, cx);
        menu::set(cx, multi);
        cx.notify();
    }

    /// Deletes the selected frame, in multiple mode.
    fn delete_frame(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.preview.selected().filter(|_| !self.busy()) {
            self.preview.remove(i);
            cx.notify();
        }
    }

    fn busy(&self) -> bool {
        matches!(self.activity, Activity::Previewing | Activity::Scanning | Activity::Discovering)
    }

    /// The pixel format the chosen mode produces.
    fn pixel(&self) -> PixelFormat {
        self.scan_mode().map_or(PixelFormat::Rgb8, |m| m.pixel)
    }

    fn begin(&mut self, activity: Activity, cx: &mut Context<Self>) {
        self.error = None;
        self.activity = activity;
        self.started = Some(Instant::now());
        self.progress = (0, 0);
        self.ticker = Some(cx.spawn(async |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    fn start_preview(&mut self, cx: &mut Context<Self>) {
        let source = self.source_caps().map(|s| s.name.clone());
        self.begin(Activity::Previewing, cx);
        self.status = "Previewing…".into();
        self.scanner.send(Command::Preview { source, gamma: self.gamma });
        cx.notify();
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        let (Some(src), Some(mode)) = (self.source_caps(), self.scan_mode()) else { return };
        let (source, mode) = (Some(src.name.clone()), mode.clone());
        let frames = self.preview.frames();
        if frames.is_empty() {
            return;
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let ext = self.format.extension();
        let outputs: Vec<Output> = if self.preview.is_multi() {
            let file = |i: usize| format!("scan-{stamp}-{:02}.{ext}", i + 1);
            frames.iter().enumerate().map(|(i, &area)| Output { area, dest: self.save_dir.join(file(i)) }).collect()
        } else {
            vec![Output { area: frames[0], dest: self.save_dir.join(format!("scan-{stamp}.{ext}")) }]
        };
        let job = Job { source, mode, dpi: self.dpi, gamma: self.gamma, format: self.format, outputs };
        self.begin(Activity::Scanning, cx);
        self.scan_px = None;
        self.scan_area = Some(job.scan_area());
        self.status = match job.outputs.len() {
            1 => format!("Scanning {} at {} DPI…", job.mode, self.dpi),
            n => format!("Scanning {n} frames in {} at {} DPI…", job.mode, self.dpi),
        }
        .into();
        self.scanner.send(Command::Scan(job));
        cx.notify();
    }

    fn choose_dir(window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Save scans here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(dir) = paths.pop()
            {
                // Fails only if the window closed while the dialog was open.
                let _ = this.update(cx, |this, cx| {
                    this.save_dir = dir;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn select_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preset = CUSTOM_PRESET;
        self.size_select.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::new(CUSTOM_PRESET)), window, cx);
        });
    }

    fn progress_text(&self) -> Option<String> {
        let started = self.started?;
        let (rows, total) = self.progress;
        let elapsed = started.elapsed();
        let mut s = format!("{} elapsed", fmt_duration(elapsed));
        if let Some(remaining) = time_left(elapsed, rows, total) {
            let _ = write!(s, " · ~{} left", fmt_duration(remaining));
        }
        if let Some((w, h)) = self.scan_px.filter(|_| self.activity == Activity::Scanning) {
            let _ = write!(s, " · {w} × {h} px");
        }
        Some(s)
    }

    fn scanner_name(&self) -> SharedString {
        match (&self.caps, self.activity) {
            (_, Activity::Discovering) => "Searching…".into(),
            (Some(c), _) => format!("{} {}", c.device.vendor, c.device.model).into(),
            (None, _) => "No scanner found".into(),
        }
    }

    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        TitleBar::new().child(
            h_flex()
                .flex_1()
                .pr_2()
                .gap_2()
                .text_sm()
                .child(div().font_weight(FontWeight::MEDIUM).child("Lightfast"))
                .child(div().text_color(theme.muted_foreground).child(self.scanner_name())),
        )
    }

    /// The toolbar beside the floating sidebar: Tahoe's title and subtitle
    /// on the leading edge, the zoom controls on a capsule at the trailing one.
    fn render_glass_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        transparent_title_bar(cx).h(px(GLASS_TOOLBAR_HEIGHT)).pl(px(glass::INSET * 2.0)).pr(px(glass::INSET)).child(
            h_flex()
                .flex_1()
                .gap_2()
                .justify_between()
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Lightfast"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(self.scanner_name()),
                        ),
                )
                .child(
                    glass::control(
                        key::ZOOM,
                        Spec {
                            control: Control::Segmented {
                                labels: vec![
                                    "−".into(),
                                    format!("{:.0}%", self.preview.zoom() * 100.0).into(),
                                    "+".into(),
                                ],
                                selected: None,
                            },
                            enabled: true,
                            size: ControlSize::Large,
                        },
                    )
                    .w(px(GLASS_ZOOM_WIDTH)),
                ),
        )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, bg, fg) = (theme.sidebar_border, theme.sidebar, theme.sidebar_foreground);
        self.render_sidebar_body(cx)
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .px_3()
            .py_3()
            .border_r_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
    }

    /// The sidebar on the system's frosted sidebar material, edge to edge as
    /// in Finder, its header holding the traffic lights.
    fn render_glass_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (fg, shade) = (cx.theme().sidebar_foreground, cx.theme().sidebar.opacity(GLASS_SIDEBAR_SHADE));
        v_flex()
            .relative()
            .flex_none()
            .h_full()
            .w(px(SIDEBAR_WIDTH))
            .text_color(fg)
            // Painted over the native material, which sits beneath GPUI.
            .bg(shade)
            .child(glass::pane("sidebar", Pane { radius: px(0.0), material: Material::Sidebar }))
            .child(transparent_title_bar(cx).h(px(GLASS_TOOLBAR_HEIGHT)))
            .child(self.render_sidebar_body(cx).flex_1().min_h_0().px_3().pb_3())
    }

    /// The settings sections and actions, shared by both sidebar styles.
    fn render_sidebar_body(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        v_flex()
            .gap_4()
            .child(self.render_scanner_section(cx))
            .children(self.render_source_section(cx))
            .child(self.render_size_section(cx))
            .child(self.render_resolution_section(cx))
            .child(self.render_mode_section(cx))
            .child(self.render_format_section(cx))
            .children(self.render_gamma_section(cx))
            .child(self.render_save_section(cx))
            .child(self.render_estimate(cx))
            .child(div().flex_1())
            .child(self.render_actions(cx))
    }

    /// A labelled group of sidebar controls.
    fn section(title: &'static str, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .gap_1p5()
            .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(title))
    }

    fn render_scanner_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::section("Scanner", cx).child(
            h_flex()
                .gap_2()
                .justify_between()
                .child(div().text_sm().min_w_0().overflow_hidden().text_ellipsis().child(self.scanner_name()))
                .child(if glass::active(cx) {
                    native_button(key::RESCAN, "Rescan", ButtonRole::Normal, ControlSize::Small, !self.busy())
                        .w(px(76.0))
                        .into_any_element()
                } else {
                    Button::new("rescan")
                        .label("Rescan")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, _| this.scanner.send(Command::Discover)))
                        .disabled(self.busy())
                        .into_any_element()
                }),
        )
    }

    fn render_size_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sel = self.preview.selection();
        let enabled = !self.busy() && sel.is_some();
        let picker = if glass::active(cx) {
            let titles = PRESETS.iter().map(|(n, _)| SharedString::from(*n)).collect();
            native_popup(key::SIZE, titles, Some(self.preset), enabled).into_any_element()
        } else {
            Select::new(&self.size_select).small().disabled(!enabled).into_any_element()
        };
        let readout = sel.map_or_else(
            || "No frame selected".to_owned(),
            |sel| format!("{:.1} × {:.1} mm at ({:.1}, {:.1})", sel.w, sel.h, sel.x, sel.y),
        );
        Self::section("Size", cx).child(picker).child(
            div()
                .text_xs()
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(cx.theme().muted_foreground)
                .child(readout),
        )
    }

    fn render_resolution_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = !self.busy() && !self.dpis.is_empty();
        Self::section("Resolution", cx).child(if glass::active(cx) {
            let titles = self.dpis.iter().map(|d| format!("{d} DPI").into()).collect();
            let selected = self.dpis.iter().position(|&d| d == self.dpi);
            native_popup(key::DPI, titles, selected, enabled).into_any_element()
        } else {
            Select::new(&self.dpi_select).small().placeholder("—").disabled(!enabled).into_any_element()
        })
    }

    /// Source picker, shown only when the scanner has more than one.
    fn render_source_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let many = self.caps.as_ref().is_some_and(|c| c.sources.len() > 1);
        many.then(|| {
            Self::section("Source", cx).child(if glass::active(cx) {
                let titles = self.caps.iter().flat_map(|c| c.sources.iter().map(source_title)).collect();
                native_popup(key::SOURCE, titles, Some(self.source), !self.busy()).into_any_element()
            } else {
                Select::new(&self.source_select).small().disabled(self.busy()).into_any_element()
            })
        })
    }

    fn render_mode_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = !self.busy() && self.caps.is_some();
        Self::section("Mode", cx).child(if glass::active(cx) {
            let labels =
                self.source_caps().map(|s| s.modes.iter().map(|m| SharedString::from(m.label.clone())).collect());
            native_popup(key::MODE, labels.unwrap_or_default(), Some(self.mode), enabled).into_any_element()
        } else {
            Select::new(&self.mode_select).small().placeholder("—").disabled(!enabled).into_any_element()
        })
    }

    /// Gamma slider, shown only when the source offers the option.
    fn render_gamma_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (state, _) = self.gamma_slider.as_ref()?;
        let gamma = self.gamma?;
        let slider = match self.source_caps().and_then(|s| s.gamma).filter(|_| glass::active(cx)) {
            Some(range) => glass::control(
                key::GAMMA,
                Spec {
                    control: Control::Slider { min: range.min, max: range.max, value: gamma },
                    enabled: !self.busy(),
                    size: ControlSize::Regular,
                },
            )
            .into_any_element(),
            None => Slider::new(state).disabled(self.busy()).into_any_element(),
        };
        Some(
            Self::section("Gamma", cx).child(
                h_flex().gap_2().child(div().flex_1().child(slider)).child(
                    div()
                        .w(px(32.0))
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{gamma:.2}")),
                ),
            ),
        )
    }

    fn render_format_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if glass::active(cx) {
            let labels = Format::ALL.iter().map(|f| f.to_string().into()).collect();
            let selected = Format::ALL.iter().position(|&f| f == self.format);
            let spec = Spec {
                control: Control::Segmented { labels, selected },
                enabled: !self.busy(),
                size: ControlSize::Regular,
            };
            return Self::section("Format", cx).child(glass::control(key::FORMAT, spec));
        }
        Self::section("Format", cx).child(
            ButtonGroup::new("format")
                .outline()
                .small()
                .disabled(self.busy())
                .children(Format::ALL.iter().map(|&f| {
                    Button::new(SharedString::from(f.to_string()))
                        .label(f.to_string())
                        .selected(f == self.format)
                        .flex_1()
                }))
                .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                    if let Some(&f) = clicks.first().and_then(|&i| Format::ALL.get(i)) {
                        this.format = f;
                        cx.notify();
                    }
                })),
        )
    }

    fn render_save_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::section("Save to", cx).child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(self.save_dir.display().to_string()),
                )
                .child(if glass::active(cx) {
                    native_button(key::CHOOSE, "Choose…", ButtonRole::Normal, ControlSize::Small, !self.busy())
                        .w(px(86.0))
                        .into_any_element()
                } else {
                    Button::new("choose-dir")
                        .label("Choose…")
                        .xsmall()
                        .ghost()
                        .disabled(self.busy())
                        .on_click(cx.listener(|_, _, window, cx| Self::choose_dir(window, cx)))
                        .into_any_element()
                }),
        )
    }

    /// Output size estimate, plus why the chosen format can't hold it, if so.
    fn render_estimate(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (frames, pixel) = (self.preview.frames(), self.pixel());
        let summary = match (self.preview.is_multi(), frames) {
            (true, []) => "No frames".to_owned(),
            (true, frames) => format!(
                "{} frame{} · ~{} raw",
                frames.len(),
                if frames.len() == 1 { "" } else { "s" },
                human_bytes(total_raw_bytes(frames, self.dpi, pixel))
            ),
            (false, frames) => frames.first().map_or_default(|&a| Estimate::new(a, self.dpi, pixel).to_string()),
        };
        // On glass a translucent well lets the material read through.
        let (border, bg) = if glass::active(cx) {
            let fills = Fills::new(theme);
            (fills.line, fills.hover)
        } else {
            (theme.border, theme.muted)
        };
        v_flex()
            .gap_1()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(border)
            .bg(bg)
            .child(div().text_xs().font_family(theme.mono_font_family.clone()).child(summary))
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy();
        let can_scan = self.caps.is_some() && !busy && !self.preview.frames().is_empty();
        let can_preview = self.caps.is_some() && !busy && !self.source_caps().is_some_and(|s| s.feeder);
        let running = matches!(self.activity, Activity::Scanning | Activity::Previewing);
        if glass::active(cx) {
            let large = ControlSize::Large;
            return v_flex()
                .gap_2()
                .child(native_button(key::PREVIEW, "Preview", ButtonRole::Normal, large, can_preview))
                .child(if running {
                    native_button(key::CANCEL, "Cancel", ButtonRole::Destructive, large, true)
                } else {
                    native_button(key::SCAN, "Scan", ButtonRole::Default, large, can_scan)
                });
        }
        v_flex()
            .gap_2()
            .child(
                Button::new("preview")
                    .label("Preview")
                    .outline()
                    .disabled(!can_preview)
                    .on_click(cx.listener(|this, _, _, cx| this.start_preview(cx))),
            )
            .child(if running {
                Button::new("cancel")
                    .label("Cancel")
                    .danger()
                    .on_click(cx.listener(|this, _, _, _| this.scanner.cancel()))
            } else {
                Button::new("scan")
                    .label("Scan")
                    .primary()
                    .disabled(!can_scan)
                    .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx)))
            })
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (rows, total) = self.progress;
        let running = matches!(self.activity, Activity::Scanning | Activity::Previewing);
        let fraction = progress_fraction(rows, total);

        v_flex()
            .flex_none()
            .children(self.error.clone().map(|e| {
                div()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(theme.danger)
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.danger.opacity(0.08))
                    .child(e)
            }))
            // Thin progress line along the top edge of the status bar.
            .child(
                div()
                    .h(px(2.0))
                    .w_full()
                    .bg(theme.status_bar_border)
                    .when(running, |el| el.child(div().h_full().w(relative(fraction)).bg(theme.primary))),
            )
            .child(
                h_flex()
                    .h(px(28.0))
                    .px_3()
                    .gap_3()
                    .text_xs()
                    .bg(theme.status_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(self.status.clone()),
                    )
                    .children(self.progress_text().map(|t| {
                        div().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(t)
                    }))
                    .children(self.reveal_button())
                    .child(div().w(px(1.0)).h(px(14.0)).bg(theme.border))
                    .child(h_flex().gap_0p5().children(self.zoom_buttons(cx))),
            )
    }

    /// The status bar docked below the canvas on a glass bar, with any error
    /// just above it. Zoom lives in the toolbar instead.
    fn render_glass_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (danger, primary, muted, mono) =
            (theme.danger, theme.primary, theme.muted_foreground, theme.mono_font_family.clone());
        let track = Fills::new(theme).active;
        let (rows, total) = self.progress;
        let running = matches!(self.activity, Activity::Scanning | Activity::Previewing);
        let radius = px(GLASS_STATUS_HEIGHT / 2.0);

        v_flex()
            .flex_none()
            .px(px(glass::INSET))
            .pb(px(glass::INSET))
            .gap_1p5()
            .children(self.error.clone().map(|e| {
                div().px_3().py_1p5().rounded(radius).text_sm().text_color(danger).bg(danger.opacity(0.12)).child(e)
            }))
            .child(
                h_flex()
                    .relative()
                    .h(px(GLASS_STATUS_HEIGHT))
                    .pl_4()
                    .pr_2()
                    .gap_3()
                    .rounded(radius)
                    .text_xs()
                    .child(glass::pane("status", Pane { radius, material: Material::Glass }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(self.status.clone()),
                    )
                    // A Finder-style progress pill in place of the edge line.
                    .when(running, |el| {
                        el.child(div().flex_none().w(px(120.0)).h(px(4.0)).rounded_full().bg(track).child(
                            div().h_full().rounded_full().w(relative(progress_fraction(rows, total))).bg(primary),
                        ))
                    })
                    .children(self.progress_text().map(|t| div().font_family(mono).text_color(muted).child(t)))
                    .when(self.last_saved.is_some() && !running, |el| {
                        el.child(
                            native_button(key::REVEAL, "Show in Finder", ButtonRole::Normal, ControlSize::Small, true)
                                .w(px(118.0)),
                        )
                    }),
            )
    }

    /// "Show in Finder" for the last saved scan, once nothing is running.
    fn reveal_button(&self) -> Option<Button> {
        let running = matches!(self.activity, Activity::Scanning | Activity::Previewing);
        let path = self.last_saved.clone().filter(|_| !running)?;
        Some(
            Button::new("reveal")
                .label("Show in Finder")
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx: &mut App| cx.reveal_path(&path)),
        )
    }

    /// Zoom out, the current zoom (click to fit), zoom in and fit.
    fn zoom_buttons(&self, cx: &mut Context<Self>) -> Vec<Button> {
        let zoom = self.preview.zoom();
        vec![
            Button::new("zoom-out")
                .label("−")
                .xsmall()
                .ghost()
                .disabled(zoom <= MIN_ZOOM)
                .on_click(cx.listener(|this, _, _, cx| this.zoom_step(false, cx))),
            Button::new("zoom-fit")
                .label(format!("{:.0}%", zoom * 100.0))
                .xsmall()
                .ghost()
                .w(px(48.0))
                .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx))),
            Button::new("zoom-in")
                .label("+")
                .xsmall()
                .ghost()
                .disabled(zoom >= MAX_ZOOM)
                .on_click(cx.listener(|this, _, _, cx| this.zoom_step(true, cx))),
            Button::new("fit")
                .label("Fit")
                .xsmall()
                .ghost()
                .disabled(zoom <= MIN_ZOOM)
                .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx))),
        ]
    }

    fn zoom_step(&mut self, zoom_in: bool, cx: &mut Context<Self>) {
        self.preview.zoom_step(zoom_in);
        cx.notify();
    }

    fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.preview.reset_zoom();
        cx.notify();
    }

    /// The frames panel on the right edge, in multiple mode.
    fn render_frames_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, bg, fg) = (theme.sidebar_border, theme.sidebar, theme.sidebar_foreground);
        self.render_frames_body(cx)
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .px_3()
            .py_3()
            .border_l_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
    }

    /// The frames panel on the sidebar material, edge to edge like the sidebar.
    fn render_glass_frames_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (fg, shade) = (cx.theme().sidebar_foreground, cx.theme().sidebar.opacity(GLASS_SIDEBAR_SHADE));
        v_flex()
            .relative()
            .flex_none()
            .h_full()
            .w(px(SIDEBAR_WIDTH))
            .text_color(fg)
            .bg(shade)
            .child(glass::pane("frames", Pane { radius: px(0.0), material: Material::Sidebar }))
            .child(transparent_title_bar(cx).h(px(GLASS_TOOLBAR_HEIGHT)))
            .child(self.render_frames_body(cx).flex_1().min_h_0().px_3().pb_3())
    }

    /// The frame list with each frame's preview crop, shared by both panel styles.
    fn render_frames_body(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, mono, radius) = (theme.muted_foreground, theme.mono_font_family.clone(), theme.radius);
        let fills = Fills::new(theme);
        let colors = CanvasColors::new(theme);
        let (busy, dpi, pixel) = (self.busy(), self.dpi, self.pixel());
        let frames = self.preview.frames().to_vec();
        let selected = self.preview.selected();
        // While scanning, crops come from the scan as it arrives.
        let source = self.scan_image.clone().zip(self.scan_area).or_else(|| {
            let bed = Area::new(0.0, 0.0, self.preview.bed_w, self.preview.bed_h);
            self.bed_image.clone().map(|img| (img, bed))
        });

        let header = h_flex()
            .justify_between()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(muted)
                    .child(format!("Frames · {}", frames.len())),
            )
            .child(
                Button::new("clear-frames")
                    .label("Clear")
                    .xsmall()
                    .ghost()
                    .disabled(busy || frames.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.preview.clear();
                        cx.notify();
                    })),
            );

        let rows = frames.iter().enumerate().map(|(i, &area)| {
            let est = Estimate::new(area, dpi, pixel);
            let source = source.clone();
            div()
                .id(("frame", i))
                .flex()
                .items_center()
                .gap_2()
                .p_1p5()
                .rounded(radius)
                .cursor_pointer()
                .when(selected == Some(i), |el| el.bg(fills.active))
                .when(selected != Some(i), |el| el.hover(|el| el.bg(fills.hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.preview.select(Some(i));
                    cx.notify();
                }))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, (), window, _| {
                            paint_frame_thumb(bounds, area, source, colors, window);
                        },
                    )
                    .size(px(FRAME_THUMB))
                    .flex_none(),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(format!("Frame {}", i + 1)))
                        .child(
                            div()
                                .text_xs()
                                .font_family(mono.clone())
                                .text_color(muted)
                                .child(format!("{:.1} × {:.1} mm", area.w, area.h)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_family(mono.clone())
                                .text_color(muted)
                                .child(format!("{} × {} px", est.width, est.height)),
                        ),
                )
                .child(Button::new(("remove-frame", i)).label("×").xsmall().ghost().disabled(busy).on_click(
                    cx.listener(move |this, _, _, cx| {
                        // The row underneath would otherwise select a neighbour.
                        cx.stop_propagation();
                        this.preview.remove(i);
                        cx.notify();
                    }),
                ))
        });

        let list = if frames.is_empty() {
            div()
                .text_sm()
                .text_color(muted)
                .child("Drag on the bed to add a frame. ⌥-drag moves one; ⌫ deletes the selected frame.")
        } else {
            v_flex().gap_1().children(rows)
        };
        v_flex().gap_3().child(header).child(div().id("frame-list").flex_1().min_h_0().overflow_y_scroll().child(list))
    }

    fn render_canvas(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = CanvasColors::new(cx.theme());
        // On glass the native backdrop already paints this colour.
        let on_glass = glass::active(cx);
        let preview = self.preview.clone();
        let bed_image = self.bed_image.clone();
        let scan = self.scan_image.clone().zip(self.scan_area);
        let entity = cx.entity().downgrade();

        div()
            .id("canvas")
            .flex_1()
            .size_full()
            .min_h_0()
            .when(!on_glass, |el| el.bg(colors.backdrop))
            .cursor_crosshair()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if !this.busy() {
                        this.preview.mouse_down(e.position, e.modifiers.alt);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                if this.preview.is_dragging() && this.preview.mouse_move(e.position) {
                    this.select_custom(window, cx);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.preview.mouse_up();
                    cx.notify();
                }),
            )
            // ⌘+scroll zooms around the cursor; plain scroll pans.
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let delta = e.delta.pixel_delta(window.line_height());
                if e.modifiers.platform {
                    this.preview.zoom_at((f64::from(delta.y) * SCROLL_ZOOM_RATE).exp(), e.position);
                } else {
                    this.preview.pan_by(delta);
                }
                cx.notify();
            }))
            .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| {
                this.preview.zoom_at(1.0 + f64::from(e.delta), e.position);
                cx.notify();
            }))
            // Releasing outside the canvas must end the drag too.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.preview.mouse_up();
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        // Inset so the fitted bed doesn't touch the edges, and
                        // remember the layout for mouse mapping.
                        let viewport = bounds.dilate(px(-CANVAS_INSET));
                        let mut p = preview;
                        p.layout(viewport);
                        if let Some(view) = entity.upgrade() {
                            view.update(cx, |this, _| this.preview.layout(viewport));
                        }
                        (bounds, p)
                    },
                    move |_, (bounds, p), window, cx| {
                        // A zoomed bed must not draw over the sidebar or status bar.
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            paint_canvas(&p, bed_image.as_ref(), scan, colors, window, cx);
                        });
                    },
                )
                .size_full(),
            )
    }
}

fn paint_canvas(
    p: &Preview,
    bed_image: Option<&Arc<RenderImage>>,
    scan: Option<(Arc<RenderImage>, Area)>,
    colors: CanvasColors,
    window: &mut Window,
    cx: &mut App,
) {
    let bed = p.bed_rect;
    let scan = scan.map(|(img, area)| (img, p.to_screen(area)));
    // The bed as scanned so far, clipped to `clip`.
    let paint_bed = |clip: Bounds<Pixels>, window: &mut Window| {
        window.paint_quad(fill(clip, colors.bed));
        if let Some(img) = bed_image {
            let _ = window.paint_image(clip, bed, Corners::default(), Arc::clone(img), 0, false);
        }
        if let Some((img, rect)) = &scan {
            let _ = window.paint_image(clip, *rect, Corners::default(), Arc::clone(img), 0, false);
        }
    };
    paint_bed(bed, window);

    let frames: Vec<Bounds<Pixels>> = p.frames().iter().map(|&f| p.to_screen(f)).collect();
    if p.is_multi() {
        // Dim the bed, then repaint each frame undimmed.
        if !frames.is_empty() {
            window.paint_quad(fill(bed, colors.dim));
            for &f in &frames {
                paint_bed(f.intersect(&bed), window);
            }
        }
    } else if let Some(&sel) = frames.first() {
        // Dim everything outside the selection.
        let (bx0, by0, bx1, by1) = (bed.left(), bed.top(), bed.right(), bed.bottom());
        let (sx0, sy0, sx1, sy1) = (sel.left(), sel.top(), sel.right(), sel.bottom());
        for r in [
            Bounds::from_corners(gpui::point(bx0, by0), gpui::point(bx1, sy0)),
            Bounds::from_corners(gpui::point(bx0, sy1), gpui::point(bx1, by1)),
            Bounds::from_corners(gpui::point(bx0, sy0), gpui::point(sx0, sy1)),
            Bounds::from_corners(gpui::point(sx1, sy0), gpui::point(bx1, sy1)),
        ] {
            if r.size.width > px(0.0) && r.size.height > px(0.0) {
                window.paint_quad(fill(r, colors.dim));
            }
        }
    }
    window.paint_quad(outline(bed, colors.border, gpui::BorderStyle::Solid));

    let selected = p.selected();
    for (i, &f) in frames.iter().enumerate() {
        let accent = if selected == Some(i) { colors.accent } else { colors.accent.opacity(FRAME_OUTLINE_OPACITY) };
        window.paint_quad(outline(f, accent, gpui::BorderStyle::Solid));
        if p.is_multi() {
            paint_frame_label(i + 1, f.origin, accent, colors.label, window, cx);
        }
    }
    if let Some(sel) = selected.and_then(|i| frames.get(i)) {
        let h = px(HANDLE_SIZE);
        for (cx, cy) in
            [(sel.left(), sel.top()), (sel.right(), sel.top()), (sel.left(), sel.bottom()), (sel.right(), sel.bottom())]
        {
            let handle = Bounds::new(gpui::point(cx - h / 2.0, cy - h / 2.0), gpui::size(h, h));
            window.paint_quad(fill(handle, colors.accent));
        }
    }
}

/// Draws frame number `n` on a tab of `fill_color` inside the corner at `origin`.
fn paint_frame_label(
    n: usize,
    origin: gpui::Point<Pixels>,
    fill_color: gpui::Hsla,
    text: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let label: SharedString = n.to_string().into();
    let run = TextRun {
        len: label.len(),
        font: window.text_style().font(),
        color: text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let size = px(FRAME_LABEL_SIZE);
    let line = window.text_system().shape_line(label, size, &[run], None);
    let (pad, height) = (px(4.0), size * 1.4);
    let tab = Bounds::new(origin, gpui::size(line.width + pad * 2.0, height));
    window.paint_quad(fill(tab, fill_color));
    let _ = line.paint(gpui::point(origin.x + pad, origin.y), height, TextAlign::Left, None, window, cx);
}

/// Draws `frame` fitted into `bounds`, cut from `source`: an image and the
/// bed area it covers.
fn paint_frame_thumb(
    bounds: Bounds<Pixels>,
    frame: Area,
    source: Option<(Arc<RenderImage>, Area)>,
    colors: CanvasColors,
    window: &mut Window,
) {
    let (bw, bh) = (f64::from(bounds.size.width), f64::from(bounds.size.height));
    let scale = (bw / frame.w.max(f64::EPSILON)).min(bh / frame.h.max(f64::EPSILON));
    let (w, h) = (frame.w * scale, frame.h * scale);
    let rect = Bounds::new(
        gpui::point(bounds.origin.x + Pixels::from((bw - w) / 2.0), bounds.origin.y + Pixels::from((bh - h) / 2.0)),
        gpui::size(Pixels::from(w), Pixels::from(h)),
    );
    window.paint_quad(fill(rect, colors.bed));
    if let Some((img, area)) = source {
        let image = Bounds::new(
            gpui::point(
                rect.origin.x + Pixels::from((area.x - frame.x) * scale),
                rect.origin.y + Pixels::from((area.y - frame.y) * scale),
            ),
            gpui::size(Pixels::from(area.w * scale), Pixels::from(area.h * scale)),
        );
        let _ = window.paint_image(rect, image, Corners::default(), img, 0, false);
    }
    window.paint_quad(outline(rect, colors.border, gpui::BorderStyle::Solid));
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for img in self.stale_images.drain(..) {
            cx.drop_image(img, Some(window));
        }
        let theme = cx.theme().clone();
        let root = div()
            .size_full()
            .flex()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_step(true, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_step(false, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.zoom_to_fit(cx)))
            .on_action(cx.listener(|this, _: &SingleAreaMode, window, cx| this.set_multi(false, window, cx)))
            .on_action(cx.listener(|this, _: &MultipleAreaMode, window, cx| this.set_multi(true, window, cx)))
            .on_action(cx.listener(|this, _: &DeleteFrame, _, cx| this.delete_frame(cx)))
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .text_size(theme.font_size);
        let multi = self.preview.is_multi();
        if glass::active(cx) {
            // Nothing here paints a window background: the native backdrop
            // does, so the glass panes can show through.
            return root
                .flex_row()
                .child(self.render_glass_sidebar(cx))
                .child(
                    v_flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(self.render_glass_toolbar(cx))
                        .child(self.render_canvas(cx))
                        .child(self.render_glass_status(cx)),
                )
                .when(multi, |el| el.child(self.render_glass_frames_panel(cx)))
                // Painted last, after every pane.
                .child(glass::commit(CanvasColors::new(&theme).backdrop));
        }
        root.flex_col().bg(theme.background).child(self.render_title_bar(cx)).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(self.render_sidebar(cx))
                .child(v_flex().flex_1().h_full().min_w_0().child(self.render_canvas(cx)).child(self.render_status(cx)))
                .when(multi, |el| el.child(self.render_frames_panel(cx))),
        )
    }
}

/// A native push button for the glass layout.
fn native_button(
    key: &'static str,
    title: &'static str,
    role: ButtonRole,
    size: ControlSize,
    enabled: bool,
) -> gpui::Canvas<()> {
    glass::control(key, Spec { control: Control::Button { title: title.into(), role }, enabled, size })
}

/// A native pop-up button for the glass layout.
fn native_popup(
    key: &'static str,
    items: Vec<SharedString>,
    selected: Option<usize>,
    enabled: bool,
) -> gpui::Canvas<()> {
    glass::control(key, Spec { control: Control::PopUp { items, selected }, enabled, size: ControlSize::Regular })
}

/// A title bar that paints nothing, for the glass layout: it still drags
/// the window and zooms it on double-click.
fn transparent_title_bar(cx: &App) -> TitleBar {
    let clear = cx.theme().transparent;
    TitleBar::new().bg(clear).border_color(clear).pl_0()
}

/// The name a source is listed under.
fn source_title(source: &SourceCaps) -> SharedString {
    source.name.clone().into()
}

/// Replaces a select's items and selects `index`.
fn set_select(
    select: &Entity<SelectState<Vec<SharedString>>>,
    items: Vec<SharedString>,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) {
    select.update(cx, |s, cx| {
        let selected = (index < items.len()).then(|| IndexPath::new(index));
        s.set_items(items, window, cx);
        s.set_selected_index(selected, window, cx);
    });
}

/// Narrows a gamma value for the slider, which works in `f32`.
#[expect(clippy::cast_possible_truncation, reason = "gamma values are small, and the slider only needs ~2 decimals")]
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// Converts a packed RGB8 image to a GPU-ready image.
///
/// # Panics
/// If `img.data` doesn't match its dimensions, which the scanner worker
/// guarantees for every image it sends.
fn to_render_image(img: &RgbImage) -> Arc<RenderImage> {
    // GPUI textures are BGRA.
    let bgra: Vec<u8> = img.data.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 0xFF]).collect();
    let buffer = u32::try_from(img.width)
        .ok()
        .zip(u32::try_from(img.height).ok())
        .and_then(|(w, h)| image::RgbaImage::from_raw(w, h, bgra))
        .unwrap_or_else(|| panic!("pixel data doesn't match image dimensions: {img:?}"));
    Arc::new(RenderImage::new([image::Frame::new(buffer)]))
}

/// Estimated time left, assuming rows keep arriving at the average rate so far.
fn time_left(elapsed: Duration, rows: usize, total: usize) -> Option<Duration> {
    let done = u32::try_from(rows).ok().filter(|&d| d > 0)?;
    let left = u32::try_from(total.checked_sub(rows)?).ok().filter(|&l| l > 0)?;
    elapsed.checked_mul(left).map(|t| t / done)
}

/// Share of `total` that `done` represents, in `0.0..=1.0`, for the progress bar.
fn progress_fraction(done: usize, total: usize) -> f32 {
    /// Steps of the progress bar; far finer than a 1000 px bar can show.
    const STEPS: u16 = 1000;
    if total == 0 {
        return 0.0;
    }
    let steps = done.min(total).saturating_mul(usize::from(STEPS)) / total;
    f32::from(u16::try_from(steps).unwrap_or(STEPS)) / f32::from(STEPS)
}

/// Formats a duration as `1h 05m` or `4m 09s`.
fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}h {:02}m", s / 3600, s / 60 % 60) } else { format!("{}m {:02}s", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_left_extrapolates_average_rate() {
        let left = time_left(Duration::from_secs(10), 25, 100);
        assert_eq!(left, Some(Duration::from_secs(30)));
        assert_eq!(time_left(Duration::from_secs(10), 0, 100), None);
        assert_eq!(time_left(Duration::from_secs(10), 100, 100), None);
        assert_eq!(time_left(Duration::from_secs(10), 120, 100), None);
    }

    #[test]
    fn progress_fraction_is_clamped() {
        assert_eq!(progress_fraction(1, 4).to_bits(), 0.25_f32.to_bits());
        assert_eq!(progress_fraction(9, 4).to_bits(), 1.0_f32.to_bits());
        assert_eq!(progress_fraction(3, 0).to_bits(), 0.0_f32.to_bits());
    }

    #[test]
    fn durations_format_with_two_units() {
        assert_eq!(fmt_duration(Duration::from_secs(249)), "4m 09s");
        assert_eq!(fmt_duration(Duration::from_mins(65)), "1h 05m");
    }
}

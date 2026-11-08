//! Geometry and interaction state for the preview canvas.
//!
//! The canvas shows the scanner bed fitted into the available space, scaled by
//! a zoom factor and offset by a pan. The selection is kept in millimetres;
//! screen positions are mapped through the bed rectangle from the last layout.
//!
//! All maths is done in `f64`; gpui's `Pixels` converts to and from `f64`
//! losslessly enough for layout, so no lossy casts are needed.

use gpui::{Bounds, Pixels, Point, point, size};
use lightfast::job::Area;

/// Corner handle hit radius, in screen pixels.
const HANDLE_HIT: f64 = 10.0;
/// Selections smaller than this (mm) are treated as a click, not a drag.
const MIN_SELECTION_MM: f64 = 2.0;
/// Zoom limits, relative to the bed fitted in the viewport.
pub const MIN_ZOOM: f64 = 1.0;
pub const MAX_ZOOM: f64 = 16.0;
/// Factor applied by one zoom in/out step.
const ZOOM_STEP: f64 = 1.25;
/// Zoom changes smaller than this are ignored, avoiding relayout churn from
/// tiny pinch or scroll deltas at the clamps.
const ZOOM_EPSILON: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Drawing a new rectangle from `anchor`. `frame` is the frame being
    /// drawn, once the drag is large enough to count.
    New { anchor: (f64, f64), frame: Option<usize> },
    /// Moving `frame` (⌥-drag inside it); `grab` is the cursor offset from
    /// its top-left.
    Move { frame: usize, grab: (f64, f64) },
    /// Resizing `frame` with the opposite corner pinned at `anchor`.
    Resize { frame: usize, anchor: (f64, f64) },
}

/// Layout, zoom/pan and frame-editing state of the preview canvas.
///
/// In single mode there is always exactly one frame, selected. In multiple
/// mode there may be any number, and none may be selected.
#[derive(Debug, Clone)]
pub struct Preview {
    /// Bed width in millimetres.
    pub bed_w: f64,
    /// Bed height in millimetres.
    pub bed_h: f64,
    /// The scan areas, always within the bed, in file order.
    frames: Vec<Area>,
    selected: Option<usize>,
    multi: bool,
    /// Where the bed is drawn, in window coordinates.
    pub bed_rect: Bounds<Pixels>,
    /// The canvas area the bed is laid out in.
    pub viewport: Bounds<Pixels>,
    /// Scale relative to the fitted bed; 1.0 shows the whole bed.
    zoom: f64,
    /// Offset of the bed centre from the viewport centre, in pixels.
    pan: (f64, f64),
    drag: Option<Drag>,
}

impl Preview {
    /// Creates a single-mode preview of a `bed_w` × `bed_h` mm bed with all of it selected.
    pub fn new(bed_w: f64, bed_h: f64) -> Self {
        Self {
            bed_w,
            bed_h,
            frames: vec![Area::new(0.0, 0.0, bed_w, bed_h)],
            selected: Some(0),
            multi: false,
            bed_rect: Bounds::default(),
            viewport: Bounds::default(),
            zoom: MIN_ZOOM,
            pan: (0.0, 0.0),
            drag: None,
        }
    }

    /// Changes the bed size, keeping every frame on the bed.
    pub fn set_bed(&mut self, bed_w: f64, bed_h: f64) {
        self.bed_w = bed_w;
        self.bed_h = bed_h;
        for f in &mut self.frames {
            *f = f.clamped(bed_w, bed_h);
        }
    }

    /// Every frame, in file order.
    pub fn frames(&self) -> &[Area] {
        &self.frames
    }

    /// Index of the selected frame.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The selected frame; always present in single mode.
    pub fn selection(&self) -> Option<Area> {
        self.frames.get(self.selected?).copied()
    }

    /// Replaces the selected frame, or adds `area` as a new, selected
    /// frame if none is selected.
    pub fn set_selection(&mut self, area: Area) {
        let area = area.clamped(self.bed_w, self.bed_h);
        if let Some(f) = self.selected.and_then(|i| self.frames.get_mut(i)) {
            *f = area;
        } else {
            self.frames.push(area);
            self.selected = Some(self.frames.len() - 1);
        }
    }

    /// Selects frame `index`, or nothing (multiple mode only).
    pub fn select(&mut self, index: Option<usize>) {
        if index.is_some_and(|i| i < self.frames.len()) || (index.is_none() && self.multi) {
            self.selected = index;
        }
    }

    /// Whether several frames can be drawn.
    pub fn is_multi(&self) -> bool {
        self.multi
    }

    /// Switches between one frame and several.
    ///
    /// Entering multiple mode keeps the current area as the first frame,
    /// unless it is the whole bed, which would cover every frame drawn
    /// after it. Leaving it keeps the selected (or first) frame only.
    pub fn set_multi(&mut self, multi: bool) {
        if multi == self.multi {
            return;
        }
        self.multi = multi;
        self.drag = None;
        if multi {
            let whole = Area::new(0.0, 0.0, self.bed_w, self.bed_h);
            if self.frames.first().is_some_and(|f| *f == whole) {
                self.frames.clear();
                self.selected = None;
            }
        } else {
            let keep = self.selection().or_else(|| self.frames.first().copied());
            self.frames = vec![keep.unwrap_or(Area::new(0.0, 0.0, self.bed_w, self.bed_h))];
            self.selected = Some(0);
        }
    }

    /// Removes frame `index` (multiple mode only).
    pub fn remove(&mut self, index: usize) {
        if !self.multi || index >= self.frames.len() {
            return;
        }
        self.frames.remove(index);
        self.drag = None;
        self.selected = match self.selected {
            Some(s) if s == index => None,
            Some(s) if s > index => Some(s - 1),
            other => other,
        };
    }

    /// Removes every frame (multiple mode only).
    pub fn clear(&mut self) {
        if self.multi {
            self.frames.clear();
            self.selected = None;
            self.drag = None;
        }
    }

    /// Largest rectangle with the bed's aspect ratio centred in `outer`.
    fn fit(&self, outer: Bounds<Pixels>) -> Bounds<Pixels> {
        let (ow, oh) = (f64::from(outer.size.width), f64::from(outer.size.height));
        let scale = (ow / self.bed_w).min(oh / self.bed_h).max(0.0);
        let (w, h) = (self.bed_w * scale, self.bed_h * scale);
        Bounds::new(
            point(outer.origin.x + Pixels::from((ow - w) / 2.0), outer.origin.y + Pixels::from((oh - h) / 2.0)),
            size(Pixels::from(w), Pixels::from(h)),
        )
    }

    /// Lays the bed out in `viewport` at the current zoom and pan.
    pub fn layout(&mut self, viewport: Bounds<Pixels>) {
        self.viewport = viewport;
        self.relayout();
    }

    fn relayout(&mut self) {
        let fit = self.fit(self.viewport);
        let w = f64::from(fit.size.width) * self.zoom;
        let h = f64::from(fit.size.height) * self.zoom;
        // When the bed overflows the viewport it must keep covering it;
        // otherwise it stays centred.
        let slack_x = ((w - f64::from(self.viewport.size.width)) / 2.0).max(0.0);
        let slack_y = ((h - f64::from(self.viewport.size.height)) / 2.0).max(0.0);
        self.pan = (self.pan.0.clamp(-slack_x, slack_x), self.pan.1.clamp(-slack_y, slack_y));
        let c = self.viewport.center();
        self.bed_rect = Bounds::new(
            point(c.x + Pixels::from(self.pan.0 - w / 2.0), c.y + Pixels::from(self.pan.1 - h / 2.0)),
            size(Pixels::from(w), Pixels::from(h)),
        );
    }

    /// Scales by `factor`, keeping the bed point under `anchor` in place.
    pub fn zoom_at(&mut self, factor: f64, anchor: Point<Pixels>) {
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if !zoom.is_finite() || (zoom - self.zoom).abs() < ZOOM_EPSILON {
            return;
        }
        let ratio = zoom / self.zoom;
        let c = self.viewport.center();
        let (ax, ay) = (f64::from(anchor.x - c.x), f64::from(anchor.y - c.y));
        // The anchor's offset from the bed centre scales with the zoom.
        self.pan = (ax - (ax - self.pan.0) * ratio, ay - (ay - self.pan.1) * ratio);
        self.zoom = zoom;
        self.relayout();
    }

    /// Zooms one step in or out about the viewport centre.
    pub fn zoom_step(&mut self, zoom_in: bool) {
        let factor = if zoom_in { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        self.zoom_at(factor, self.viewport.center());
    }

    /// Shows the whole bed again.
    pub fn reset_zoom(&mut self) {
        self.zoom = MIN_ZOOM;
        self.pan = (0.0, 0.0);
        self.relayout();
    }

    /// Moves the bed by `delta`, within the limits that keep it covering the viewport.
    pub fn pan_by(&mut self, delta: Point<Pixels>) {
        self.pan.0 += f64::from(delta.x);
        self.pan.1 += f64::from(delta.y);
        self.relayout();
    }

    /// Current zoom relative to the fitted bed.
    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    fn px_per_mm(&self) -> f64 {
        f64::from(self.bed_rect.size.width) / self.bed_w
    }

    /// Converts a window position to bed millimetres, clamped to the bed.
    pub fn to_mm(&self, p: Point<Pixels>) -> (f64, f64) {
        let s = self.px_per_mm().max(f64::EPSILON);
        let x = f64::from(p.x - self.bed_rect.origin.x) / s;
        let y = f64::from(p.y - self.bed_rect.origin.y) / s;
        (x.clamp(0.0, self.bed_w), y.clamp(0.0, self.bed_h))
    }

    /// Screen rectangle of an area on the bed.
    pub fn to_screen(&self, a: Area) -> Bounds<Pixels> {
        let s = self.px_per_mm();
        Bounds::new(
            point(self.bed_rect.origin.x + Pixels::from(a.x * s), self.bed_rect.origin.y + Pixels::from(a.y * s)),
            size(Pixels::from(a.w * s), Pixels::from(a.h * s)),
        )
    }

    fn corners(a: Area) -> [(f64, f64); 4] {
        [(a.x, a.y), (a.x + a.w, a.y), (a.x, a.y + a.h), (a.x + a.w, a.y + a.h)]
    }

    /// Whether a mouse drag on the canvas is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The topmost (last drawn) frame containing the bed point (`x`, `y`).
    fn frame_at(&self, x: f64, y: f64) -> Option<usize> {
        self.frames.iter().rposition(|f| f.contains(x, y))
    }

    /// Starts a drag at `p`: resizing from a corner handle of the selected
    /// frame, moving a frame from inside it when `move_frame` is set, or
    /// otherwise drawing a new frame, even from inside an existing one. In
    /// single mode the new frame replaces the only one.
    pub fn mouse_down(&mut self, p: Point<Pixels>, move_frame: bool) {
        let (mx, my) = self.to_mm(p);
        let hit = HANDLE_HIT / self.px_per_mm().max(f64::EPSILON);
        let handle = self.selected.and_then(|i| {
            let corners = Self::corners(self.frames[i]);
            let near = corners.iter().position(|&(cx, cy)| (cx - mx).abs() <= hit && (cy - my).abs() <= hit)?;
            Some(Drag::Resize { frame: i, anchor: corners[3 - near] })
        });
        let moving = || {
            let frame = self.frame_at(mx, my).filter(|_| move_frame)?;
            let f = self.frames[frame];
            Some(Drag::Move { frame, grab: (mx - f.x, my - f.y) })
        };
        let drag = handle
            .or_else(moving)
            .unwrap_or(Drag::New { anchor: (mx, my), frame: if self.multi { None } else { Some(0) } });
        if let Drag::Move { frame, .. } | Drag::Resize { frame, .. } = drag {
            self.selected = Some(frame);
        }
        self.drag = Some(drag);
    }

    /// Updates the frame being dragged for a drag to `p`. Returns whether it changed.
    pub fn mouse_move(&mut self, p: Point<Pixels>) -> bool {
        let Some(drag) = self.drag else { return false };
        let (mx, my) = self.to_mm(p);
        let (index, next) = match drag {
            Drag::New { anchor, frame } => {
                let Some(area) = Self::spanned(anchor, (mx, my)) else { return false };
                let index = frame.unwrap_or_else(|| {
                    self.frames.push(area);
                    self.frames.len() - 1
                });
                self.drag = Some(Drag::New { anchor, frame: Some(index) });
                (index, area)
            }
            Drag::Resize { frame, anchor } => {
                let Some(area) = Self::spanned(anchor, (mx, my)) else { return false };
                (frame, area)
            }
            Drag::Move { frame, grab } => {
                let f = self.frames[frame];
                (frame, Area::new(mx - grab.0, my - grab.1, f.w, f.h))
            }
        };
        self.selected = Some(index);
        let next = next.clamped(self.bed_w, self.bed_h);
        let changed = next != self.frames[index];
        self.frames[index] = next;
        changed
    }

    /// The rectangle between two corners, if it is large enough to keep.
    fn spanned(a: (f64, f64), b: (f64, f64)) -> Option<Area> {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        (x1 - x0 >= MIN_SELECTION_MM && y1 - y0 >= MIN_SELECTION_MM).then(|| Area::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// Ends any drag. In multiple mode, a click that drew nothing selects
    /// the frame under it, or clears the selection on empty bed.
    pub fn mouse_up(&mut self) {
        if let Some(Drag::New { anchor, frame: None }) = self.drag.take()
            && self.multi
        {
            self.selected = self.frame_at(anchor.0, anchor.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::px;

    use super::*;

    fn preview() -> Preview {
        let mut p = Preview::new(200.0, 300.0);
        // 1 mm == 2 px, bed drawn at (10, 20).
        p.bed_rect = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(600.0)));
        p
    }

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    #[test]
    fn fit_preserves_bed_aspect() {
        let p = Preview::new(200.0, 300.0);
        let r = p.fit(Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(600.0))));
        assert_eq!(r.size, size(px(400.0), px(600.0)));
        assert_eq!(r.origin, point(px(300.0), px(0.0)));
    }

    #[test]
    fn drawing_outside_creates_new_selection() {
        let mut p = preview();
        p.set_selection(Area::new(0.0, 0.0, 20.0, 20.0));
        p.mouse_down(at(10.0 + 100.0, 20.0 + 100.0), false); // (50, 50) mm
        assert!(p.mouse_move(at(10.0 + 60.0, 20.0 + 300.0))); // (30, 150) mm
        assert_eq!(p.selection().unwrap(), Area::new(30.0, 50.0, 20.0, 100.0));
    }

    #[test]
    fn dragging_inside_moves_and_clamps_to_bed() {
        let mut p = preview();
        p.set_selection(Area::new(10.0, 10.0, 50.0, 50.0));
        p.mouse_down(at(10.0 + 40.0, 20.0 + 40.0), true); // (20, 20) mm: ⌥ inside
        p.mouse_move(at(10.0 + 400.0, 20.0 + 40.0)); // far right
        assert_eq!(p.selection().unwrap(), Area::new(150.0, 10.0, 50.0, 50.0));
    }

    fn zoomable() -> Preview {
        let mut p = Preview::new(200.0, 300.0);
        // Fits at 1 mm == 2 px, bed drawn at (10, 20).
        p.layout(Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(600.0))));
        p
    }

    #[test]
    fn zoom_at_keeps_anchor_fixed() {
        let mut p = zoomable();
        let anchor = at(10.0 + 100.0, 20.0 + 450.0);
        let before = p.to_mm(anchor);
        p.zoom_at(2.0, anchor);
        assert!((p.zoom() - 2.0).abs() < ZOOM_EPSILON);
        let after = p.to_mm(anchor);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
    }

    #[test]
    fn zoom_clamps_to_fit_and_max() {
        let mut p = zoomable();
        p.zoom_at(0.5, at(50.0, 50.0));
        assert!((p.zoom() - MIN_ZOOM).abs() < ZOOM_EPSILON);
        assert_eq!(p.bed_rect, p.viewport);
        p.zoom_at(1000.0, at(50.0, 50.0));
        assert!((p.zoom() - MAX_ZOOM).abs() < ZOOM_EPSILON);
        p.reset_zoom();
        assert_eq!(p.bed_rect, p.viewport);
    }

    #[test]
    fn pan_is_clamped_when_zoomed() {
        let mut p = zoomable();
        p.pan_by(at(500.0, 0.0));
        assert_eq!(p.bed_rect, p.viewport, "fitted bed cannot pan");
        p.zoom_step(true);
        p.pan_by(at(10_000.0, -10_000.0));
        let (b, v) = (p.bed_rect, p.viewport);
        assert_eq!(b.left(), v.left());
        assert_eq!(b.bottom(), v.bottom());
    }

    #[test]
    fn selection_maps_correctly_when_zoomed() {
        let mut p = zoomable();
        p.zoom_at(4.0, p.viewport.origin); // 1 mm == 8 px, bed origin pinned
        p.set_selection(Area::new(0.0, 0.0, 10.0, 10.0));
        p.mouse_down(at(10.0 + 80.0, 20.0 + 80.0), false); // bottom-right corner (10, 10)
        p.mouse_move(at(10.0 + 160.0, 20.0 + 120.0)); // (20, 15)
        assert_eq!(p.selection().unwrap(), Area::new(0.0, 0.0, 20.0, 15.0));
    }

    #[test]
    fn corner_drag_resizes_against_opposite_corner() {
        let mut p = preview();
        p.set_selection(Area::new(10.0, 10.0, 50.0, 50.0));
        p.mouse_down(at(10.0 + 120.0, 20.0 + 120.0), false); // bottom-right corner (60, 60)
        p.mouse_move(at(10.0 + 200.0, 20.0 + 160.0)); // (100, 80)
        assert_eq!(p.selection().unwrap(), Area::new(10.0, 10.0, 90.0, 70.0));
    }

    #[test]
    fn drawing_inside_selection_replaces_it() {
        let mut p = preview(); // full-bed selection
        p.mouse_down(at(10.0 + 100.0, 20.0 + 100.0), false); // (50, 50) mm
        assert!(p.mouse_move(at(10.0 + 200.0, 20.0 + 240.0))); // (100, 120) mm
        assert_eq!(p.selection().unwrap(), Area::new(50.0, 50.0, 50.0, 70.0));
    }

    #[test]
    fn click_without_drag_keeps_selection() {
        let mut p = preview();
        p.set_selection(Area::new(10.0, 10.0, 50.0, 50.0));
        p.mouse_down(at(10.0 + 60.0, 20.0 + 60.0), false); // (30, 30) mm
        assert!(!p.mouse_move(at(10.0 + 61.0, 20.0 + 61.0)));
        p.mouse_up();
        assert_eq!(p.selection().unwrap(), Area::new(10.0, 10.0, 50.0, 50.0));
    }

    fn multi() -> Preview {
        let mut p = preview();
        p.set_multi(true);
        p
    }

    /// Drags from one bed point (mm) to another, at 2 px per mm.
    fn drag(p: &mut Preview, from: (f32, f32), to: (f32, f32), alt: bool) {
        p.mouse_down(at(10.0 + from.0 * 2.0, 20.0 + from.1 * 2.0), alt);
        p.mouse_move(at(10.0 + to.0 * 2.0, 20.0 + to.1 * 2.0));
        p.mouse_up();
    }

    #[test]
    fn entering_multi_drops_a_whole_bed_frame_but_keeps_a_chosen_one() {
        assert_eq!(multi().frames(), []);
        let mut p = preview();
        p.set_selection(Area::new(10.0, 10.0, 50.0, 50.0));
        p.set_multi(true);
        assert_eq!(p.frames(), [Area::new(10.0, 10.0, 50.0, 50.0)]);
    }

    #[test]
    fn dragging_adds_frames_and_selects_the_newest() {
        let mut p = multi();
        drag(&mut p, (10.0, 10.0), (50.0, 40.0), false);
        drag(&mut p, (100.0, 100.0), (150.0, 200.0), false);
        assert_eq!(p.frames(), [Area::new(10.0, 10.0, 40.0, 30.0), Area::new(100.0, 100.0, 50.0, 100.0)]);
        assert_eq!(p.selected(), Some(1));
    }

    #[test]
    fn clicks_select_frames_without_adding_any() {
        let mut p = multi();
        drag(&mut p, (10.0, 10.0), (50.0, 40.0), false);
        drag(&mut p, (100.0, 100.0), (150.0, 200.0), false);
        drag(&mut p, (20.0, 20.0), (20.5, 20.5), false);
        assert_eq!((p.frames().len(), p.selected()), (2, Some(0)));
        drag(&mut p, (180.0, 280.0), (180.0, 280.0), false);
        assert_eq!(p.selected(), None, "clicking empty bed clears the selection");
    }

    #[test]
    fn alt_drag_moves_the_frame_under_the_cursor() {
        let mut p = multi();
        drag(&mut p, (10.0, 10.0), (50.0, 40.0), false);
        drag(&mut p, (100.0, 100.0), (150.0, 200.0), false);
        drag(&mut p, (20.0, 20.0), (30.0, 25.0), true);
        assert_eq!(p.frames()[0], Area::new(20.0, 15.0, 40.0, 30.0));
        assert_eq!(p.selected(), Some(0));
    }

    #[test]
    fn removing_frames_keeps_the_selection_on_the_same_frame() {
        let mut p = multi();
        for i in 0..3_u8 {
            let y = f32::from(i) * 60.0;
            drag(&mut p, (10.0, y + 10.0), (50.0, y + 50.0), false);
        }
        p.remove(0);
        assert_eq!(p.selected(), Some(1));
        assert_eq!(p.selection(), Some(Area::new(10.0, 130.0, 40.0, 40.0)));
        p.remove(1);
        assert_eq!(p.selected(), None);
        p.clear();
        assert_eq!(p.frames(), []);
    }

    #[test]
    fn leaving_multi_keeps_the_selected_frame() {
        let mut p = multi();
        drag(&mut p, (10.0, 10.0), (50.0, 40.0), false);
        drag(&mut p, (100.0, 100.0), (150.0, 200.0), false);
        p.select(Some(0));
        p.set_multi(false);
        assert_eq!(p.frames(), [Area::new(10.0, 10.0, 40.0, 30.0)]);
        p.remove(0);
        assert_eq!(p.frames().len(), 1, "single mode always keeps its frame");
        let mut empty = multi();
        empty.set_multi(false);
        assert_eq!(empty.selection(), Some(Area::new(0.0, 0.0, 200.0, 300.0)));
    }
}

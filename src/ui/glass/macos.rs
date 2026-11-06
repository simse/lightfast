//! `AppKit` side of [`super`]: material views and a backdrop layer kept
//! beneath GPUI's view.
//!
//! The view hierarchy inside the window's content view, bottom to top:
//! backdrop (a layer-hosting view with a `CAShapeLayer`), one
//! `NSVisualEffectView` per sidebar pane, an `NSGlassEffectContainerView`
//! holding one glass view per glass pane, and GPUI's own view.

use std::cell::RefCell;
use std::collections::HashMap;

use gpui::{Bounds, Hsla, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, MainThreadOnly as _, Message as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions,
    NSGlassEffectContainerView, NSGlassEffectView, NSGlassEffectViewStyle, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGMutablePath};
use objc2_quartz_core::{CAShapeLayer, CATransaction, kCAFillRuleEvenOdd};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{Material, Painted, control as gpui_control, take_frame};

mod control;

/// Whether this system has the Liquid Glass classes, checked by name so
/// older systems never trigger a failing class lookup.
pub(super) fn supported() -> bool {
    MainThreadMarker::new().is_some()
        && AnyClass::get(c"NSGlassEffectView").is_some()
        && AnyClass::get(c"NSGlassEffectContainerView").is_some()
}

/// A pane's native view, by the material it was created with.
enum NativePane {
    Glass(Retained<NSGlassEffectView>),
    Sidebar(Retained<NSVisualEffectView>),
}

impl NativePane {
    fn view(&self) -> &NSView {
        match self {
            Self::Glass(view) => view,
            Self::Sidebar(view) => view,
        }
    }

    fn material(&self) -> Material {
        match self {
            Self::Glass(_) => Material::Glass,
            Self::Sidebar(_) => Material::Sidebar,
        }
    }

    fn set_radius(&self, radius: CGFloat) {
        match self {
            Self::Glass(view) => view.setCornerRadius(radius),
            Self::Sidebar(view) => {
                if let Some(layer) = view.layer() {
                    layer.setCornerRadius(radius);
                    layer.setMasksToBounds(radius > 0.0);
                }
            }
        }
    }
}

struct PaneView {
    native: NativePane,
    radius: CGFloat,
}

/// Native views owned for one GPUI window.
struct Native {
    gpui: Retained<NSView>,
    backdrop: Retained<NSView>,
    shape: Retained<CAShapeLayer>,
    container: Retained<NSGlassEffectContainerView>,
    dark: Option<Retained<NSAppearance>>,
    /// The container's content view, where pane views live.
    host: Retained<NSView>,
    panes: HashMap<&'static str, PaneView>,
    controls: HashMap<&'static str, control::View>,
    attached: bool,
    fill: Option<[u8; 3]>,
    cutouts: Vec<(CGRect, CGFloat)>,
    size: CGSize,
}

thread_local! {
    /// Keyed by the address of GPUI's `NSView`.
    static WINDOWS: RefCell<HashMap<usize, Native>> = RefCell::new(HashMap::new());
}

fn gpui_view(window: &Window) -> Option<Retained<NSView>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: the handle is GPUI's live `NSView` for `window`, borrowed on the
    // main thread for this paint; retaining it keeps it valid afterwards.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    Some(view.retain())
}

/// Brings the native views in line with this frame's panes, or removes
/// them when glass is off.
pub(super) fn commit(window: &Window, enabled: bool, backdrop: Hsla) {
    let frame = take_frame();
    let controls = gpui_control::take_frame();
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(gpui) = gpui_view(window) else { return };
    let key = Retained::as_ptr(&gpui) as usize;
    WINDOWS.with_borrow_mut(|windows| {
        // Forget windows AppKit has closed.
        windows.retain(|_, native| native.gpui.window().is_some());
        if !enabled {
            if let Some(native) = windows.get_mut(&key) {
                native.detach();
            }
            return;
        }
        let native = windows.entry(key).or_insert_with(|| Native::new(gpui, mtm));
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        native.sync(&frame, to_srgb(backdrop), mtm);
        native.sync_controls(&controls, mtm);
        CATransaction::commit();
    });
}

impl Native {
    fn new(gpui: Retained<NSView>, mtm: MainThreadMarker) -> Self {
        let zero = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
        let shape = CAShapeLayer::new();
        // SAFETY: `kCAFillRuleEvenOdd` is an immutable string constant
        // exported by Quartz Core.
        shape.setFillRule(unsafe { kCAFillRuleEvenOdd });
        // A layer-hosting view: setting the layer before `wantsLayer` leaves
        // its contents entirely to us.
        let backdrop = NSView::initWithFrame(NSView::alloc(mtm), zero);
        backdrop.setLayer(Some(&shape));
        backdrop.setWantsLayer(true);

        let container = NSGlassEffectContainerView::initWithFrame(NSGlassEffectContainerView::alloc(mtm), zero);
        let host = NSView::initWithFrame(NSView::alloc(mtm), zero);
        host.setAutoresizingMask(sizable());
        container.setContentView(Some(&host));
        // SAFETY: `NSAppearanceNameDarkAqua` is an immutable string constant
        // exported by AppKit. The app is always dark.
        let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
        container.setAppearance(dark.as_deref());
        backdrop.setAutoresizingMask(sizable());
        container.setAutoresizingMask(sizable());

        Self {
            gpui,
            backdrop,
            shape,
            container,
            dark,
            host,
            panes: HashMap::new(),
            controls: HashMap::new(),
            attached: false,
            fill: None,
            cutouts: Vec::new(),
            size: CGSize::new(0.0, 0.0),
        }
    }

    fn attach(&mut self) {
        if self.attached {
            return;
        }
        // SAFETY: called on the main thread; GPUI's view is in its window's
        // view hierarchy for as long as the window is open.
        let Some(parent) = (unsafe { self.gpui.superview() }) else { return };
        let bounds = parent.bounds();
        self.backdrop.setFrame(bounds);
        self.container.setFrame(bounds);
        self.host.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), bounds.size));
        // Bottom to top: backdrop, glass, GPUI.
        parent.addSubview_positioned_relativeTo(&self.container, NSWindowOrderingMode::Below, Some(&self.gpui));
        parent.addSubview_positioned_relativeTo(&self.backdrop, NSWindowOrderingMode::Below, Some(&self.container));
        self.attached = true;
    }

    fn detach(&mut self) {
        if !self.attached {
            return;
        }
        for (_, pane) in self.panes.drain() {
            pane.native.view().removeFromSuperview();
        }
        for (_, control) in self.controls.drain() {
            control.remove();
        }
        self.container.removeFromSuperview();
        self.backdrop.removeFromSuperview();
        self.attached = false;
        self.cutouts.clear();
        self.fill = None;
    }

    fn sync_controls(&mut self, frame: &[gpui_control::Painted], mtm: MainThreadMarker) {
        if !self.attached {
            return;
        }
        let height = self.backdrop.bounds().size.height;
        control::sync(&mut self.controls, frame, &self.gpui, height, self.dark.as_deref(), mtm);
    }

    fn sync(&mut self, frame: &[Painted], fill: [u8; 3], mtm: MainThreadMarker) {
        self.attach();
        if !self.attached {
            return;
        }
        let size = self.backdrop.bounds().size;
        if self.fill != Some(fill) {
            let [r, g, b] = fill.map(|c| CGFloat::from(c) / 255.0);
            let color = CGColor::new_srgb(r, g, b, 1.0);
            self.shape.setFillColor(Some(&color));
            self.fill = Some(fill);
        }

        let mut cutouts = Vec::new();
        for painted in frame {
            let rect = to_appkit(painted.bounds, size.height);
            let radius =
                CGFloat::from(f32::from(painted.pane.radius)).min(rect.size.height / 2.0).min(rect.size.width / 2.0);
            let material = painted.pane.material;
            if material == Material::Sidebar {
                // The desktop must show through the backdrop for the sidebar
                // material to blur it.
                cutouts.push((rect, radius));
            }
            // A key keeps its material; a changed one gets a fresh view.
            if self.panes.get(painted.key).is_some_and(|p| p.native.material() != material)
                && let Some(old) = self.panes.remove(painted.key)
            {
                old.native.view().removeFromSuperview();
            }
            let pane = self.panes.entry(painted.key).or_insert_with(|| PaneView {
                native: new_pane(material, rect, &self.host, &self.container, self.dark.as_deref(), mtm),
                radius: -1.0,
            });
            let view = pane.native.view();
            if view.frame() != rect {
                view.setFrame(rect);
            }
            view.setAutoresizingMask(anchoring(rect, size));
            if (pane.radius - radius).abs() > f64::EPSILON {
                pane.native.set_radius(radius);
                pane.radius = radius;
            }
        }
        self.panes.retain(|key, pane| {
            let keep = frame.iter().any(|p| p.key == *key);
            if !keep {
                pane.native.view().removeFromSuperview();
            }
            keep
        });

        if cutouts != self.cutouts || size != self.size {
            let path = CGMutablePath::new();
            let whole = CGRect::new(CGPoint::new(0.0, 0.0), size);
            // SAFETY: a null transform means identity; `path` is a live
            // mutable path owned by this frame.
            unsafe {
                CGMutablePath::add_rect(Some(&path), std::ptr::null(), whole);
                for &(rect, radius) in &cutouts {
                    CGMutablePath::add_rounded_rect(Some(&path), std::ptr::null(), rect, radius, radius);
                }
            }
            self.shape.setPath(Some(&path));
            self.cutouts = cutouts;
            self.size = size;
        }
    }
}

/// Creates a pane's view and inserts it where its material belongs: glass
/// inside the glass container's `host`, the sidebar material just below the
/// `container`, above the backdrop.
fn new_pane(
    material: Material,
    frame: CGRect,
    host: &NSView,
    container: &NSView,
    dark: Option<&NSAppearance>,
    mtm: MainThreadMarker,
) -> NativePane {
    match material {
        Material::Glass => {
            let view = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
            view.setStyle(NSGlassEffectViewStyle::Regular);
            host.addSubview(&view);
            NativePane::Glass(view)
        }
        Material::Sidebar => {
            let view = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
            view.setMaterial(NSVisualEffectMaterial::Sidebar);
            view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            view.setState(NSVisualEffectState::FollowsWindowActiveState);
            view.setWantsLayer(true);
            view.setAppearance(dark);
            // SAFETY: called on the main thread; the container was attached to
            // the window's content view before any pane is created.
            if let Some(parent) = unsafe { container.superview() } {
                parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(container));
            }
            NativePane::Sidebar(view)
        }
    }
}

fn sizable() -> NSAutoresizingMaskOptions {
    NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable
}

/// Keeps a pane glued to its nearest edges while `AppKit` live-resizes the
/// window, until the next GPUI frame places it exactly.
fn anchoring(rect: CGRect, size: CGSize) -> NSAutoresizingMaskOptions {
    let horizontal = if rect.size.width > size.width * 0.5 {
        NSAutoresizingMaskOptions::ViewWidthSizable
    } else if rect.origin.x + rect.size.width / 2.0 > size.width / 2.0 {
        NSAutoresizingMaskOptions::ViewMinXMargin
    } else {
        NSAutoresizingMaskOptions::ViewMaxXMargin
    };
    let vertical = if rect.size.height > size.height * 0.5 {
        NSAutoresizingMaskOptions::ViewHeightSizable
    } else if rect.origin.y + rect.size.height / 2.0 > size.height / 2.0 {
        // AppKit's y axis points up: a flexible bottom margin pins the top.
        NSAutoresizingMaskOptions::ViewMinYMargin
    } else {
        NSAutoresizingMaskOptions::ViewMaxYMargin
    };
    horizontal | vertical
}

/// GPUI bounds (top-left origin, points) to the unflipped parent's frame space.
fn to_appkit(bounds: Bounds<Pixels>, height: CGFloat) -> CGRect {
    let [x, y, w, h] =
        [bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height].map(|v| CGFloat::from(f32::from(v)));
    CGRect::new(CGPoint::new(x, height - y - h), CGSize::new(w, h))
}

/// 8-bit sRGB channels of `color`, ignoring alpha.
fn to_srgb(color: Hsla) -> [u8; 3] {
    let rgb = color.to_rgb();
    [rgb.r, rgb.g, rgb.b].map(channel)
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to 0..=255 before the cast")]
fn channel(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

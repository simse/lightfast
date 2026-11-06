//! Liquid Glass: the system's materials behind the window chrome.
//!
//! GPUI draws the whole window into one Metal layer, so it cannot refract
//! what lies beneath an element. On macOS 26+ the window is made transparent
//! instead and real `AppKit` views are kept *below* GPUI's view:
//!
//! 1. a backdrop layer painted in the content colour, with a cut-out under
//!    every [`Material::Sidebar`] pane so the desktop shows through;
//! 2. one native view per [`pane`]: an `NSVisualEffectView` with the system
//!    sidebar material, or an `NSGlassEffectView` inside a glass container;
//! 3. GPUI's view on top, leaving each pane's area unpainted so the material
//!    shows through and the pane's own controls sit on it.
//!
//! Panes record their painted bounds every frame; [`commit`], painted last,
//! reconciles the native views with that frame's list. Elsewhere (other
//! platforms, older macOS, tests) [`active`] is false, everything here is
//! inert and the UI renders opaque.

use gpui::{App, Bounds, Global, Hsla, IntoElement, Pixels, Styled as _, canvas, px};
use gpui_component::Theme;

mod control;
#[cfg(all(target_os = "macos", not(test)))]
mod macos;

pub use control::{ButtonRole, Control, ControlEvent, ControlSize, Spec, control, control_events};

/// The system material behind a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Material {
    /// The frosted sidebar material, blurring the desktop behind the window,
    /// as in Finder.
    Sidebar,
    /// Liquid Glass over the window's own backdrop: bars and capsules
    /// floating over the content area.
    Glass,
}

/// One material surface, sized by the element it is painted in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pane {
    pub radius: Pixels,
    pub material: Material,
}

/// Whether Liquid Glass is in use; render code reads it to pick the layout.
#[derive(Debug, Clone, Copy, Default)]
struct Glass(bool);

impl Global for Glass {}

/// Inset of floating bars from the window edge.
pub const INSET: f32 = 8.0;
/// Traffic-light origin, as in Finder: 19 pt in from the window's corner,
/// so the 14 pt buttons centre on a 52 pt toolbar.
pub const TRAFFIC_LIGHTS: (f32, f32) = (19.0, 19.0);

/// Decides once, at startup, whether the UI renders on glass.
pub fn init(cx: &mut App) {
    #[cfg(all(target_os = "macos", not(test)))]
    let supported = macos::supported();
    #[cfg(not(all(target_os = "macos", not(test))))]
    let supported = false;
    cx.set_global(Glass(supported));
}

/// Whether the UI renders on Liquid Glass.
pub fn active(cx: &App) -> bool {
    cx.try_global::<Glass>().is_some_and(|g| g.0)
}

/// An absolutely positioned child that turns its parent's bounds into glass.
///
/// The parent must be `.relative()` and paint no background of its own.
/// `key` must be unique within the window.
pub fn pane(key: &'static str, pane: Pane) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, (), _, _| record(key, bounds, pane)).absolute().inset_0()
}

/// Reconciles the native views with the panes and controls painted this frame.
///
/// Must be painted after every [`pane`] and [`control`], so it goes last in the root
/// element. `backdrop` fills the window outside sidebar panes.
pub fn commit(backdrop: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, (), window, cx| {
            let enabled = active(cx);
            #[cfg(all(target_os = "macos", not(test)))]
            macos::commit(window, enabled, backdrop);
            #[cfg(not(all(target_os = "macos", not(test))))]
            {
                let _ = (window, enabled, backdrop);
                FRAME.with_borrow_mut(Vec::clear);
                control::FRAME.with_borrow_mut(Vec::clear);
            }
        },
    )
    .absolute()
    .size_0()
}

/// Hover, selection and hairline fills for controls on glass.
///
/// They are the foreground at low opacity so the material reads through,
/// as `AppKit`'s own selection does.
#[derive(Debug, Clone, Copy)]
pub struct Fills {
    pub hover: Hsla,
    pub active: Hsla,
    pub line: Hsla,
}

impl Fills {
    /// Fills for `theme`, which is always dark.
    pub fn new(theme: &Theme) -> Self {
        let fg = theme.foreground;
        Self { hover: fg.opacity(0.08), active: fg.opacity(0.14), line: fg.opacity(0.10) }
    }
}

/// Fits the component library to glass, if active.
///
/// Secondary text becomes translucent, like `AppKit`'s secondary label
/// colour, so it keeps its contrast over the sidebar material.
pub fn adapt_theme(cx: &mut App) {
    if !active(cx) {
        return;
    }
    Theme::update(cx, |theme| theme.muted_foreground = theme.foreground.opacity(0.6));
}

/// One pane painted this frame, as the native side receives it.
#[cfg_attr(
    not(all(target_os = "macos", not(test))),
    expect(dead_code, reason = "only the macOS bridge reads painted panes")
)]
struct Painted {
    key: &'static str,
    bounds: Bounds<Pixels>,
    pane: Pane,
}

thread_local! {
    /// Panes painted since the last commit. GPUI paints windows one at a
    /// time on the main thread and this app has one window, which paints
    /// its own [`commit`], so a single list suffices.
    static FRAME: std::cell::RefCell<Vec<Painted>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record(key: &'static str, bounds: Bounds<Pixels>, pane: Pane) {
    // Collapsed or not yet laid out.
    if bounds.size.width <= px(0.5) || bounds.size.height <= px(0.5) {
        return;
    }
    FRAME.with_borrow_mut(|frame| frame.push(Painted { key, bounds, pane }));
}

/// Takes this frame's panes, leaving the list empty for the next frame.
#[cfg(all(target_os = "macos", not(test)))]
fn take_frame() -> Vec<Painted> {
    FRAME.with_borrow_mut(std::mem::take)
}

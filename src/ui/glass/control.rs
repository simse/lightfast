//! Native `AppKit` controls laid out by GPUI.
//!
//! On glass the app's pop-up buttons, segmented controls, sliders and push
//! buttons are the system's own, so they take the platform's Liquid Glass
//! look and behaviour. A [`control`] is an empty GPUI element that reserves
//! the control's space and records its painted bounds and [`Spec`] every
//! frame; the glass commit then creates, updates, places or removes the
//! matching `NSControl` above GPUI's view. User actions come back as
//! [`ControlEvent`]s on [`control_events`], tagged with the control's key.

use std::cell::RefCell;

use async_channel::{Receiver, Sender};
use gpui::{Bounds, Canvas, Pixels, SharedString, Styled as _, canvas, px};

/// What a native control shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// A pop-up button choosing one of `items`.
    PopUp { items: Vec<SharedString>, selected: Option<usize> },
    /// A segmented control. With `selected` it picks one segment; without,
    /// each segment acts as a button.
    Segmented { labels: Vec<SharedString>, selected: Option<usize> },
    /// A continuous horizontal slider.
    Slider { min: f64, max: f64, value: f64 },
    /// A push button.
    Button { title: SharedString, role: ButtonRole },
}

/// The part a push button plays, which sets its appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonRole {
    Normal,
    /// The window's default action: accent coloured and triggered by Return.
    Default,
    /// An action that stops or discards work.
    Destructive,
}

/// The system control size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlSize {
    Small,
    Regular,
    Large,
}

impl ControlSize {
    /// Height GPUI reserves for a control of this size; the native control
    /// is centred vertically within it at its own height.
    pub fn height(self) -> Pixels {
        match self {
            Self::Small => px(24.0),
            Self::Regular => px(30.0),
            Self::Large => px(36.0),
        }
    }
}

/// A native control as rendered this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub control: Control,
    pub enabled: bool,
    pub size: ControlSize,
}

/// A user action on a native control.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(
    not(all(target_os = "macos", not(test))),
    expect(dead_code, reason = "only the macOS bridge sends control events")
)]
pub enum ControlEvent {
    /// A pop-up item or segment was chosen.
    Selected(usize),
    /// A slider moved.
    Value(f64),
    /// A button was pressed.
    Clicked,
}

/// Reserves full-width space for a native control and keeps it in place.
///
/// `key` must be unique within the window and always name the same kind of
/// control. Callers may override the width.
pub fn control(key: &'static str, spec: Spec) -> Canvas<()> {
    let height = spec.size.height();
    canvas(|_, _, _| {}, move |bounds, (), _, _| record(key, bounds, spec)).w_full().h(height).flex_none()
}

/// Actions on native controls, tagged with their keys.
pub fn control_events() -> Receiver<Action> {
    EVENTS.with(|(_, rx)| rx.clone())
}

/// One control painted this frame, as the native side receives it.
#[cfg_attr(
    not(all(target_os = "macos", not(test))),
    expect(dead_code, reason = "only the macOS bridge reads painted controls")
)]
pub(super) struct Painted {
    pub key: &'static str,
    pub bounds: Bounds<Pixels>,
    pub spec: Spec,
}

/// An action, tagged with the key of the control it came from.
type Action = (&'static str, ControlEvent);

thread_local! {
    /// Controls painted since the last commit, like the panes' list.
    pub(super) static FRAME: RefCell<Vec<Painted>> = const { RefCell::new(Vec::new()) };
    /// Unbounded, and this thread holds a receiver, so sends never fail.
    static EVENTS: (Sender<Action>, Receiver<Action>) = async_channel::unbounded();
}

fn record(key: &'static str, bounds: Bounds<Pixels>, spec: Spec) {
    if bounds.size.width <= px(0.5) || bounds.size.height <= px(0.5) {
        return;
    }
    FRAME.with_borrow_mut(|frame| frame.push(Painted { key, bounds, spec }));
}

/// Delivers an action from the native side.
#[cfg(all(target_os = "macos", not(test)))]
pub(super) fn send(key: &'static str, event: ControlEvent) {
    EVENTS.with(|(tx, _)| {
        // Cannot fail: the channel is unbounded and `EVENTS` keeps it open.
        let _ = tx.try_send((key, event));
    });
}

/// Takes this frame's controls, leaving the list empty for the next frame.
#[cfg(all(target_os = "macos", not(test)))]
pub(super) fn take_frame() -> Vec<Painted> {
    FRAME.with_borrow_mut(std::mem::take)
}

//! `AppKit` side of [`super::super::control`]: one `NSControl` per key, kept
//! above GPUI's view so it draws on top and takes its own clicks.

use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSBezelStyle, NSButton, NSControl, NSControlSize, NSPopUpButton,
    NSSegmentDistribution, NSSegmentSwitchTracking, NSSegmentedControl, NSSlider, NSView, NSWindowOrderingMode,
};
use objc2_core_foundation::{CGFloat, CGRect};
use objc2_foundation::NSString;

use super::super::control::{ButtonRole, Control, ControlEvent, ControlSize, Painted, Spec, send};
use super::to_appkit;

struct TargetIvars {
    key: &'static str,
}

define_class!(
    /// Receives a control's action and forwards it to GPUI by key.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ScanAppControlTarget"]
    #[ivars = TargetIvars]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(act:))]
        fn act(&self, sender: &NSControl) {
            if let Some(event) = event(sender) {
                send(self.ivars().key, event);
            }
        }
    }
);

impl Target {
    fn new(key: &'static str, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { key });
        // SAFETY: `init` is `NSObject`'s designated initialiser and takes no
        // arguments.
        unsafe { msg_send![super(this), init] }
    }
}

/// Reads what an action means from the control that sent it.
fn event(sender: &NSControl) -> Option<ControlEvent> {
    // `NSPopUpButton` is an `NSButton`, so it is checked first.
    if let Some(popup) = sender.downcast_ref::<NSPopUpButton>() {
        return usize::try_from(popup.indexOfSelectedItem()).ok().map(ControlEvent::Selected);
    }
    if let Some(segmented) = sender.downcast_ref::<NSSegmentedControl>() {
        return usize::try_from(segmented.selectedSegment()).ok().map(ControlEvent::Selected);
    }
    if sender.downcast_ref::<NSSlider>().is_some() {
        return Some(ControlEvent::Value(sender.doubleValue()));
    }
    Some(ControlEvent::Clicked)
}

enum Kind {
    PopUp(Retained<NSPopUpButton>),
    Segmented(Retained<NSSegmentedControl>),
    Slider(Retained<NSSlider>),
    Button(Retained<NSButton>),
}

impl Kind {
    fn control(&self) -> &NSControl {
        match self {
            Self::PopUp(c) => c,
            Self::Segmented(c) => c,
            Self::Slider(c) => c,
            Self::Button(c) => c,
        }
    }
}

/// A native control and the spec it currently shows.
pub(super) struct View {
    kind: Kind,
    spec: Spec,
    /// Controls hold their target weakly; this keeps it alive.
    _target: Retained<Target>,
}

impl View {
    pub(super) fn remove(&self) {
        self.kind.control().removeFromSuperview();
    }
}

/// Brings `views` in line with this frame's controls.
pub(super) fn sync(
    views: &mut HashMap<&'static str, View>,
    frame: &[Painted],
    gpui: &NSView,
    height: CGFloat,
    dark: Option<&NSAppearance>,
    mtm: MainThreadMarker,
) {
    for painted in frame {
        let kind_changed = views
            .get(painted.key)
            .is_some_and(|v| std::mem::discriminant(&v.spec.control) != std::mem::discriminant(&painted.spec.control));
        if kind_changed && let Some(old) = views.remove(painted.key) {
            old.remove();
        }
        if let Some(view) = views.get_mut(painted.key) {
            if view.spec != painted.spec {
                apply(&view.kind, Some(&view.spec), &painted.spec);
                view.spec = painted.spec.clone();
            }
        } else {
            let view = create(painted.key, &painted.spec, gpui, dark, mtm);
            views.insert(painted.key, view);
        }
        if let Some(view) = views.get(painted.key) {
            place(view.kind.control(), to_appkit(painted.bounds, height));
        }
    }
    views.retain(|key, view| {
        let keep = frame.iter().any(|p| p.key == *key);
        if !keep {
            view.remove();
        }
        keep
    });
}

fn create(key: &'static str, spec: &Spec, gpui: &NSView, dark: Option<&NSAppearance>, mtm: MainThreadMarker) -> View {
    let zero = CGRect::default();
    let kind = match spec.control {
        Control::PopUp { .. } => {
            Kind::PopUp(NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), zero, false))
        }
        Control::Segmented { .. } => {
            let c = NSSegmentedControl::initWithFrame(NSSegmentedControl::alloc(mtm), zero);
            c.setSegmentDistribution(NSSegmentDistribution::FillEqually);
            Kind::Segmented(c)
        }
        Control::Slider { .. } => {
            let c = NSSlider::initWithFrame(NSSlider::alloc(mtm), zero);
            c.setContinuous(true);
            Kind::Slider(c)
        }
        Control::Button { .. } => {
            let c = NSButton::initWithFrame(NSButton::alloc(mtm), zero);
            c.setBezelStyle(NSBezelStyle::Push);
            Kind::Button(c)
        }
    };
    let target = Target::new(key, mtm);
    let control = kind.control();
    // SAFETY: `target` lives in the returned `View` as long as the control,
    // and `act:` takes the single sender argument AppKit passes.
    unsafe {
        control.setTarget(Some(&target));
        control.setAction(Some(sel!(act:)));
    };
    control.setAppearance(dark);
    apply(&kind, None, spec);
    // SAFETY: called on the main thread; GPUI's view is in its window's
    // content view for as long as the window is open.
    if let Some(parent) = unsafe { gpui.superview() } {
        parent.addSubview_positioned_relativeTo(control, NSWindowOrderingMode::Above, Some(gpui));
    }
    View { kind, spec: spec.clone(), _target: target }
}

/// Applies `new` to the control, touching only what differs from `old`.
fn apply(kind: &Kind, old: Option<&Spec>, new: &Spec) {
    let control = kind.control();
    if old.is_none_or(|o| o.enabled != new.enabled) {
        control.setEnabled(new.enabled);
    }
    if old.is_none_or(|o| o.size != new.size) {
        control.setControlSize(match new.size {
            ControlSize::Small => NSControlSize::Small,
            ControlSize::Regular => NSControlSize::Regular,
            ControlSize::Large => NSControlSize::Large,
        });
    }
    let old = old.map(|o| &o.control);
    match (kind, &new.control) {
        (Kind::PopUp(c), Control::PopUp { items, selected }) => {
            let items_changed = !matches!(old, Some(Control::PopUp { items: o, .. }) if o == items);
            if items_changed {
                c.removeAllItems();
                for item in items {
                    c.addItemWithTitle(&NSString::from_str(item));
                }
            }
            select(c.indexOfSelectedItem(), *selected, |i| c.selectItemAtIndex(i));
        }
        (Kind::Segmented(c), Control::Segmented { labels, selected }) => {
            let labels_changed = !matches!(old, Some(Control::Segmented { labels: o, .. }) if o == labels);
            if labels_changed {
                c.setSegmentCount(to_index(labels.len()));
                for (i, label) in labels.iter().enumerate() {
                    c.setLabel_forSegment(&NSString::from_str(label), to_index(i));
                }
            }
            c.setTrackingMode(if selected.is_some() {
                NSSegmentSwitchTracking::SelectOne
            } else {
                NSSegmentSwitchTracking::Momentary
            });
            select(c.selectedSegment(), *selected, |i| c.setSelectedSegment(i));
        }
        (Kind::Slider(c), &Control::Slider { min, max, value }) => {
            c.setMinValue(min);
            c.setMaxValue(max);
            // The app rounds what it is sent; don't fight a drag over that.
            if (c.doubleValue() - value).abs() > 0.01 {
                c.setDoubleValue(value);
            }
        }
        (Kind::Button(c), Control::Button { title, role }) => {
            if !matches!(old, Some(Control::Button { title: o, .. }) if o == title) {
                c.setTitle(&NSString::from_str(title));
            }
            let return_key = if *role == ButtonRole::Default { "\r" } else { "" };
            c.setKeyEquivalent(&NSString::from_str(return_key));
            c.setHasDestructiveAction(*role == ButtonRole::Destructive);
        }
        // `sync` replaces a view whose kind changed before applying.
        _ => {}
    }
}

/// Selects `want` if the control shows something else.
fn select(current: isize, want: Option<usize>, set: impl FnOnce(isize)) {
    if let Some(want) = want.map(to_index).filter(|&w| w != current) {
        set(want);
    }
}

fn to_index(i: usize) -> isize {
    isize::try_from(i).unwrap_or(isize::MAX)
}

/// Places `control` in `rect`, at its own height, centred vertically.
fn place(control: &NSControl, mut rect: CGRect) {
    let fitting = control.fittingSize().height;
    if fitting > 0.0 && fitting < rect.size.height {
        rect.origin.y += ((rect.size.height - fitting) / 2.0).round();
        rect.size.height = fitting;
    }
    if control.frame() != rect {
        control.setFrame(rect);
    }
}

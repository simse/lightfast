//! Automatic updates through Sparkle.
//!
//! `Sparkle.framework` ships in the packaged app's `Contents/Frameworks` and
//! is loaded at runtime, so the executable has no link-time dependency on it.
//! Runs without the framework, such as `cargo run`, have no updater: [`init`]
//! does nothing, [`available`] is false and the menu omits the update item.
//!
//! Sparkle reads its feed URL and signing key from the app's `Info.plist`
//! (`scripts/macos/Info.plist.ext`) and draws all of its own UI.

use gpui::App;

#[cfg(target_os = "macos")]
mod sparkle {
    use gpui::Global;
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{MainThreadMarker, msg_send};
    use objc2_foundation::{NSBundle, NSString};

    /// Owns the `SPUStandardUpdaterController`, which must outlive the app's
    /// run loop for scheduled checks to keep firing.
    pub(super) struct Updater(Retained<AnyObject>);

    impl Global for Updater {}

    impl Updater {
        /// Loads the embedded framework and starts the updater.
        ///
        /// Returns `None` when not running from a bundle containing Sparkle.
        pub(super) fn start() -> Option<Self> {
            // Sparkle's controller may only be used on the main thread.
            MainThreadMarker::new()?;
            let frameworks = NSBundle::mainBundle().privateFrameworksPath()?;
            let path = NSString::from_str(&format!("{frameworks}/Sparkle.framework"));
            let bundle = NSBundle::bundleWithPath(&path)?;
            // SAFETY: the framework is our own signed copy, and loading it
            // runs only Sparkle's Objective-C class registration.
            if !unsafe { bundle.load() } {
                log::warn!("Sparkle.framework is present but failed to load");
                return None;
            }
            let class = AnyClass::get(c"SPUStandardUpdaterController")?;
            // SAFETY: the selectors and argument types match Sparkle's
            // `-initWithStartingUpdater:updaterDelegate:userDriverDelegate:`,
            // where both delegates are nullable, and this is the main thread.
            let controller: Option<Retained<AnyObject>> = unsafe {
                let allocated: Allocated<AnyObject> = msg_send![class, alloc];
                msg_send![
                    allocated,
                    initWithStartingUpdater: true,
                    updaterDelegate: None::<&AnyObject>,
                    userDriverDelegate: None::<&AnyObject>,
                ]
            };
            controller.map(Self)
        }

        /// Starts a user-initiated check, which shows Sparkle's update window.
        pub(super) fn check(&self) {
            // SAFETY: `-checkForUpdates:` is an action method taking a
            // nullable sender. GPUI dispatches actions on the main thread.
            let () = unsafe { msg_send![&*self.0, checkForUpdates: None::<&AnyObject>] };
        }
    }
}

/// Starts the updater if the app was packaged with Sparkle.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    if let Some(updater) = sparkle::Updater::start() {
        cx.set_global(updater);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// Whether the updater is running.
pub fn available(cx: &App) -> bool {
    #[cfg(target_os = "macos")]
    return cx.has_global::<sparkle::Updater>();
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cx;
        false
    }
}

/// Checks for updates now, if the updater is running.
pub fn check(cx: &App) {
    #[cfg(target_os = "macos")]
    if let Some(updater) = cx.try_global::<sparkle::Updater>() {
        updater.check();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

//! Automatic updates: Sparkle on macOS, `LinSparkle` on Linux.
//!
//! Both read the same appcast and accept only updates signed with the same
//! Ed25519 key. Each platform's items carry a `sparkle:os`, so each updater
//! sees only its own.
//!
//! On macOS, `Sparkle.framework` ships in the packaged app's
//! `Contents/Frameworks` and is loaded at runtime, so the executable has no
//! link-time dependency on it. Sparkle reads its feed URL and signing key
//! from the app's `Info.plist` (`macos/Info.plist`) and draws all of its own
//! UI.
//!
//! On Linux, `LinSparkle` draws its UI in a copy of this executable, which
//! branches off in [`run_if_helper`]. It replaces the running `AppImage` in
//! place. The feed URL and key are `linux::FEED_URL` and
//! `linux::PUBLIC_KEY`, copies of the `Info.plist` values.
//!
//! Builds without an updater, such as `cargo run`, have none: [`init`] does
//! nothing, [`available`] is false and the menu omits the update item. On
//! macOS that is any run outside the app bundle; on Linux, any build without
//! `LIGHTFAST_BUILD`, the version the appcast numbers it by.

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

#[cfg(target_os = "linux")]
mod linux {
    use gpui::{App, Global};

    /// `SUFeedURL` in `macos/Info.plist`. `LIGHTFAST_FEED_URL` at build time
    /// replaces it, to test updates against a local feed.
    pub(super) const FEED_URL: &str = match option_env!("LIGHTFAST_FEED_URL") {
        Some(url) => url,
        None => "https://github.com/simse/lightfast/releases/latest/download/appcast.xml",
    };
    /// `SUPublicEDKey` in `macos/Info.plist`. `LIGHTFAST_PUBLIC_KEY` at build
    /// time replaces it, to test updates signed with a throwaway key.
    pub(super) const PUBLIC_KEY: &str = match option_env!("LIGHTFAST_PUBLIC_KEY") {
        Some(key) => key,
        None => "QS5qoA/vpWlcrILWh6o1SifzNgNqvFTep7Uet7AmIy4=",
    };

    /// The build's `sparkle:version`, the UTC timestamp CI also gives the
    /// macOS build as its `CFBundleVersion`.
    const BUILD: Option<&str> = option_env!("LIGHTFAST_BUILD");

    /// Marks `LinSparkle` as configured.
    pub(super) struct Updater;

    impl Global for Updater {}

    impl Updater {
        /// Configures `LinSparkle` and runs a scheduled check if one is due.
        ///
        /// Returns `None` for builds without [`BUILD`].
        pub(super) fn start(cx: &mut App) -> Option<Self> {
            let build = BUILD.filter(|b| !b.is_empty())?;
            linsparkle::set_appcast_url(FEED_URL);
            if !linsparkle::set_eddsa_public_key(PUBLIC_KEY) {
                log::warn!("the update signing key is invalid, so updates are off");
                return None;
            }
            linsparkle::set_app_details("Lightfast", build);
            // Each release's Linux update is one tarball holding an AppImage
            // per architecture, with a manifest naming each by its
            // architecture. Bundle mode installs only this one's, found by
            // that id even if the AppImage was renamed; the others are
            // skipped because they aren't installed next to it.
            linsparkle::set_bundle_mode(true);
            linsparkle::set_bundle_app_id(std::env::consts::ARCH);

            // The callback runs on a LinSparkle thread once the new version
            // is in place and will start when this process exits.
            let (quit_tx, quit_rx) = async_channel::bounded::<()>(1);
            linsparkle::set_shutdown_request_callback(move || {
                let _ = quit_tx.try_send(());
            });
            cx.spawn(async move |cx| {
                if quit_rx.recv().await.is_ok() {
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();
            cx.on_app_quit(|_| {
                linsparkle::cleanup();
                async {}
            })
            .detach();

            // Sparkle's default: check daily in the background.
            linsparkle::set_automatic_check_for_updates(true);
            linsparkle::init();
            Some(Self)
        }
    }
}

/// Runs `LinSparkle`'s update window instead of the app when this process is
/// its helper, then exits. Must come first in `main`.
pub fn run_if_helper() {
    #[cfg(target_os = "linux")]
    linsparkle::run_if_helper();
}

/// Starts the updater if this build has one.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    if let Some(updater) = sparkle::Updater::start() {
        cx.set_global(updater);
    }
    #[cfg(target_os = "linux")]
    if let Some(updater) = linux::Updater::start(cx) {
        cx.set_global(updater);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = cx;
}

/// Whether the updater is running.
pub fn available(cx: &App) -> bool {
    #[cfg(target_os = "macos")]
    return cx.has_global::<sparkle::Updater>();
    #[cfg(target_os = "linux")]
    return cx.has_global::<linux::Updater>();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    if available(cx) {
        linsparkle::check_update_with_ui();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = cx;
}

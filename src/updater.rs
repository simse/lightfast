//! Automatic updates: Sparkle on macOS, `LinSparkle` on Linux, `WinSparkle`
//! on Windows.
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
//! place. The feed URL and key are `feed::FEED_URL` and
//! `feed::PUBLIC_KEY`, copies of the `Info.plist` values.
//!
//! On Windows, `WinSparkle.dll` ships next to the executable in the MSI and
//! is loaded at runtime, like Sparkle. It draws its own UI on its own
//! thread, downloads the new MSI and runs it once this process has quit.
//! It reads the same `feed::FEED_URL` and `feed::PUBLIC_KEY`.
//!
//! Builds without an updater, such as `cargo run`, have none: [`init`] does
//! nothing, [`available`] is false and the menu omits the update item. On
//! macOS that is any run outside the app bundle; on Linux and Windows, any
//! build without `LIGHTFAST_BUILD`, the version the appcast numbers it by,
//! and on Windows also any run without `WinSparkle.dll` beside it.

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

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod feed {
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
    pub(super) const BUILD: Option<&str> = option_env!("LIGHTFAST_BUILD");
}

#[cfg(target_os = "linux")]
mod linux {
    use gpui::{App, Global};

    use super::feed::{BUILD, FEED_URL, PUBLIC_KEY};

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

#[cfg(target_os = "windows")]
mod windows {
    use std::ffi::{CString, c_char, c_int};
    use std::sync::OnceLock;

    use gpui::{App, Global};
    use libloading::Library;

    use super::feed::{BUILD, FEED_URL, PUBLIC_KEY};

    type SetStr = unsafe extern "C" fn(*const c_char);
    type SetKey = unsafe extern "C" fn(*const c_char) -> c_int;
    type SetDetails = unsafe extern "C" fn(*const u16, *const u16, *const u16);
    type SetWide = unsafe extern "C" fn(*const u16);
    type SetInt = unsafe extern "C" fn(c_int);
    type SetCallback = unsafe extern "C" fn(extern "C" fn());
    type SetCanShutdown = unsafe extern "C" fn(extern "C" fn() -> c_int);
    type Call = unsafe extern "C" fn();

    /// Where `WinSparkle` keeps its settings, under `HKEY_CURRENT_USER`.
    const REGISTRY_PATH: &core::ffi::CStr = c"Software\\Lightfast\\WinSparkle";

    /// Wakes the app to quit when `WinSparkle` is ready to run an installer.
    static QUIT: OnceLock<async_channel::Sender<()>> = OnceLock::new();

    /// Called on a `WinSparkle` thread before it asks the app to quit. Nothing
    /// in Lightfast needs saving, so the answer is always yes.
    extern "C" fn can_shutdown() -> c_int {
        1
    }

    /// Called on a `WinSparkle` thread once the installer is downloaded. It
    /// starts the installer when this process exits.
    extern "C" fn shutdown_request() {
        if let Some(quit) = QUIT.get() {
            let _ = quit.try_send(());
        }
    }

    /// A NUL-terminated UTF-16 copy of `s`, as `WinSparkle`'s `wchar_t` APIs take.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Keeps `WinSparkle.dll` loaded for as long as the app runs.
    pub(super) struct Updater {
        check: Call,
        _library: Library,
    }

    impl Global for Updater {}

    impl Updater {
        /// Loads `WinSparkle.dll` from beside the executable, configures it and
        /// starts its scheduled checks.
        ///
        /// Returns `None` for builds without [`BUILD`] and installs without
        /// the DLL.
        pub(super) fn start(cx: &mut App) -> Option<Self> {
            let build = BUILD.filter(|b| !b.is_empty())?;
            let path = std::env::current_exe().ok()?.with_file_name("WinSparkle.dll");
            if !path.exists() {
                return None;
            }
            // SAFETY: the DLL is the pinned WinSparkle the MSI installs next
            // to the executable; loading it runs only its C runtime's setup.
            let library = match unsafe { Library::new(&path) } {
                Ok(library) => library,
                Err(err) => {
                    log::warn!(err:display; "WinSparkle.dll failed to load, so updates are off");
                    return None;
                }
            };
            // SAFETY: each type matches the function's declaration in
            // WinSparkle 0.9's `winsparkle.h`, all `__cdecl` (the only
            // convention on x64 and ARM64). The strings outlive each call,
            // which copies them, and every call precedes `win_sparkle_init`
            // as the header requires.
            let (init, cleanup, check) = unsafe {
                let appcast_url = *library.get::<SetStr>(b"win_sparkle_set_appcast_url\0").ok()?;
                let eddsa_key = *library.get::<SetKey>(b"win_sparkle_set_eddsa_public_key\0").ok()?;
                let app_details = *library.get::<SetDetails>(b"win_sparkle_set_app_details\0").ok()?;
                let build_version = *library.get::<SetWide>(b"win_sparkle_set_app_build_version\0").ok()?;
                let registry_path = *library.get::<SetStr>(b"win_sparkle_set_registry_path\0").ok()?;
                let automatic = *library.get::<SetInt>(b"win_sparkle_set_automatic_check_for_updates\0").ok()?;
                let can_shutdown_callback =
                    *library.get::<SetCanShutdown>(b"win_sparkle_set_can_shutdown_callback\0").ok()?;
                let shutdown_callback =
                    *library.get::<SetCallback>(b"win_sparkle_set_shutdown_request_callback\0").ok()?;
                let init = *library.get::<Call>(b"win_sparkle_init\0").ok()?;
                let cleanup = *library.get::<Call>(b"win_sparkle_cleanup\0").ok()?;
                let check = *library.get::<Call>(b"win_sparkle_check_update_with_ui\0").ok()?;

                let url = CString::new(FEED_URL).ok()?;
                appcast_url(url.as_ptr());
                let key = CString::new(PUBLIC_KEY).ok()?;
                if eddsa_key(key.as_ptr()) == 0 {
                    log::warn!("the update signing key is invalid, so updates are off");
                    return None;
                }
                // The company and app name only matter for the registry path,
                // which is set explicitly. The version is what WinSparkle
                // shows; the build is what it compares to `sparkle:version`.
                let (company, name, version) = (wide("simse"), wide("Lightfast"), wide(env!("CARGO_PKG_VERSION")));
                app_details(company.as_ptr(), name.as_ptr(), version.as_ptr());
                let build = wide(build);
                build_version(build.as_ptr());
                registry_path(REGISTRY_PATH.as_ptr());
                can_shutdown_callback(can_shutdown);
                shutdown_callback(shutdown_request);
                // Sparkle's default: check daily in the background.
                automatic(1);

                (init, cleanup, check)
            };

            let (quit_tx, quit_rx) = async_channel::bounded::<()>(1);
            QUIT.set(quit_tx).ok()?;
            cx.spawn(async move |cx| {
                if quit_rx.recv().await.is_ok() {
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();
            cx.on_app_quit(move |_| {
                // SAFETY: `win_sparkle_cleanup` takes no arguments, and the
                // library stays loaded in the `Updater` global until the
                // process exits.
                unsafe { cleanup() };
                async {}
            })
            .detach();
            // SAFETY: configuration is complete, and `win_sparkle_init` takes
            // no arguments and returns at once, checking on its own thread.
            unsafe { init() };
            Some(Self { check, _library: library })
        }

        /// Starts a user-initiated check, which shows `WinSparkle`'s window.
        pub(super) fn check(&self) {
            // SAFETY: `win_sparkle_check_update_with_ui` takes no arguments,
            // may be called from any thread after `win_sparkle_init`, and the
            // library is still loaded since `self` holds it.
            unsafe { (self.check)() };
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
    #[cfg(target_os = "windows")]
    if let Some(updater) = windows::Updater::start(cx) {
        cx.set_global(updater);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = cx;
}

/// Whether the updater is running.
pub fn available(cx: &App) -> bool {
    #[cfg(target_os = "macos")]
    return cx.has_global::<sparkle::Updater>();
    #[cfg(target_os = "linux")]
    return cx.has_global::<linux::Updater>();
    #[cfg(target_os = "windows")]
    return cx.has_global::<windows::Updater>();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    #[cfg(target_os = "windows")]
    if let Some(updater) = cx.try_global::<windows::Updater>() {
        updater.check();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = cx;
}

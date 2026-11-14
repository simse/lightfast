//! Lightfast: a flatbed photo/art scanner.

mod ui;
mod updater;

/// mimalloc handles the large, short-lived row batches of multi-gigabyte
/// scans better than the system allocator (M-MIMALLOC-APPS).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use gpui::{
    AppContext as _, Bounds, KeyBinding, Styled as _, WindowBackgroundAppearance, WindowBounds, WindowOptions, point,
    px, size, transparent_black,
};
use gpui_component::{Root, TitleBar};

/// A shortcut with the platform's command modifier: Cmd on macOS, Ctrl
/// elsewhere. GPUI's own `secondary-` only parses in keymap files.
macro_rules! secondary {
    ($key:literal) => {
        if cfg!(target_os = "macos") { concat!("cmd-", $key) } else { concat!("ctrl-", $key) }
    };
}

/// The macOS bundle identifier, also the Linux desktop entry's name.
const APP_ID: &str = "io.simse.lightfast";

fn main() {
    // Before anything else: on Linux this process may be the updater's
    // helper, which shows its own window and exits.
    updater::run_if_helper();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).init();
    log::info!(version = env!("CARGO_PKG_VERSION"); "starting Lightfast");
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(|cx| {
        gpui_component::init(cx);
        ui::glass::init(cx);
        ui::theme::init(cx);
        cx.bind_keys([
            KeyBinding::new(secondary!("="), ui::ZoomIn, None),
            KeyBinding::new(secondary!("+"), ui::ZoomIn, None),
            KeyBinding::new(secondary!("-"), ui::ZoomOut, None),
            KeyBinding::new(secondary!("0"), ui::ZoomToFit, None),
            KeyBinding::new(secondary!("1"), ui::SingleAreaMode, None),
            KeyBinding::new(secondary!("2"), ui::MultipleAreaMode, None),
            KeyBinding::new("backspace", ui::DeleteFrame, None),
            KeyBinding::new("delete", ui::DeleteFrame, None),
            KeyBinding::new(secondary!("q"), ui::Quit, None),
        ]);
        cx.on_action(|_: &ui::Quit, cx| cx.quit());
        cx.on_action(|_: &ui::CheckForUpdates, cx| updater::check(cx));
        updater::init(cx);
        ui::menu::set(cx, false);
        let bounds = Bounds::centered(None, size(px(1100.0), px(820.0)), cx);
        let glass = ui::glass::active(cx);
        let mut options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Wayland matches the window to `linux/io.simse.lightfast.desktop`
            // by this, for its icon and name.
            app_id: Some(APP_ID.into()),
            ..TitleBar::window_options()
        };
        if glass {
            // Transparent from the first frame, with the traffic lights
            // already inside the floating sidebar.
            options.window_background = WindowBackgroundAppearance::Transparent;
            if let Some(titlebar) = options.titlebar.as_mut() {
                let (x, y) = ui::glass::TRAFFIC_LIGHTS;
                titlebar.traffic_light_position = Some(point(px(x), px(y)));
            }
        }
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| ui::AppView::new(window, cx));
            // On glass the native backdrop paints the window, so the Root
            // plugin's theme background must not cover it.
            cx.new(|cx| {
                let root = Root::new(view, window, cx);
                if glass { root.bg(transparent_black()) } else { root }
            })
        })
        .expect("the main window should open");
        cx.activate(true);
    });
}

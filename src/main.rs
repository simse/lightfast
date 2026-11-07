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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!(version = env!("CARGO_PKG_VERSION"); "starting Lightfast");
    gpui_platform::application().run(|cx| {
        gpui_component::init(cx);
        ui::glass::init(cx);
        ui::theme::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-=", ui::ZoomIn, None),
            KeyBinding::new("cmd-+", ui::ZoomIn, None),
            KeyBinding::new("cmd--", ui::ZoomOut, None),
            KeyBinding::new("cmd-0", ui::ZoomToFit, None),
            KeyBinding::new("cmd-1", ui::SingleAreaMode, None),
            KeyBinding::new("cmd-2", ui::MultipleAreaMode, None),
            KeyBinding::new("backspace", ui::DeleteFrame, None),
            KeyBinding::new("delete", ui::DeleteFrame, None),
            KeyBinding::new("cmd-q", ui::Quit, None),
        ]);
        cx.on_action(|_: &ui::Quit, cx| cx.quit());
        cx.on_action(|_: &ui::CheckForUpdates, cx| updater::check(cx));
        updater::init(cx);
        ui::menu::set(cx, false);
        let bounds = Bounds::centered(None, size(px(1100.0), px(820.0)), cx);
        let glass = ui::glass::active(cx);
        let mut options =
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..TitleBar::window_options() };
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

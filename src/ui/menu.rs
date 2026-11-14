//! The menu bar: the system's on macOS, drawn in the title bar elsewhere.

use gpui::{App, Menu, MenuItem};

use super::app_view::{CheckForUpdates, MultipleAreaMode, Quit, SingleAreaMode, ZoomIn, ZoomOut, ZoomToFit};
use crate::updater;

/// Installs the menu bar, checking the item for the current scan mode.
///
/// Called at startup and again on every mode switch, since a menu item's
/// check mark is fixed when the menu is built.
pub fn set(cx: &mut App, multi: bool) {
    let mut app_items = Vec::with_capacity(3);
    if updater::available(cx) {
        app_items.push(MenuItem::action("Check for Updates…", CheckForUpdates));
        app_items.push(MenuItem::separator());
    }
    app_items.push(MenuItem::action("Quit Lightfast", Quit));
    cx.set_menus([
        Menu { name: "Lightfast".into(), items: app_items, disabled: false },
        Menu {
            name: "Scan".into(),
            items: vec![
                MenuItem::action("Single Area", SingleAreaMode).checked(!multi),
                MenuItem::action("Multiple Areas", MultipleAreaMode).checked(multi),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Zoom to Fit", ZoomToFit),
            ],
            disabled: false,
        },
    ]);
    #[cfg(not(target_os = "macos"))]
    in_window::reload(cx);
}

#[cfg(not(target_os = "macos"))]
pub use in_window::bar;

/// GPUI has no global menu bar outside macOS: it only keeps the menus, which
/// the component library's `AppMenuBar` draws in the window's title bar.
#[cfg(not(target_os = "macos"))]
mod in_window {
    use gpui::{App, Entity, Global};
    use gpui_component::GlobalState;
    use gpui_component::menu::AppMenuBar;

    /// The one window's menu bar.
    struct Bar(Entity<AppMenuBar>);

    impl Global for Bar {}

    /// The menu bar to put in the title bar, showing the menus last [`set`](super::set).
    pub fn bar(cx: &mut App) -> Entity<AppMenuBar> {
        if let Some(Bar(bar)) = cx.try_global::<Bar>() {
            return bar.clone();
        }
        let bar = AppMenuBar::new(cx);
        cx.set_global(Bar(bar.clone()));
        bar
    }

    /// Hands the menus GPUI keeps to the menu bar.
    pub(super) fn reload(cx: &mut App) {
        let menus = cx.get_menus().unwrap_or_default();
        GlobalState::global_mut(cx).set_app_menus(menus);
        if let Some(Bar(bar)) = cx.try_global::<Bar>() {
            bar.clone().update(cx, AppMenuBar::reload);
        }
    }
}

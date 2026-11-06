//! The macOS menu bar.

use gpui::{App, Menu, MenuItem};

use super::app_view::{CheckForUpdates, MultipleAreaMode, Quit, SingleAreaMode, ZoomIn, ZoomOut, ZoomToFit};
use crate::updater;

/// Installs the menu bar, checking the item for the current scan mode.
///
/// Called at startup and again on every mode switch, since a menu item's
/// check mark is fixed when the menu is built.
pub fn set(cx: &App, multi: bool) {
    let mut app_items = Vec::with_capacity(3);
    if updater::available(cx) {
        app_items.push(MenuItem::action("Check for Updates…", CheckForUpdates));
        app_items.push(MenuItem::separator());
    }
    app_items.push(MenuItem::action("Quit ScanApp", Quit));
    cx.set_menus([
        Menu { name: "ScanApp".into(), items: app_items, disabled: false },
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
}

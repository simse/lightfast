//! Zed/Delta-inspired dark palette; the app is always dark.

use std::rc::Rc;

use gpui::{App, Hsla};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};

const THEMES: &str = include_str!("../../assets/themes/scanapp.json");
const DARK: &str = "ScanApp Dark";

/// Installs the `ScanApp` dark palette and switches to it.
///
/// The dark palette is installed for both modes, so a macOS appearance change
/// can never switch the app to light.
///
/// # Panics
/// If the bundled theme file is malformed or lacks the dark palette; that is
/// a build-time bug, caught on first launch.
pub fn init(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry.load_themes_from_str(THEMES).expect("bundled theme JSON should parse");
    let dark = registry.themes().get(DARK).unwrap_or_else(|| panic!("bundled themes should include '{DARK}'"));
    let dark = Rc::clone(dark);
    Theme::update(cx, |t| {
        t.light_theme = Rc::clone(&dark);
        t.dark_theme = dark;
    });
    Theme::change(ThemeMode::Dark, None, cx);
    super::glass::adapt_theme(cx);
}

/// Colours for the preview canvas, which sits a shade apart from the panels.
///
/// On Liquid Glass, `backdrop` is the native fill behind the whole window.
#[derive(Debug, Clone, Copy)]
pub struct CanvasColors {
    pub backdrop: Hsla,
    pub bed: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    /// Text on an `accent` fill, such as the frame numbers.
    pub label: Hsla,
    pub dim: Hsla,
}

impl CanvasColors {
    /// Picks canvas colours to suit `theme`.
    pub fn new(theme: &Theme) -> Self {
        Self {
            backdrop: gpui::rgb(0x0014_1517).into(),
            bed: theme.muted,
            border: theme.border,
            accent: theme.primary,
            label: theme.primary_foreground,
            dim: gpui::black().opacity(0.55),
        }
    }
}

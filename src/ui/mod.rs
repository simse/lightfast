//! GPUI front end.

mod app_view;
pub mod glass;
pub mod menu;
mod preview;
pub mod theme;

pub use app_view::{
    AppView, CheckForUpdates, DeleteFrame, MultipleAreaMode, Quit, SingleAreaMode, ZoomIn, ZoomOut, ZoomToFit,
};

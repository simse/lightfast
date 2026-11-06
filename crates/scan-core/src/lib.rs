//! The scanner API every backend implements.
//!
//! This replaces SANE's dynamic, string-keyed option model with typed
//! settings for what every flatbed has (source, mode, resolution, area,
//! gamma), plus a small list of named [`ExtraOption`]s for the long tail of
//! backend-specific knobs.
//!
//! A scan is pull-based, like SANE: [`Device::start`] returns the frame's
//! [`Parameters`], [`Device::read`] streams image bytes until it returns 0,
//! and [`Device::finish`] ends the frame (always, even after an error).
//! Image bytes use SANE's layout: rows of packed samples, 16-bit samples in
//! host byte order, lineart 1 bit per pixel with 1 = black, MSB first.

#![forbid(unsafe_code)]

pub mod driver;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Millimetres per inch.
pub const MM_PER_INCH: f64 = 25.4;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The operation was cancelled through a [`CancelToken`].
    Cancelled,
    /// The device is busy, e.g. still finishing a previous scan.
    Busy,
    /// The settings or arguments are invalid for this device.
    Invalid,
    /// The device didn't answer in time.
    Timeout,
    /// USB or other transport failure.
    Io,
    /// The device answered with something the driver doesn't understand.
    Protocol,
    /// The device reported an error condition of its own.
    Device,
    /// No such device, or it was unplugged.
    NotFound,
    /// The OS or another program denied access to the device.
    Access,
}

/// An error from a scanner backend.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Creates an error of `kind` with a message for the user.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    /// A cancellation error.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "scan cancelled")
    }

    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }

    /// Prefixes the message with what was being done.
    #[must_use]
    pub fn context(mut self, what: &str) -> Self {
        self.message = format!("{what}: {}", self.message);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Result type for scanner operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A shared flag that asks a running scan to stop.
///
/// Cheap to clone; all clones share the flag. Backends check it between
/// transfers and while waiting on the device.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Clears a cancellation request, for reuse.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Returns [`ErrorKind::Cancelled`] if cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::cancelled()) } else { Ok(()) }
    }
}

/// Identifies one attached scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable name, unique among attached devices, e.g.
    /// `pixma:04A91912_508624`. Pass it to the registry to open the device.
    pub name: String,
    pub vendor: String,
    pub model: String,
}

/// The kind of a scan source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Flatbed,
    /// A sheet feeder: every scan uses up a page.
    Feeder,
    /// A film or slide adapter.
    Transparency,
}

/// Samples per pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Gray,
    Rgb,
}

impl Channels {
    #[must_use]
    pub fn count(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }
}

/// The pixel layout of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLayout {
    pub channels: Channels,
    /// Bits per sample: 1 (lineart), 8 or 16.
    pub depth: u32,
}

/// One scan mode a source offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeCaps {
    /// The backend's name for the mode, e.g. `Color` or `48 bits color`.
    pub name: String,
    pub layout: PixelLayout,
    /// Supported resolutions in DPI, ascending.
    pub resolutions: Vec<u32>,
}

/// A numeric range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}

/// The values a backend-specific option accepts.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionKind {
    /// One of a list of names.
    Choice(Vec<String>),
    /// An integer in a range.
    Int { min: i32, max: i32 },
}

/// The value of a backend-specific option.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    Choice(String),
    Int(i32),
}

/// A backend-specific option, e.g. pixma's `calibrate`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraOption {
    pub name: String,
    pub title: String,
    pub kind: OptionKind,
    pub default: OptionValue,
}

/// What one scan source offers.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceCaps {
    /// The backend's name for the source, e.g. `Flatbed`.
    pub name: String,
    pub kind: SourceKind,
    /// Largest scan width in millimetres.
    pub width_mm: f64,
    /// Largest scan height in millimetres.
    pub height_mm: f64,
    /// Never empty.
    pub modes: Vec<ModeCaps>,
    /// Accepted gamma values and the default, if gamma is adjustable.
    pub gamma: Option<(Range, f64)>,
    pub extras: Vec<ExtraOption>,
}

impl SourceCaps {
    #[must_use]
    pub fn mode(&self, name: &str) -> Option<&ModeCaps> {
        self.modes.iter().find(|m| m.name == name)
    }
}

/// Everything a device offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    /// Never empty; the first source is the default.
    pub sources: Vec<SourceCaps>,
}

impl Capabilities {
    #[must_use]
    pub fn source(&self, name: &str) -> Option<&SourceCaps> {
        self.sources.iter().find(|s| s.name == name)
    }
}

/// A rectangle on the scan surface, in millimetres from the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// How to scan one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanSettings {
    /// A [`SourceCaps::name`].
    pub source: String,
    /// A [`ModeCaps::name`] of that source.
    pub mode: String,
    /// One of the mode's resolutions.
    pub resolution: u32,
    pub area: Area,
    /// `None` uses the source's default gamma.
    pub gamma: Option<f64>,
    /// Values for [`SourceCaps::extras`]; unset ones use their defaults.
    pub extras: Vec<(String, OptionValue)>,
}

impl ScanSettings {
    /// The value set for extra option `name`, if any.
    #[must_use]
    pub fn extra(&self, name: &str) -> Option<&OptionValue> {
        self.extras.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

/// Geometry and pixel layout of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parameters {
    pub layout: PixelLayout,
    pub pixels_per_line: usize,
    /// Bytes per row, including any padding.
    pub bytes_per_line: usize,
    /// Rows in the frame. For scanners that only know a page's length once it
    /// has passed (SANE's `lines = -1`, typical of sheet feeders), this is an
    /// upper bound and [`Device::read`] may end the frame sooner.
    pub lines: usize,
}

/// An open scanner.
pub trait Device: Send + fmt::Debug {
    fn info(&self) -> &DeviceInfo;

    fn capabilities(&self) -> &Capabilities;

    /// The frame `settings` would produce, without touching the device.
    ///
    /// # Errors
    /// [`ErrorKind::Invalid`] if the settings don't fit the device.
    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters>;

    /// Starts a frame. `cancel` is checked for the rest of the frame.
    ///
    /// # Errors
    /// Invalid settings, a busy or failing device, or cancellation.
    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters>;

    /// Reads image bytes into `buf`; returns 0 at the end of the frame.
    ///
    /// # Errors
    /// Transfer or device errors, or cancellation.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Ends the current frame, stopping the scan if it is still running and
    /// returning the device to idle. Does nothing when idle.
    fn finish(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_is_shared() {
        let t = CancelToken::new();
        let clone = t.clone();
        assert!(t.check().is_ok());
        clone.cancel();
        assert!(t.check().unwrap_err().is_cancelled());
        t.reset();
        assert!(!clone.is_cancelled());
    }

    #[test]
    fn later_extras_win() {
        let s = ScanSettings {
            source: "Flatbed".into(),
            mode: "Color".into(),
            resolution: 75,
            area: Area { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            gamma: None,
            extras: vec![("a".into(), OptionValue::Int(1)), ("a".into(), OptionValue::Int(2))],
        };
        assert_eq!(s.extra("a"), Some(&OptionValue::Int(2)));
        assert_eq!(s.extra("b"), None);
    }

    #[test]
    fn errors_carry_context() {
        let e = Error::new(ErrorKind::Busy, "device busy").context("starting scan");
        assert_eq!(e.to_string(), "starting scan: device busy");
        assert_eq!(e.kind(), ErrorKind::Busy);
    }
}

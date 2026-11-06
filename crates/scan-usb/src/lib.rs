//! USB transport for scanner drivers.
//!
//! Drivers talk to hardware only through [`UsbTransport`], so the same driver
//! code runs against a real device ([`NusbTransport`], feature `hardware`) or
//! against a USB session recorded by C SANE ([`Replay`]). Replay is what lets
//! drivers be tested against scanners nobody here owns.
//!
//! The transfer model is the one SANE's `sanei_usb` exposes, since that is
//! what the recordings capture: one bulk-out, one bulk-in and one interrupt-in
//! endpoint on the device's first interface, plus control transfers for
//! descriptors.

#![forbid(unsafe_code)]

#[cfg(feature = "hardware")]
mod hardware;
mod replay;

use std::fmt;
use std::time::Duration;

#[cfg(feature = "hardware")]
pub use hardware::{NusbTransport, UsbDeviceInfo, list_devices};
pub use replay::{Replay, ReplayError};

/// A USB transfer failure.
#[derive(Debug)]
pub enum Error {
    /// The transfer didn't complete within its timeout.
    Timeout,
    /// The device was unplugged.
    Disconnected,
    /// The endpoint stalled.
    Stall,
    /// The OS refused access, e.g. because another driver holds the device.
    Access(String),
    /// No device matched.
    NotFound,
    /// Any other OS or transfer error.
    Io(String),
    /// A replayed session diverged from the recording.
    Replay(ReplayError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("USB transfer timed out"),
            Self::Disconnected => f.write_str("scanner was disconnected"),
            Self::Stall => f.write_str("USB endpoint stalled"),
            Self::Access(m) => write!(f, "can't access the scanner: {m}"),
            Self::NotFound => f.write_str("scanner not found"),
            Self::Io(m) => write!(f, "USB error: {m}"),
            Self::Replay(e) => write!(f, "replay: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Result type for transfers.
pub type Result<T> = std::result::Result<T, Error>;

/// Identity of the device behind a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIds {
    pub vendor_id: u16,
    pub product_id: u16,
    /// The serial number string descriptor, if the device has one.
    pub serial: Option<String>,
}

/// Blocking transfers on one claimed USB interface.
///
/// Bulk and interrupt reads may return fewer bytes than `buf` holds (a short
/// packet ends the transfer), never more.
pub trait UsbTransport: Send + fmt::Debug {
    /// Vendor, product and serial number of the device.
    fn ids(&self) -> &DeviceIds;

    /// Sends `data` on the bulk-out endpoint; returns the bytes written.
    ///
    /// # Errors
    /// Fails on timeout or any transfer error.
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<usize>;

    /// Reads from the bulk-in endpoint into `buf`; returns the bytes read.
    ///
    /// # Errors
    /// Fails on timeout or any transfer error.
    fn bulk_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;

    /// Reads one interrupt packet into `buf`; returns its length.
    ///
    /// # Errors
    /// [`Error::Timeout`] if no packet arrived in time.
    fn interrupt_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;

    /// Waits `duration`, for drivers that pace the device. Replay skips it.
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

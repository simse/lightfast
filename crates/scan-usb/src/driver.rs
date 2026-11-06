//! How a USB backend presents itself to the registry.

use scan_core::{Device, DeviceInfo, Result};

use crate::{DeviceIds, UsbTransport};

/// A scanner model a backend recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbModel {
    pub vendor: &'static str,
    pub model: &'static str,
}

/// A USB backend: the Rust counterpart of a SANE backend's device table and
/// `sane_open`.
#[derive(Debug, Clone, Copy)]
pub struct UsbDriver {
    /// The backend's name, used as the device name prefix, e.g. `hpljm1005`.
    pub name: &'static str,
    /// Recognises a vendor and product ID.
    pub find: fn(u16, u16) -> Option<UsbModel>,
    /// Opens a device `find` recognised.
    pub open: fn(Box<dyn UsbTransport>) -> Result<Box<dyn Device>>,
    /// Which interface to claim.
    pub interface: u8,
}

impl UsbDriver {
    /// SANE-style device name: `<backend>:<VID><PID>[_<serial>]`.
    #[must_use]
    pub fn device_name(&self, vid: u16, pid: u16, serial: Option<&str>) -> String {
        let mut name = format!("{}:{vid:04X}{pid:04X}", self.name);
        if let Some(serial) = serial {
            name.push('_');
            name.push_str(serial);
        }
        name
    }

    /// The [`DeviceInfo`] for an open device. An unrecognised ID (which the
    /// registry never opens) gets `Unknown` as its vendor and model.
    #[must_use]
    pub fn device_info(&self, ids: &DeviceIds) -> DeviceInfo {
        let model = (self.find)(ids.vendor_id, ids.product_id);
        DeviceInfo {
            name: self.device_name(ids.vendor_id, ids.product_id, ids.serial.as_deref()),
            vendor: model.map_or("Unknown", |m| m.vendor).into(),
            model: model.map_or("Unknown", |m| m.model).into(),
        }
    }
}

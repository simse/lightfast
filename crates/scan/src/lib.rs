//! Finds attached scanners and opens them with the right backend.
//!
//! This is the Rust replacement for SANE's `dll` meta-backend: a fixed list
//! of backends, each recognising devices by USB vendor and product ID.
//!
//! Besides real devices, a name of the form `replay:<path>` opens a SANE USB
//! recording (see `scan_usb::Replay`) as if it were the scanner, which lets
//! a frontend be exercised without hardware. [`list_devices`] includes one
//! such device when the `SCAN_REPLAY` environment variable names a file.
//!
//! Backends whose C source has no SANE linking exception are only compiled
//! with the `gpl-backends` feature, because linking them makes the whole
//! program GPL. See the README's driver table.

#![forbid(unsafe_code)]

use scan_core::driver::NetDriver;
pub use scan_core::*;
use scan_usb::{UsbDriver, UsbTransport as _};

mod drivers;

/// Name prefix for recorded sessions opened as devices.
pub const REPLAY_PREFIX: &str = "replay:";

/// Environment variable naming a recording to list as a device.
pub const REPLAY_ENV: &str = "SCAN_REPLAY";

/// The compiled-in USB backends, in matching order: the first to recognise a
/// vendor and product ID drives the device, like `dll.conf` order in SANE.
#[must_use]
pub fn usb_drivers() -> &'static [UsbDriver] {
    drivers::USB
}

/// The compiled-in network backends.
#[must_use]
pub fn net_drivers() -> &'static [NetDriver] {
    drivers::NET
}

fn find_driver(vid: u16, pid: u16) -> Option<(&'static UsbDriver, scan_usb::UsbModel)> {
    usb_drivers().iter().find_map(|d| (d.find)(vid, pid).map(|m| (d, m)))
}

/// Attached USB devices some backend supports, with their device names.
fn supported_usb() -> Result<Vec<(DeviceInfo, &'static UsbDriver, scan_usb::UsbDeviceInfo)>> {
    let devices = scan_usb::list_devices()?;
    Ok(devices
        .into_iter()
        .filter_map(|d| {
            let ids = &d.ids;
            let (driver, model) = find_driver(ids.vendor_id, ids.product_id)?;
            let info = DeviceInfo {
                name: driver.device_name(ids.vendor_id, ids.product_id, ids.serial.as_deref()),
                vendor: model.vendor.into(),
                model: model.model.into(),
            };
            Some((info, driver, d))
        })
        .collect())
}

/// Lists attached scanners, plus the `SCAN_REPLAY` recording if set.
///
/// # Errors
/// Fails if USB devices can't be enumerated.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out: Vec<DeviceInfo> = supported_usb()?.into_iter().map(|(info, ..)| info).collect();
    out.extend(net_drivers().iter().flat_map(|d| (d.discover)()));
    if let Some(path) = std::env::var_os(REPLAY_ENV) {
        let path = path.to_string_lossy();
        out.push(DeviceInfo {
            name: format!("{REPLAY_PREFIX}{path}"),
            vendor: "Replay".into(),
            model: path.into_owned(),
        });
    }
    Ok(out)
}

/// Opens the scanner named `name`, as returned by [`list_devices`].
///
/// # Errors
/// [`ErrorKind::NotFound`] if no such scanner is attached; otherwise the
/// backend's error from opening it.
pub fn open(name: &str) -> Result<Box<dyn Device>> {
    if let Some(path) = name.strip_prefix(REPLAY_PREFIX) {
        let replay = scan_usb::Replay::open(path)?;
        let ids = replay.ids().clone();
        let (driver, _) = find_driver(ids.vendor_id, ids.product_id).ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("no backend for recorded device {:04x}:{:04x}", ids.vendor_id, ids.product_id),
            )
        })?;
        return (driver.open)(Box::new(replay));
    }
    for driver in net_drivers() {
        if let Some(address) = name.strip_prefix(driver.name).and_then(|n| n.strip_prefix(':')) {
            return (driver.open)(address);
        }
    }
    let (_, driver, usb) = supported_usb()?
        .into_iter()
        .find(|(info, ..)| info.name == name)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("scanner {name} is not connected")))?;
    let transport =
        scan_usb::NusbTransport::open(&usb, driver.interface).map_err(|e| Error::from(e).context("opening scanner"))?;
    (driver.open)(Box::new(transport))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_are_unique() {
        let mut names: Vec<_> = usb_drivers().iter().map(|d| d.name).collect();
        names.extend(net_drivers().iter().map(|d| d.name));
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total);
    }

    #[test]
    fn finds_the_lide_400() {
        let (driver, model) = find_driver(0x04a9, 0x1912).unwrap();
        assert_eq!(driver.name, "pixma");
        assert_eq!(model.model, "CanoScan LiDE 400");
    }

    #[test]
    fn unknown_devices_have_no_driver() {
        assert!(find_driver(0xffff, 0xffff).is_none());
    }
}

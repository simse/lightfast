//! Finds attached scanners and opens them with the right backend.
//!
//! This is the Rust replacement for SANE's `dll` meta-backend: a fixed list
//! of backends, each recognising devices by USB vendor and product ID.
//!
//! Besides real devices, a name of the form `replay:<path>` opens a SANE USB
//! recording (see `scan_usb::Replay`) as if it were the scanner, which lets
//! a frontend be exercised without hardware. [`list_devices`] includes one
//! such device when the `SCAN_REPLAY` environment variable names a file.

#![forbid(unsafe_code)]

pub use scan_core::*;

/// Name prefix for recorded sessions opened as devices.
pub const REPLAY_PREFIX: &str = "replay:";

/// Environment variable naming a recording to list as a device.
pub const REPLAY_ENV: &str = "SCAN_REPLAY";

fn usb_error(e: &scan_usb::Error) -> Error {
    let kind = match e {
        scan_usb::Error::NotFound | scan_usb::Error::Disconnected => ErrorKind::NotFound,
        scan_usb::Error::Access(_) => ErrorKind::Access,
        scan_usb::Error::Timeout => ErrorKind::Timeout,
        _ => ErrorKind::Io,
    };
    Error::new(kind, e.to_string())
}

/// Attached USB devices some backend supports, with their device names.
fn supported_usb() -> Result<Vec<(DeviceInfo, scan_usb::UsbDeviceInfo)>> {
    let devices = scan_usb::list_devices().map_err(|e| usb_error(&e))?;
    Ok(devices
        .into_iter()
        .filter_map(|d| {
            let ids = &d.ids;
            let model = scan_pixma::models::find(ids.vendor_id, ids.product_id)?;
            let info = DeviceInfo {
                name: scan_pixma::device_name(ids.vendor_id, ids.product_id, ids.serial.as_deref()),
                vendor: "CANON".into(),
                model: model.name.into(),
            };
            Some((info, d))
        })
        .collect())
}

/// Lists attached scanners, plus the `SCAN_REPLAY` recording if set.
///
/// # Errors
/// Fails if USB devices can't be enumerated.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out: Vec<DeviceInfo> = supported_usb()?.into_iter().map(|(info, _)| info).collect();
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
        let replay = scan_usb::Replay::open(path).map_err(|e| usb_error(&e))?;
        return Ok(Box::new(scan_pixma::Pixma::open(Box::new(replay))?));
    }
    let (_, usb) = supported_usb()?
        .into_iter()
        .find(|(info, _)| info.name == name)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("scanner {name} is not connected")))?;
    let transport = scan_usb::NusbTransport::open(&usb, 0).map_err(|e| usb_error(&e).context("opening scanner"))?;
    Ok(Box::new(scan_pixma::Pixma::open(Box::new(transport))?))
}

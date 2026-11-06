//! Real USB devices through `nusb`.

use std::time::Duration;

use nusb::MaybeFuture as _;
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::{Endpoint, Interface};

use crate::{DeviceIds, Error, Result, UsbTransport};

/// An attached USB device, as found by [`list_devices`].
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub ids: DeviceIds,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    info: nusb::DeviceInfo,
}

/// Lists attached USB devices. Uses the OS's cached descriptors, so it
/// doesn't disturb devices that are busy.
///
/// # Errors
/// Fails if the OS can't enumerate devices.
pub fn list_devices() -> Result<Vec<UsbDeviceInfo>> {
    let devices = nusb::list_devices().wait().map_err(|e| Error::Io(e.to_string()))?;
    Ok(devices
        .map(|info| UsbDeviceInfo {
            ids: DeviceIds {
                vendor_id: info.vendor_id(),
                product_id: info.product_id(),
                serial: info.serial_number().map(str::to_owned),
            },
            manufacturer: info.manufacturer_string().map(str::to_owned),
            product: info.product_string().map(str::to_owned),
            info,
        })
        .collect())
}

fn map_os_error(e: &nusb::Error) -> Error {
    match e.kind() {
        nusb::ErrorKind::Disconnected => Error::Disconnected,
        nusb::ErrorKind::NotFound => Error::NotFound,
        nusb::ErrorKind::Busy | nusb::ErrorKind::PermissionDenied => Error::Access(e.to_string()),
        _ => Error::Io(e.to_string()),
    }
}

fn map_transfer_error(e: TransferError) -> Error {
    match e {
        // `transfer_blocking` reports its own timeout as a cancellation.
        TransferError::Cancelled => Error::Timeout,
        TransferError::Stall => Error::Stall,
        TransferError::Disconnected => Error::Disconnected,
        other => Error::Io(other.to_string()),
    }
}

/// The endpoints SANE's `sanei_usb` would pick: the first bulk-out, bulk-in
/// and interrupt-in endpoint of the interface's first alternate setting.
fn find_endpoints(device: &nusb::Device, interface: u8) -> Result<(u8, u8, Option<u8>)> {
    let config = device.active_configuration().map_err(|e| Error::Io(e.to_string()))?;
    let alt = config
        .interface_alt_settings()
        .find(|a| a.interface_number() == interface)
        .ok_or_else(|| Error::Io(format!("device has no interface {interface}")))?;
    let find = |ty: TransferType, dir: Direction| {
        alt.endpoints().find(|e| e.transfer_type() == ty && e.direction() == dir).map(|e| e.address())
    };
    let bulk_out = find(TransferType::Bulk, Direction::Out).ok_or_else(|| Error::Io("no bulk-out endpoint".into()))?;
    let bulk_in = find(TransferType::Bulk, Direction::In).ok_or_else(|| Error::Io("no bulk-in endpoint".into()))?;
    Ok((bulk_out, bulk_in, find(TransferType::Interrupt, Direction::In)))
}

/// A claimed interface on a real device.
pub struct NusbTransport {
    ids: DeviceIds,
    bulk_out: Endpoint<Bulk, Out>,
    bulk_in: Endpoint<Bulk, In>,
    interrupt_in: Option<Endpoint<Interrupt, In>>,
    device: nusb::Device,
    // Dropped last: releasing the interface closes the endpoints' pipes.
    interface: Interface,
}

impl std::fmt::Debug for NusbTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NusbTransport").field("ids", &self.ids).finish_non_exhaustive()
    }
}

impl NusbTransport {
    /// Opens `device` and claims `interface`.
    ///
    /// macOS leaves devices with vendor-specific descriptors unconfigured, so
    /// an unconfigured device is put into its first configuration first.
    ///
    /// # Errors
    /// Fails if the device is gone, another driver holds it, or it lacks the
    /// expected endpoints.
    pub fn open(device: &UsbDeviceInfo, interface: u8) -> Result<Self> {
        let dev = device.info.open().wait().map_err(|e| map_os_error(&e))?;
        if dev.active_configuration().is_err() {
            let first = dev.configurations().next().map(|c| c.configuration_value());
            if let Some(value) = first {
                log::debug!(value; "configuring unconfigured device");
                dev.set_configuration(value).wait().map_err(|e| map_os_error(&e))?;
            }
        }
        let (out_addr, in_addr, irq_addr) = find_endpoints(&dev, interface)?;
        let iface = dev.claim_interface(interface).wait().map_err(|e| map_os_error(&e))?;
        let bulk_out = iface.endpoint::<Bulk, Out>(out_addr).map_err(|e| Error::Io(e.to_string()))?;
        let bulk_in = iface.endpoint::<Bulk, In>(in_addr).map_err(|e| Error::Io(e.to_string()))?;
        let interrupt_in = match irq_addr {
            Some(a) => Some(iface.endpoint::<Interrupt, In>(a).map_err(|e| Error::Io(e.to_string()))?),
            None => None,
        };
        Ok(Self { ids: device.ids.clone(), bulk_out, bulk_in, interrupt_in, device: dev, interface: iface })
    }
}

/// The type and recipient fields of a `bmRequestType`.
fn control_fields(request_type: u8) -> (ControlType, Recipient) {
    let control_type = match (request_type >> 5) & 3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        _ => ControlType::Vendor,
    };
    let recipient = match request_type & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        _ => Recipient::Other,
    };
    (control_type, recipient)
}

/// Reads one transfer of up to `buf.len()` bytes from `ep`.
///
/// IN transfers must request a multiple of the packet size, so the request
/// is rounded up. A device that sends more than `buf` holds is reporting
/// more than the driver asked for, which is a protocol error.
fn read<T: nusb::transfer::BulkOrInterrupt>(
    ep: &mut Endpoint<T, In>,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<usize> {
    let packet = ep.max_packet_size().max(1);
    let mut request = Buffer::new(buf.len().max(1).div_ceil(packet) * packet);
    request.set_requested_len(request.capacity());
    let data = ep.transfer_blocking(request, timeout).into_result().map_err(map_transfer_error)?;
    if data.len() > buf.len() {
        return Err(Error::Io(format!("device sent {} bytes, {} expected at most", data.len(), buf.len())));
    }
    buf[..data.len()].copy_from_slice(&data);
    Ok(data.len())
}

impl UsbTransport for NusbTransport {
    fn ids(&self) -> &DeviceIds {
        &self.ids
    }

    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<usize> {
        let buf = Buffer::from(data.to_vec());
        let done = self.bulk_out.transfer_blocking(buf, timeout);
        done.status.map_err(map_transfer_error)?;
        Ok(done.actual_len)
    }

    fn bulk_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        read(&mut self.bulk_in, buf, timeout)
    }

    fn interrupt_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if let Some(ep) = &mut self.interrupt_in {
            return read(ep, buf, timeout);
        }
        std::thread::sleep(timeout);
        Err(Error::Timeout)
    }

    fn control_in(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize> {
        let (control_type, recipient) = control_fields(request_type);
        let length = u16::try_from(buf.len()).map_err(|_| Error::Io("control transfer too long".into()))?;
        let request = ControlIn { control_type, recipient, request, value, index, length };
        let data = self.interface.control_in(request, timeout).wait().map_err(map_transfer_error)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn control_out(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize> {
        let (control_type, recipient) = control_fields(request_type);
        let request = ControlOut { control_type, recipient, request, value, index, data };
        self.interface.control_out(request, timeout).wait().map_err(map_transfer_error)?;
        Ok(data.len())
    }

    fn clear_halt(&mut self) -> Result<()> {
        self.bulk_out.clear_halt().wait().map_err(|e| map_os_error(&e))?;
        self.bulk_in.clear_halt().wait().map_err(|e| map_os_error(&e))
    }

    fn reset(&mut self) -> Result<()> {
        self.device.reset().wait().map_err(|e| map_os_error(&e))
    }

    fn set_bulk_endpoints(&mut self, bulk_out: Option<u8>, bulk_in: Option<u8>) -> Result<()> {
        if let Some(address) = bulk_out {
            self.bulk_out = self.interface.endpoint::<Bulk, Out>(address).map_err(|e| Error::Io(e.to_string()))?;
        }
        if let Some(address) = bulk_in {
            self.bulk_in = self.interface.endpoint::<Bulk, In>(address).map_err(|e| Error::Io(e.to_string()))?;
        }
        Ok(())
    }
}

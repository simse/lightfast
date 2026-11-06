//! Ricoh Aficio SP-100, SP-111 and SG-3100 series MFPs over USB.
//!
//! A port of SANE's `ricoh2` backend. Untested on real hardware.
//!
//! These scanners always scan the whole A4 bed: the area in the settings is
//! ignored, as in SANE, and [`Parameters`] reports the full frame. Colour
//! data comes as consecutive R, G and B lines, each padded with a few info
//! bytes, in fixed-size blocks.

#![forbid(unsafe_code)]
#![expect(clippy::cast_possible_truncation, reason = "the block size fits a byte in 256ths")]

use std::time::Duration;

use scan_core::driver::{ByteQueue, invalid, mode, protocol, resolve};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, Parameters, PixelLayout, Result, ScanSettings, SourceCaps,
    SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};

const VENDOR_ID: u16 = 0x05ca;
const TIMEOUT: Duration = Duration::from_millis(20_000);
const MAX_LINE_SIZE: usize = 240 * 256;
const HEIGHT_PIXELS_300DPI: usize = 3508;
const WIDTH_BYTES_300DPI: usize = 2560;
const WIDTH_PIXELS_300DPI: usize = 2550;
const INFO_SIZE: usize = WIDTH_BYTES_300DPI - WIDTH_PIXELS_300DPI;
const MAX_COMMAND_SIZE: usize = 64;
const RESOLUTIONS: [u32; 2] = [300, 600];
const MODE_COLOR: &str = "Color";

const MODELS: &[(u16, &str)] = &[
    (0x042c, "Aficio SP-100SU"),
    (0x0438, "Aficio SG-3100SNw"),
    (0x0439, "Aficio SG-3110SFNw"),
    (0x0448, "Aficio SP-111SU/SP-112SU"),
];

/// This backend, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: "ricoh2",
    find: |vid, pid| {
        if vid != VENDOR_ID {
            return None;
        }
        MODELS.iter().find(|m| m.0 == pid).map(|m| UsbModel { vendor: "Ricoh", model: m.1 })
    },
    open: |usb| Ok(Box::new(Ricoh2::open(usb))),
    interface: 0,
};

fn capabilities() -> Capabilities {
    Capabilities {
        sources: vec![SourceCaps {
            name: "Flatbed".into(),
            kind: SourceKind::Flatbed,
            width_mm: WIDTH_PIXELS_300DPI as f64 / 300.0 * scan_core::MM_PER_INCH,
            height_mm: HEIGHT_PIXELS_300DPI as f64 / 300.0 * scan_core::MM_PER_INCH,
            modes: vec![
                mode(MODE_COLOR, Channels::Rgb, 8, &RESOLUTIONS),
                mode("Gray", Channels::Gray, 8, &RESOLUTIONS),
            ],
            gamma: None,
            extras: vec![],
        }],
    }
}

/// Splits fixed-size blocks into pixels (`ricoh2_buffer_get_data`).
#[derive(Debug, Clone, Copy)]
struct Deinterleave {
    pixels_per_line: usize,
    info_size: usize,
    rgb: bool,
}

impl Deinterleave {
    fn push(self, block: &[u8], out: &mut ByteQueue) {
        let per_color = self.pixels_per_line + self.info_size;
        let stride = if self.rgb { per_color * 3 } else { per_color };
        let needed = stride - self.info_size;
        let mut row = Vec::with_capacity(self.pixels_per_line * 3);
        for start in (0..block.len()).step_by(stride) {
            if start + needed > block.len() {
                break;
            }
            row.clear();
            for i in start..start + self.pixels_per_line {
                row.push(block[i]);
                if self.rgb {
                    row.push(block[i + per_color]);
                    row.push(block[i + 2 * per_color]);
                }
            }
            out.push(&row);
        }
    }
}

#[derive(Debug)]
struct Active {
    bytes_to_read: usize,
    format: Deinterleave,
    cancel: CancelToken,
}

/// An open Ricoh scanner.
#[derive(Debug)]
pub struct Ricoh2 {
    usb: Box<dyn UsbTransport>,
    info: DeviceInfo,
    caps: Capabilities,
    active: Option<Active>,
    queue: ByteQueue,
}

impl Ricoh2 {
    /// Wraps an open transport. The device isn't touched until a scan starts.
    pub fn open(usb: Box<dyn UsbTransport>) -> Self {
        let info = DRIVER.device_info(usb.ids());
        Self { usb, info, caps: capabilities(), active: None, queue: ByteQueue::new() }
    }

    /// Sends a command padded to the fixed command size, then reads an exact
    /// reply. A short reply is an error.
    fn send_receive(&mut self, command: &[u8], reply: &mut [u8]) -> Result<()> {
        let mut packet = [0u8; MAX_COMMAND_SIZE];
        packet[..command.len()].copy_from_slice(command);
        self.usb.bulk_out(&packet, TIMEOUT)?;
        if !reply.is_empty() {
            let n = self.usb.bulk_in(reply, TIMEOUT)?;
            if n != reply.len() {
                return Err(protocol(format!("short reply: {n} of {} bytes", reply.len())));
            }
        }
        Ok(())
    }

    fn init_scan(&mut self, rgb: bool, dpi: u32) -> Result<()> {
        let mut magic1 = [
            0x03, 0x0c, 0x11, 0x00, 0x00, 0x00, 0x01, 0x02, 0x05, 0xff, 0x00, 0x00, 0x00, 0x00, 0xec, 0x13, 0x6c, 0x1b,
        ];
        if dpi == 600 {
            magic1[6] = 0x02;
        }
        if rgb {
            magic1[7] = 0x03;
        }
        let mut dummy = [0u8; 11];
        self.send_receive(&[0x03, 0x09, 0x01], &mut dummy[..1])?;
        self.send_receive(&[0x03, 0x0d, 0x0b], &mut dummy[..11])?;
        self.send_receive(&magic1, &mut [])?;
        self.send_receive(&[0x03, 0x0b, 0x08], &mut dummy[..8])?;
        self.send_receive(&[0x03, 0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x50, 0x6d, 0x06, 0x01], &mut [])
    }
}

fn frame(rgb: bool, dpi: u32) -> Parameters {
    let factor = if dpi == 600 { 2 } else { 1 };
    let channels = if rgb { Channels::Rgb } else { Channels::Gray };
    Parameters {
        layout: PixelLayout { channels, depth: 8 },
        pixels_per_line: WIDTH_PIXELS_300DPI * factor,
        bytes_per_line: WIDTH_PIXELS_300DPI * factor * channels.count(),
        lines: HEIGHT_PIXELS_300DPI * factor,
    }
}

impl Device for Ricoh2 {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        let r = resolve(&self.caps, settings)?;
        Ok(frame(r.mode.layout.channels == Channels::Rgb, r.dpi))
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let r = resolve(&self.caps, settings)?;
        let (rgb, dpi) = (r.mode.layout.channels == Channels::Rgb, r.dpi);
        self.finish();
        cancel.check()?;
        self.usb.set_bulk_endpoints(Some(0x03), Some(0x85))?;
        self.usb.reset()?;
        self.init_scan(rgb, dpi)?;
        let params = frame(rgb, dpi);
        let factor = if dpi == 600 { 2 } else { 1 };
        self.active = Some(Active {
            bytes_to_read: params.bytes_per_line * params.lines,
            format: Deinterleave { pixels_per_line: params.pixels_per_line, info_size: INFO_SIZE * factor, rgb },
            cancel: cancel.clone(),
        });
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(active) = &self.active else { return Err(invalid("no scan is running")) };
        active.cancel.check()?;
        if active.bytes_to_read == 0 {
            return Ok(0);
        }
        let format = active.format;
        if self.queue.is_empty() {
            let command = [0x03, 0x0e, 0x04, 0, 0, 0, 0, (MAX_LINE_SIZE / 256) as u8];
            let mut block = vec![0u8; MAX_LINE_SIZE];
            self.send_receive(&command, &mut block)?;
            format.push(&block, &mut self.queue);
        }
        let Some(active) = &mut self.active else { return Ok(0) };
        let want = buf.len().min(active.bytes_to_read);
        let n = self.queue.pop_into(&mut buf[..want]);
        active.bytes_to_read -= n;
        Ok(n)
    }

    fn finish(&mut self) {
        if self.active.take().is_some() {
            self.queue.clear();
            // Best effort: the device resets at the next start anyway.
            let _ = self.send_receive(&[0x03, 0x0a], &mut []);
            let mut dummy = [0u8; 1];
            let _ = self.send_receive(&[0x03, 0x09, 0x01], &mut dummy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::driver::{full_area_settings, scan_frame};
    use scan_usb::mock::{Mock, Transfer};

    /// Blocks whose first R, G, B bytes in each line identify the channel.
    fn device(pid: u16) -> Mock {
        let mut replies: Vec<usize> = vec![1, 11, 8, 1];
        Mock::new(VENDOR_ID, pid, move |t| match t {
            Transfer::BulkIn(len) if *len == MAX_LINE_SIZE => {
                let mut block = vec![0u8; MAX_LINE_SIZE];
                for (i, b) in block.iter_mut().enumerate() {
                    *b = match (i / WIDTH_BYTES_300DPI) % 3 {
                        0 => 0x10,
                        1 => 0x20,
                        _ => 0x30,
                    };
                }
                Ok(block)
            }
            Transfer::BulkIn(len) => {
                assert_eq!(replies.remove(0), *len);
                Ok(vec![0; *len])
            }
            _ => Ok(vec![]),
        })
    }

    #[test]
    fn color_scan_interleaves_lines() {
        let mock = device(0x042c);
        let log = mock.log();
        let mut scanner = Ricoh2::open(Box::new(mock));
        let settings = full_area_settings(scanner.capabilities(), MODE_COLOR, 300);
        let (params, image) = scan_frame(&mut scanner, &settings).unwrap();
        assert_eq!((params.pixels_per_line, params.lines), (2550, 3508));
        assert_eq!(image.len(), 2550 * 3 * 3508);
        assert_eq!(&image[..3], [0x10, 0x20, 0x30]);

        let log = log.lock().unwrap();
        let Transfer::BulkOut(magic1) = &log[4] else { panic!("{:?}", log[4]) };
        assert_eq!(magic1.len(), MAX_COMMAND_SIZE, "commands are padded");
        assert_eq!(&magic1[..9], [0x03, 0x0c, 0x11, 0, 0, 0, 0x01, 0x03, 0x05]);
        let Transfer::BulkOut(last) = &log[log.len() - 2] else { panic!() };
        assert_eq!(&last[..3], [0x03, 0x09, 0x01], "teardown ends the session");
    }

    #[test]
    fn modes_and_dpi_set_the_right_magic_bits() {
        let mock = device(0x0448);
        let log = mock.log();
        let mut scanner = Ricoh2::open(Box::new(mock));
        let settings = full_area_settings(scanner.capabilities(), "Gray", 600);
        let params = scanner.start(&settings, &CancelToken::new()).unwrap();
        assert_eq!((params.pixels_per_line, params.bytes_per_line), (5100, 5100));
        let log = log.lock().unwrap();
        let Transfer::BulkOut(magic1) = &log[4] else { panic!() };
        assert_eq!(&magic1[6..8], [0x02, 0x02]);
    }

    #[test]
    fn short_replies_are_errors() {
        let mock = Mock::new(VENDOR_ID, 0x042c, |t| match t {
            Transfer::BulkIn(_) => Ok(vec![]),
            _ => Ok(vec![]),
        });
        let mut scanner = Ricoh2::open(Box::new(mock));
        let settings = full_area_settings(scanner.capabilities(), MODE_COLOR, 300);
        assert!(scanner.start(&settings, &CancelToken::new()).is_err());
    }

    #[test]
    fn only_ricoh_ids_match() {
        assert!((DRIVER.find)(VENDOR_ID, 0x0439).is_some());
        assert!((DRIVER.find)(0x03f0, 0x0439).is_none());
    }
}

//! Lexmark X2600 series over USB.
//!
//! A port of SANE's `lexmark_x2600` backend. Untested on real hardware.
//!
//! The scanner takes a fixed command sequence and then streams packets, each
//! either a control marker or image lines with a 9-byte header; lines can span
//! transfers. Colour pixels arrive as BGR. The scan always starts at the
//! top-left corner: as in SANE, only the bottom-right corner is settable, and
//! the top-left of the requested area is included in the image.
//!
//! SANE reports an unknown line count for this scanner. [`Parameters::lines`]
//! is the requested height, but the scanner decides where the image ends.

#![forbid(unsafe_code)]
#![expect(clippy::cast_possible_truncation, reason = "coordinates are bounded by the bed")]

use std::time::Duration;

use scan_core::driver::{ByteQueue, invalid, mode, resolve};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, Error, Parameters, PixelLayout, Result, ScanSettings,
    SourceCaps, SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};

const TIMEOUT: Duration = Duration::from_secs(30);
const TRANSFER_BUFFER_SIZE: usize = 32768;
const REPLY_SIZE: usize = 256;
const LINE_HEADER_LENGTH: usize = 9;
const RESOLUTIONS: [u32; 4] = [100, 200, 300, 600];
const MODE_COLOR: &str = "Color";
/// The coordinate limits of `x_range` and `y_range`.
const MAX_X: u32 = 5078;
const MAX_Y: u32 = 7015;

const COMMAND1: [u8; 28] = [
    0xA5, 0x00, 0x19, 0x10, 0x01, 0x83, 0xAA, 0xBB, 0xCC, 0xDD, 0x02, 0x00, 0x1B, 0x53, 0x03, 0x00, 0x00, 0x00, 0x80,
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xAA, 0xBB, 0xCC, 0xDD,
];
const COMMAND2: [u8; 28] = [
    0xA5, 0x00, 0x19, 0x10, 0x01, 0x83, 0xAA, 0xBB, 0xCC, 0xDD, 0x02, 0x00, 0x1B, 0x53, 0x04, 0x00, 0x00, 0x00, 0x80,
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xAA, 0xBB, 0xCC, 0xDD,
];
const COMMAND_WITH_PARAMS: [u8; 52] = [
    0xA5, 0x00, 0x31, 0x10, 0x01, 0x83, 0xAA, 0xBB, 0xCC, 0xDD, 0x02, 0x00, 0x1B, 0x53, 0x05, 0x00, 0x18, 0x00, 0x80,
    0x00, 0xFF, 0x00, 0x00, 0x02, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xAA, 0xBB, 0xCC, 0xDD, 0xAA, 0xBB, 0xCC, 0xDD,
];
const COMMAND_CANCEL1: [u8; 28] = [
    0xa5, 0x00, 0x19, 0x10, 0x01, 0x83, 0xaa, 0xbb, 0xcc, 0xdd, 0x02, 0x00, 0x1b, 0x53, 0x0f, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xaa, 0xbb, 0xcc, 0xdd,
];
const COMMAND_CANCEL2: [u8; 28] = [
    0xa5, 0x00, 0x19, 0x10, 0x01, 0x83, 0xaa, 0xbb, 0xcc, 0xdd, 0x02, 0x00, 0x1b, 0x53, 0x06, 0x00, 0x00, 0x00, 0x80,
    0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xaa, 0xbb, 0xcc, 0xdd,
];

const LAST_DATA_PACKET: [u8; 9] = [0x1b, 0x53, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01];
const CANCEL_PACKET: [u8; 9] = [0x1b, 0x53, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03];
const LINE_BEGIN: [u8; 4] = [0x1b, 0x53, 0x02, 0x00];
/// Packets that carry no image data.
const IGNORED_PACKETS: [&[u8]; 6] = [
    &[0x1b, 0x53, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
    &[0x1b, 0x53, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00],
    &[0x1b, 0x53, 0x04, 0x00, 0x00, 0x00, 0x80, 0x00],
    &[0x1b, 0x53, 0x04, 0x00, 0x00, 0x00, 0x84, 0x00],
    &[0x1b, 0x53, 0x05, 0x00, 0x00, 0x00],
    &[0xa5, 0x00, 0x06, 0x10, 0x01, 0xaa, 0xbb, 0xcc, 0xdd],
];

/// This backend, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: "lexmark_x2600",
    find: |vid, pid| (vid == 0x043d && pid == 0x011d).then_some(UsbModel { vendor: "Lexmark", model: "X2600 series" }),
    open: |usb| Ok(Box::new(LexmarkX2600::open(usb))),
    interface: 0,
};

fn capabilities() -> Capabilities {
    let mm = |px: u32| f64::from(px) / 600.0 * scan_core::MM_PER_INCH;
    Capabilities {
        sources: vec![SourceCaps {
            name: "Flatbed".into(),
            kind: SourceKind::Flatbed,
            width_mm: mm(MAX_X),
            height_mm: mm(MAX_Y),
            modes: vec![
                mode(MODE_COLOR, Channels::Rgb, 8, &RESOLUTIONS),
                mode("Gray", Channels::Gray, 8, &RESOLUTIONS),
            ],
            gamma: None,
            extras: vec![],
        }],
    }
}

/// `build_packet`.
fn params_packet(id: u8, rgb: bool, br_x: u16, br_y: u16, dpi: u16) -> [u8; 52] {
    let mut p = COMMAND_WITH_PARAMS;
    p[14] = id;
    p[20] = if rgb { 0x03 } else { 0x02 };
    p[24..26].copy_from_slice(&br_x.to_le_bytes());
    p[28..30].copy_from_slice(&br_y.to_le_bytes());
    p[40..42].copy_from_slice(&dpi.to_le_bytes());
    p[42..44].copy_from_slice(&dpi.to_le_bytes());
    p
}

/// Reassembles lines from transfers (`clean_and_copy_data`), keeping whether
/// a line is unfinished from one transfer to the next. The C code reads past
/// the transfer if a line's remainder doesn't fit; here that line just stays
/// unfinished.
#[derive(Debug, Default)]
struct Lines {
    line_size: usize,
    line_read: usize,
    /// Bytes short of a whole BGR pixel, held until the rest arrives.
    partial: Vec<u8>,
}

impl Lines {
    fn push(&mut self, source: &[u8], rgb: bool, out: &mut ByteQueue) -> Result<()> {
        if source.starts_with(&LINE_BEGIN) && source.len() > 5 {
            self.line_size = (usize::from(source[4]) | (usize::from(source[5]) << 8)).saturating_sub(1);
            self.line_read = self.line_size;
        }
        if self.line_size == 0 {
            // The device gave no line size, which SANE treats as a cancel.
            return Err(Error::cancelled());
        }
        let mut i = 0;
        while i < source.len() {
            if self.line_read == self.line_size {
                let start = i + LINE_HEADER_LENGTH;
                if start >= source.len() {
                    break;
                }
                let len = self.line_size.min(source.len() - start);
                self.line_read = len;
                self.emit(&source[start..start + len], rgb, out);
                i = start + len;
            } else {
                let len = (self.line_size - self.line_read).min(source.len() - i);
                self.line_read += len;
                self.emit(&source[i..i + len], rgb, out);
                i += len;
            }
        }
        Ok(())
    }

    fn emit(&mut self, data: &[u8], rgb: bool, out: &mut ByteQueue) {
        if !rgb {
            out.push(data);
            return;
        }
        self.partial.extend_from_slice(data);
        let whole = self.partial.len() / 3 * 3;
        for pixel in self.partial[..whole].as_chunks_mut::<3>().0 {
            pixel.swap(0, 2);
        }
        out.push(&self.partial[..whole]);
        self.partial.drain(..whole);
    }
}

#[derive(Debug)]
struct Active {
    rgb: bool,
    cancel: CancelToken,
    lines: Lines,
    /// The scanner has sent its last packet.
    eof: bool,
}

/// An open X2600.
#[derive(Debug)]
pub struct LexmarkX2600 {
    usb: Box<dyn UsbTransport>,
    info: DeviceInfo,
    caps: Capabilities,
    active: Option<Active>,
    queue: ByteQueue,
}

impl LexmarkX2600 {
    /// Wraps an open transport. The device isn't touched until a scan starts.
    #[must_use]
    pub fn open(usb: Box<dyn UsbTransport>) -> Self {
        let info = DRIVER.device_info(usb.ids());
        Self { usb, info, caps: capabilities(), active: None, queue: ByteQueue::new() }
    }

    fn write_then_read(&mut self, command: &[u8]) -> Result<()> {
        self.usb.bulk_out(command, TIMEOUT)?;
        let mut reply = [0u8; REPLY_SIZE];
        self.usb.bulk_in(&mut reply, TIMEOUT)?;
        Ok(())
    }

    /// The bottom-right corner and frame for `settings`.
    fn geometry(&self, settings: &ScanSettings) -> Result<(u16, u16, u16, Parameters)> {
        let r = resolve(&self.caps, settings)?;
        let rgb = r.mode.layout.channels == Channels::Rgb;
        let br_x = (r.x + r.width).min(MAX_X);
        let br_y = (r.y + r.height).min(MAX_Y);
        let channels = if rgb { Channels::Rgb } else { Channels::Gray };
        let params = Parameters {
            layout: PixelLayout { channels, depth: 8 },
            pixels_per_line: br_x as usize,
            bytes_per_line: br_x as usize * channels.count(),
            lines: br_y as usize,
        };
        Ok((br_x as u16, br_y as u16, r.dpi as u16, params))
    }
}

impl Device for LexmarkX2600 {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        Ok(self.geometry(settings)?.3)
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let (br_x, br_y, dpi, params) = self.geometry(settings)?;
        let rgb = params.layout.channels == Channels::Rgb;
        self.finish();
        cancel.check()?;
        self.usb.set_bulk_endpoints(Some(0x02), None)?;
        self.write_then_read(&COMMAND1)?;
        self.write_then_read(&COMMAND2)?;
        self.write_then_read(&params_packet(0x05, rgb, br_x, br_y, dpi))?;
        self.write_then_read(&params_packet(0x01, rgb, br_x, br_y, dpi))?;
        self.active = Some(Active { rgb, cancel: cancel.clone(), lines: Lines::default(), eof: false });
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut transfer = vec![0u8; TRANSFER_BUFFER_SIZE];
        loop {
            let Some(active) = &self.active else { return Err(invalid("no scan is running")) };
            active.cancel.check()?;
            if !self.queue.is_empty() {
                return Ok(self.queue.pop_into(buf));
            }
            if active.eof {
                return Ok(0);
            }
            let n = self.usb.bulk_in(&mut transfer, TIMEOUT)?;
            let packet = &transfer[..n];
            let Some(active) = &mut self.active else { return Ok(0) };
            if packet.starts_with(&LAST_DATA_PACKET) {
                active.eof = true;
            } else if packet.starts_with(&CANCEL_PACKET) {
                return Err(Error::cancelled());
            } else if n > 0 && !IGNORED_PACKETS.iter().any(|p| packet.starts_with(p)) {
                active.lines.push(packet, active.rgb, &mut self.queue)?;
            }
        }
    }

    fn finish(&mut self) {
        let Some(active) = self.active.take() else { return };
        self.queue.clear();
        // Best effort throughout: there's nobody left to report a failure to.
        let _ = self.usb.reset();
        if !active.eof {
            for command in [&COMMAND_CANCEL1, &COMMAND_CANCEL2, &COMMAND_CANCEL1, &COMMAND_CANCEL2] {
                let _ = self.write_then_read(command);
            }
            let mut transfer = vec![0u8; TRANSFER_BUFFER_SIZE];
            for _ in 0..3 {
                if self.usb.bulk_in(&mut transfer, TIMEOUT).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::driver::{full_area_settings, scan_frame};
    use scan_usb::mock::{Mock, Transfer};
    use std::collections::VecDeque;

    fn device(data: Vec<Vec<u8>>) -> Mock {
        let mut data: VecDeque<_> = data.into();
        let mut acks = 4;
        Mock::new(0x043d, 0x011d, move |t| match t {
            Transfer::BulkIn(_) if acks > 0 => {
                acks -= 1;
                Ok(vec![0])
            }
            Transfer::BulkIn(_) => data.pop_front().ok_or(scan_usb::Error::Timeout),
            _ => Ok(vec![]),
        })
    }

    fn line(pixels: &[u8]) -> Vec<u8> {
        let size = pixels.len() + 1;
        let mut p = vec![0x1b, 0x53, 0x02, 0x00, size as u8, (size >> 8) as u8, 0, 0, 0];
        p.extend_from_slice(pixels);
        p
    }

    #[test]
    fn params_packet_carries_mode_corner_and_dpi() {
        let p = params_packet(0x01, true, 1654, 2339, 200);
        assert_eq!((p[14], p[20]), (0x01, 0x03));
        assert_eq!(&p[24..26], 1654u16.to_le_bytes());
        assert_eq!(&p[28..30], 2339u16.to_le_bytes());
        assert_eq!(&p[40..44], [200, 0, 200, 0]);
        assert_eq!(params_packet(0x05, false, 1, 1, 100)[20], 0x02);
    }

    #[test]
    fn color_is_swapped_from_bgr() {
        let packets = vec![line(&[1, 2, 3, 4, 5, 6]), LAST_DATA_PACKET.to_vec()];
        let mock = device(packets);
        let log = mock.log();
        let mut scanner = LexmarkX2600::open(Box::new(mock));
        let settings = full_area_settings(scanner.capabilities(), MODE_COLOR, 100);
        let (params, image) = scan_frame(&mut scanner, &settings).unwrap();
        assert_eq!(image, [3, 2, 1, 6, 5, 4]);
        assert_eq!(params.bytes_per_line, params.pixels_per_line * 3);
        let log = log.lock().unwrap();
        assert_eq!(log[0], Transfer::BulkOut(COMMAND1.to_vec()));
        assert!(!log.contains(&Transfer::BulkOut(COMMAND_CANCEL1.to_vec())), "a finished scan needs no cancel");
    }

    #[test]
    fn lines_span_transfers_and_ignored_packets_are_skipped() {
        let whole = line(&[1, 2, 3, 4]);
        let packets = vec![
            whole[..11].to_vec(),
            IGNORED_PACKETS[1].to_vec(),
            whole[11..].to_vec(),
            line(&[5, 6, 7, 8]),
            LAST_DATA_PACKET.to_vec(),
        ];
        let mut lines = Lines::default();
        let mut q = ByteQueue::new();
        for p in &packets {
            if p != &LAST_DATA_PACKET && !IGNORED_PACKETS.iter().any(|i| p.starts_with(i)) {
                lines.push(p, false, &mut q).unwrap();
            }
        }
        let mut out = [0; 16];
        let n = q.pop_into(&mut out);
        assert_eq!(&out[..n], [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn stopping_early_sends_the_cancel_sequence() {
        let mock = device(vec![line(&[1, 2, 3])]);
        let log = mock.log();
        let mut scanner = LexmarkX2600::open(Box::new(mock));
        let settings = full_area_settings(scanner.capabilities(), MODE_COLOR, 100);
        scanner.start(&settings, &CancelToken::new()).unwrap();
        scanner.finish();
        assert!(log.lock().unwrap().contains(&Transfer::BulkOut(COMMAND_CANCEL2.to_vec())));
    }

    #[test]
    fn a_stream_with_no_line_size_cancels() {
        let mut lines = Lines::default();
        assert!(lines.push(&[9, 9, 9], false, &mut ByteQueue::new()).unwrap_err().is_cancelled());
    }
}

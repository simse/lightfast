//! HP `LaserJet` M1005 and M1120 MFP scanner over USB.
//!
//! A port of SANE's `hpljm1005` backend. Untested on real hardware: nobody
//! here owns one and no recording exists yet, so the tests only check the
//! driver against a reading of the C source.
//!
//! The protocol is packets of big-endian `u32`s. The scanner sends each colour
//! as separate layers in blocks, usually R G B ... but a run of RRR GGG BBB at
//! the end, so layers are buffered until all three have the same pixel.

#![forbid(unsafe_code)]
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "device units are small"
)]

use std::time::Duration;

use scan_core::driver::{ByteQueue, device_error, invalid, mode, protocol};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, ExtraOption, OptionKind, OptionValue, Parameters,
    PixelLayout, Result, ScanSettings, SourceCaps, SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};

const BACKEND: &str = "hpljm1005";
const TIMEOUT: Duration = Duration::from_secs(30);

const MAGIC_NUMBER: u32 = 0x4153_5001;
const PKT_READ_STATUS: u32 = 0x0;
const PKT_UNKNOWN_1: u32 = 0x1;
const PKT_START_SCAN: u32 = 0x2;
const PKT_GO_IDLE: u32 = 0x3;
const PKT_DATA: u32 = 0x5;
const PKT_READCONF: u32 = 0x6;
const PKT_SETCONF: u32 = 0x7;
const PKT_END_DATA: u32 = 0xe;
const PKT_RESET: u32 = 0x15;

const RED_LAYER: u32 = 0x3;
const GREEN_LAYER: u32 = 0x4;
const BLUE_LAYER: u32 = 0x5;
const GRAY_LAYER: u32 = 0x6;

/// The smallest scan zone in device units.
const MIN_SCAN_ZONE: i32 = 101;
const MAX_X_H: i32 = 0x351;
const MAX_Y_H: i32 = 0x490;
const MAX_X_S: i32 = 216;
const MAX_Y_S: i32 = 297;
const CONF_SIZE: usize = 100;
const BR_CONT_MIN: i32 = 1;
const BR_CONT_MAX: i32 = 0xb;
const BR_CONT_DEFAULT: i32 = 6;
const RESOLUTIONS: [u32; 7] = [75, 100, 150, 200, 300, 600, 1200];
const MODE_GRAY: &str = "Gray";
const MODE_COLOR: &str = "Color";

const MODELS: &[(u16, u16, &str)] =
    &[(0x03f0, 0x3b17, "LaserJet M1005"), (0x03f0, 0x5617, "LaserJet M1120"), (0x03f0, 0x5717, "LaserJet M1120n")];

/// This backend, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: BACKEND,
    find: |vid, pid| {
        MODELS.iter().find(|m| m.0 == vid && m.1 == pid).map(|m| UsbModel { vendor: "Hewlett-Packard", model: m.2 })
    },
    open: |transport| Ok(Box::new(Hpljm1005::open(transport))),
    interface: 0,
};

/// The C code's `round2`, rounding halves away from zero.
fn round2(x: f64) -> i32 {
    if x >= 0.0 { (x + 0.5) as i32 } else { (x - 0.5) as i32 }
}

fn to_units(mm: i32, max_mm: i32, max_units: i32) -> i32 {
    round2(f64::from(mm) / f64::from(max_mm) * f64::from(max_units))
}

fn to_mm(units: i32, max_mm: i32, max_units: i32) -> i32 {
    round2(f64::from(units) / f64::from(max_units) * f64::from(max_mm))
}

/// Applies `setvalue`'s minimum scan zone to whole-millimetre edges and
/// returns the edges in device units.
fn zone(start_mm: f64, end_mm: f64, max_mm: i32, max_units: i32) -> (i32, i32) {
    let start = round2(start_mm).clamp(0, max_mm);
    let end = round2(end_mm).clamp(0, max_mm);
    let mut first = to_units(start, max_mm, max_units);
    let last = to_units(end, max_mm, max_units);
    if (last - first).abs() < MIN_SCAN_ZONE {
        first = last - MIN_SCAN_ZONE;
    }
    // Settings are converted back to whole millimetres, then to units again in
    // `send_conf`. A zone pushed off the bed start falls back to the far side.
    let mut start = to_mm(first, max_mm, max_units);
    let mut end = end;
    if start < 0 {
        start = 0;
        end = to_mm(MIN_SCAN_ZONE, max_mm, max_units);
    }
    (to_units(start, max_mm, max_units), to_units(end, max_mm, max_units))
}

/// A scan in device units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scan {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    resolution: i32,
    brightness: i32,
    contrast: i32,
    rgb: bool,
}

fn int_extra(settings: &ScanSettings, name: &str, default: i32) -> Result<i32> {
    match settings.extra(name) {
        None => Ok(default),
        Some(OptionValue::Int(v)) if (BR_CONT_MIN..=BR_CONT_MAX).contains(v) => Ok(*v),
        Some(_) => Err(invalid(format!("bad value for {name}"))),
    }
}

impl Scan {
    fn from_settings(caps: &Capabilities, s: &ScanSettings) -> Result<Self> {
        let request = scan_core::driver::resolve(caps, s)?;
        let area = &s.area;
        let (x1, x2) = zone(area.x, area.x + area.width, MAX_X_S, MAX_X_H);
        let (y1, y2) = zone(area.y, area.y + area.height, MAX_Y_S, MAX_Y_H);
        Ok(Self {
            x1,
            y1,
            x2,
            y2,
            resolution: request.dpi as i32,
            brightness: int_extra(s, "brightness", BR_CONT_DEFAULT)?,
            contrast: int_extra(s, "contrast", BR_CONT_DEFAULT)?,
            rgb: request.mode.layout.channels == Channels::Rgb,
        })
    }

    /// The `send_conf` packet.
    fn conf(&self) -> [u8; CONF_SIZE] {
        let mut words = [0u32; CONF_SIZE / 4];
        words[0] = 0x15;
        words[1] = self.brightness as u32;
        words[2] = self.contrast as u32;
        words[3] = self.resolution as u32;
        words[4..8].fill(1);
        words[10] = 8;
        words[15] = if self.rgb { 2 } else { 6 };
        words[16] = self.y1 as u32;
        words[17] = self.x1 as u32;
        words[18] = self.y2 as u32;
        words[19] = self.x2 as u32;
        words[22] = 0x491;
        words[23] = 0x352;
        words[24] = u32::from(self.rgb);
        let mut out = [0; CONF_SIZE];
        for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn height(&self) -> usize {
        ((self.y2 - self.y1) * self.resolution / 100) as usize
    }

    fn parameters(&self, width: usize) -> Parameters {
        let channels = if self.rgb { Channels::Rgb } else { Channels::Gray };
        Parameters {
            layout: PixelLayout { channels, depth: 8 },
            pixels_per_line: width,
            bytes_per_line: width * channels.count(),
            lines: self.height(),
        }
    }
}

#[derive(Debug, Default)]
struct Layers {
    r: Vec<u8>,
    g: Vec<u8>,
    b: Vec<u8>,
    /// Layer bytes one buffer may hold; 0 until the first data packet.
    capacity: usize,
    read_offset: usize,
}

impl Layers {
    fn available(&self, rgb: bool) -> usize {
        if rgb { self.r.len().min(self.g.len()).min(self.b.len()) } else { self.r.len() }
    }

    fn synchronized(&self, rgb: bool) -> bool {
        !rgb || (self.r.len() == self.g.len() && self.r.len() == self.b.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Scanning,
}

/// An open M1005 or M1120.
#[derive(Debug)]
pub struct Hpljm1005 {
    usb: Box<dyn UsbTransport>,
    info: DeviceInfo,
    caps: Capabilities,
    status: Status,
    scan: Option<Scan>,
    cancel: CancelToken,
    layers: Layers,
    queue: ByteQueue,
    width: usize,
    data_width: usize,
    scanned_pixels: usize,
}

fn word(buf: &[u8], index: usize) -> u32 {
    let at = index * 4;
    buf.get(at..at + 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn capabilities() -> Capabilities {
    let option = |name: &str, title: &str| ExtraOption {
        name: name.into(),
        title: title.into(),
        kind: OptionKind::Int { min: BR_CONT_MIN, max: BR_CONT_MAX },
        default: OptionValue::Int(BR_CONT_DEFAULT),
    };
    Capabilities {
        sources: vec![SourceCaps {
            name: "Flatbed".into(),
            kind: SourceKind::Flatbed,
            width_mm: f64::from(MAX_X_S),
            height_mm: f64::from(MAX_Y_S),
            modes: vec![
                mode(MODE_COLOR, Channels::Rgb, 8, &RESOLUTIONS),
                mode(MODE_GRAY, Channels::Gray, 8, &RESOLUTIONS),
            ],
            gamma: None,
            extras: vec![option("brightness", "Brightness"), option("contrast", "Contrast")],
        }],
    }
}

impl Hpljm1005 {
    /// Wraps an open transport. The device isn't touched until a scan starts.
    pub fn open(usb: Box<dyn UsbTransport>) -> Self {
        let ids = usb.ids().clone();
        let model = (DRIVER.find)(ids.vendor_id, ids.product_id).map_or("LaserJet M1005", |m| m.model);
        let info = DeviceInfo {
            name: DRIVER.device_name(ids.vendor_id, ids.product_id, ids.serial.as_deref()),
            vendor: "Hewlett-Packard".into(),
            model: model.into(),
        };
        Self {
            usb,
            info,
            caps: capabilities(),
            status: Status::Idle,
            scan: None,
            cancel: CancelToken::new(),
            layers: Layers::default(),
            queue: ByteQueue::new(),
            width: 0,
            data_width: 0,
            scanned_pixels: 0,
        }
    }

    fn send_pkt(&mut self, command: u32, data_size: u32) -> Result<()> {
        let mut pkt = [0u8; 32];
        pkt[0..4].copy_from_slice(&MAGIC_NUMBER.to_be_bytes());
        pkt[4..8].copy_from_slice(&command.to_be_bytes());
        pkt[20..24].copy_from_slice(&data_size.to_be_bytes());
        self.usb.bulk_out(&pkt, TIMEOUT)?;
        Ok(())
    }

    /// Reads until a non-empty transfer arrives. The C code retries forever;
    /// here a timeout ends the wait, and so does cancellation.
    fn read_nonempty(&mut self, buf: &mut [u8]) -> Result<usize> {
        loop {
            let n = self.usb.bulk_in(buf, TIMEOUT)?;
            if n > 0 {
                return Ok(n);
            }
            std::thread::yield_now();
        }
    }

    /// `wait_ack`: returns (status, next packet size).
    fn wait_ack(&mut self) -> Result<(u32, usize)> {
        let mut pkt = [0u8; 32];
        self.read_nonempty(&mut pkt)?;
        Ok((word(&pkt, 4), word(&pkt, 5) as usize))
    }

    /// Reads and discards the payload an ack announces.
    fn skip_announced(&mut self) -> Result<()> {
        let (_, size) = self.wait_ack()?;
        if size > 0 {
            let mut conf = vec![0; size];
            self.usb.bulk_in(&mut conf, TIMEOUT)?;
        }
        Ok(())
    }

    /// `get_data`: buffers one data packet. Returns false at the end of data.
    fn get_data(&mut self) -> Result<bool> {
        if self.status == Status::Idle {
            return Err(device_error("the scanner is idle"));
        }
        let mut pkt = [0u8; 32];
        let mut packet_size;
        loop {
            let n = self.usb.bulk_in(&mut pkt, TIMEOUT)?;
            if n == 0 || word(&pkt, 0) != MAGIC_NUMBER {
                continue;
            }
            match word(&pkt, 1) {
                PKT_DATA => {
                    packet_size = word(&pkt, 5) as usize;
                    break;
                }
                PKT_END_DATA => {
                    self.end_scan()?;
                    return Ok(false);
                }
                _ => {}
            }
        }
        if self.layers.capacity == 0 {
            // <start> R G B ... R G B RRR GGG BBB <end>: room for the last run.
            self.layers.capacity = packet_size.saturating_sub(24) * 3;
        }
        let mut header = [0u8; 24];
        let n = self.read_nonempty(&mut header)?;
        let color = word(&header, 0);
        packet_size = packet_size.saturating_sub(n);
        self.width = word(&header, 4) as usize;
        self.data_width = word(&header, 5) as usize;
        if self.data_width == 0 {
            return Err(protocol("the scanner reported a data width of 0"));
        }
        let capacity = self.layers.capacity;
        while packet_size > 0 {
            let mut block = [0u8; 512];
            let want = packet_size.min(block.len());
            let size = self.read_nonempty(&mut block[..want])?;
            packet_size -= size;
            let layer = match color {
                GRAY_LAYER | RED_LAYER => &mut self.layers.r,
                GREEN_LAYER => &mut self.layers.g,
                BLUE_LAYER => &mut self.layers.b,
                other => return Err(protocol(format!("unknown colour layer {other}"))),
            };
            if layer.len() + size > capacity {
                return Err(protocol("the scanner overflowed a layer buffer"));
            }
            layer.extend_from_slice(&block[..size]);
        }
        Ok(true)
    }

    /// The end-of-scan sequence. Waits for the head to go home.
    fn end_scan(&mut self) -> Result<()> {
        self.status = Status::Idle;
        self.send_pkt(PKT_GO_IDLE, 0)?;
        self.wait_ack()?;
        self.wait_ack()?;
        self.send_pkt(PKT_UNKNOWN_1, 0)?;
        self.wait_ack()?;
        self.send_pkt(PKT_RESET, 0)?;
        self.usb.sleep(Duration::from_secs(2));
        Ok(())
    }

    /// Moves complete pixels from the layers to the queue, dropping the padding
    /// each line carries past `width` and any pixels past the image's end.
    fn drain_layers(&mut self, scan: Scan) {
        let available = self.layers.available(scan.rgb);
        let image_size = self.width * scan.height();
        for pos in self.layers.read_offset..available {
            if pos % self.data_width >= self.width {
                continue;
            }
            if self.scanned_pixels >= image_size {
                break;
            }
            self.scanned_pixels += 1;
            if scan.rgb {
                self.queue.push(&[self.layers.r[pos], self.layers.g[pos], self.layers.b[pos]]);
            } else {
                self.queue.push(&[self.layers.r[pos]]);
            }
        }
        self.layers.read_offset = available;
        if self.layers.synchronized(scan.rgb) {
            self.layers = Layers::default();
        }
    }
}

impl Device for Hpljm1005 {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        let scan = Scan::from_settings(&self.caps, settings)?;
        // The scanner reports the true width once data flows.
        let width = (scan.x2 - scan.x1) * scan.resolution / 100;
        Ok(scan.parameters(width as usize))
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let scan = Scan::from_settings(&self.caps, settings)?;
        self.finish();
        self.cancel = cancel.clone();
        self.layers = Layers::default();
        self.queue.clear();
        self.scanned_pixels = 0;
        cancel.check()?;

        self.send_pkt(PKT_RESET, 0)?;
        self.send_pkt(PKT_READ_STATUS, 0)?;
        let (status, _) = self.wait_ack()?;
        if status != 0 {
            return Err(device_error(format!("the scanner reported status {status}")));
        }
        self.send_pkt(PKT_READCONF, 0)?;
        let (_, size) = self.wait_ack()?;
        if size > 0 {
            let mut conf = vec![0; size];
            self.usb.bulk_in(&mut conf, TIMEOUT)?;
        }
        self.send_pkt(PKT_SETCONF, CONF_SIZE as u32)?;
        self.usb.bulk_out(&scan.conf(), TIMEOUT)?;
        self.wait_ack()?;

        self.send_pkt(PKT_START_SCAN, 0)?;
        self.wait_ack()?;
        for _ in 0..3 {
            self.skip_announced()?;
        }

        self.status = Status::Scanning;
        self.scan = Some(scan);
        // The first data packet carries the frame's width.
        self.get_data()?;
        Ok(scan.parameters(self.width))
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(scan) = self.scan else { return Err(invalid("no scan is running")) };
        self.cancel.check()?;
        loop {
            if !self.queue.is_empty() {
                return Ok(self.queue.pop_into(buf));
            }
            if self.layers.available(scan.rgb) > self.layers.read_offset {
                self.drain_layers(scan);
                continue;
            }
            if self.status == Status::Idle {
                return Ok(0);
            }
            self.cancel.check()?;
            self.get_data()?;
        }
    }

    fn finish(&mut self) {
        if self.status == Status::Scanning {
            // The scanner has to run to the end of the page before it goes idle.
            while matches!(self.get_data(), Ok(true)) {
                self.layers = Layers::default();
            }
            self.status = Status::Idle;
        }
        self.layers = Layers::default();
        self.queue.clear();
        self.scan = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::Area;
    use scan_usb::mock::{Mock, Transfer};
    use std::collections::VecDeque;

    fn pkt(command: u32, status: u32, size: u32) -> Vec<u8> {
        let mut p = vec![0u8; 32];
        p[0..4].copy_from_slice(&MAGIC_NUMBER.to_be_bytes());
        p[4..8].copy_from_slice(&command.to_be_bytes());
        p[16..20].copy_from_slice(&status.to_be_bytes());
        p[20..24].copy_from_slice(&size.to_be_bytes());
        p
    }

    fn data_header(layer: u32, width: u32, data_width: u32) -> Vec<u8> {
        let mut h = vec![0u8; 24];
        h[0..4].copy_from_slice(&layer.to_be_bytes());
        h[16..20].copy_from_slice(&width.to_be_bytes());
        h[20..24].copy_from_slice(&data_width.to_be_bytes());
        h
    }

    /// A device answering every read with the next queued packet.
    fn device(replies: Vec<Vec<u8>>) -> Mock {
        let mut replies: VecDeque<_> = replies.into();
        Mock::new(0x03f0, 0x3b17, move |t| match t {
            Transfer::BulkIn(_) => replies.pop_front().ok_or(scan_usb::Error::Timeout),
            _ => Ok(vec![]),
        })
    }

    fn settings(mode: &str, width: f64, height: f64) -> ScanSettings {
        ScanSettings {
            source: "Flatbed".into(),
            mode: mode.into(),
            resolution: 100,
            area: Area { x: 0.0, y: 0.0, width, height },
            gamma: None,
            extras: vec![],
        }
    }

    #[test]
    fn conf_packet_matches_send_conf() {
        let caps = capabilities();
        let scan = Scan::from_settings(&caps, &settings(MODE_COLOR, 216.0, 297.0)).unwrap();
        let conf = scan.conf();
        let w = |i| word(&conf, i);
        assert_eq!((w(0), w(1), w(2), w(3)), (0x15, 6, 6, 100));
        assert_eq!((w(4), w(7), w(8), w(10)), (1, 1, 0, 8));
        assert_eq!((w(15), w(24)), (2, 1));
        assert_eq!((w(16), w(17), w(18), w(19)), (0, 0, 0x490, 0x351));
        assert_eq!((w(22), w(23)), (0x491, 0x352));
        let gray = Scan::from_settings(&caps, &settings(MODE_GRAY, 216.0, 297.0)).unwrap();
        assert_eq!((word(&gray.conf(), 15), word(&gray.conf(), 24)), (6, 0));
    }

    #[test]
    fn small_areas_grow_to_the_minimum_zone() {
        let (first, last) = zone(0.0, 1.0, MAX_X_S, MAX_X_H);
        assert!(last - first >= MIN_SCAN_ZONE - 4, "{first}..{last}");
        assert_eq!(first, 0);
    }

    #[test]
    fn known_models_are_recognised() {
        assert_eq!((DRIVER.find)(0x03f0, 0x5717).unwrap().model, "LaserJet M1120n");
        assert!((DRIVER.find)(0x03f0, 0x0001).is_none());
    }

    /// Replies up to and including the start of data.
    fn startup() -> Vec<Vec<u8>> {
        vec![
            pkt(0, 0, 0),     // status
            pkt(0, 0, 0),     // readconf, nothing announced
            pkt(0, 0, 0),     // setconf ack
            pkt(0, 0, 0),     // start ack
            pkt(0, 0, 4),     // announces 4 bytes
            vec![1, 2, 3, 4], // which are discarded
            pkt(0, 0, 0),
            pkt(0, 0, 0),
        ]
    }

    fn shutdown(replies: &mut Vec<Vec<u8>>) {
        replies.push(pkt(PKT_END_DATA, 0, 0));
        replies.extend([pkt(0, 0, 0), pkt(0, 0, 0), pkt(0, 0, 0)]);
    }

    #[test]
    fn gray_scan_drops_line_padding() {
        // 2-pixel lines padded to 3 bytes; a 2 mm tall scan grows to the
        // minimum zone, so the image is well over one line.
        let mut replies = startup();
        replies.push(pkt(PKT_DATA, 0, 24 + 6));
        replies.push(data_header(GRAY_LAYER, 2, 3));
        replies.push(vec![10, 11, 0xff, 12, 13, 0xff]);
        shutdown(&mut replies);
        let mock = device(replies);
        let log = mock.log();
        let mut scanner = Hpljm1005::open(Box::new(mock));
        let params = scanner.start(&settings(MODE_GRAY, 216.0, 100.0), &CancelToken::new()).unwrap();
        assert_eq!((params.pixels_per_line, params.bytes_per_line), (2, 2));
        let mut out = vec![];
        let mut buf = [0; 16];
        loop {
            let n = scanner.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, [10, 11, 12, 13]);
        scanner.finish();

        let writes: Vec<u32> = log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|t| match t {
                Transfer::BulkOut(d) if d.len() == 32 => Some(word(d, 1)),
                _ => None,
            })
            .collect();
        assert_eq!(
            writes,
            [
                PKT_RESET,
                PKT_READ_STATUS,
                PKT_READCONF,
                PKT_SETCONF,
                PKT_START_SCAN,
                PKT_GO_IDLE,
                PKT_UNKNOWN_1,
                PKT_RESET
            ]
        );
    }

    #[test]
    fn color_layers_interleave_including_the_final_run() {
        let mut replies = startup();
        for (layer, bytes) in [(RED_LAYER, [1, 2]), (GREEN_LAYER, [3, 4]), (BLUE_LAYER, [5, 6])] {
            replies.push(pkt(PKT_DATA, 0, 24 + 2));
            replies.push(data_header(layer, 2, 2));
            replies.push(bytes.to_vec());
        }
        shutdown(&mut replies);
        let mut scanner = Hpljm1005::open(Box::new(device(replies)));
        scanner.start(&settings(MODE_COLOR, 216.0, 100.0), &CancelToken::new()).unwrap();
        let mut buf = [0; 16];
        let mut out = vec![];
        loop {
            let n = scanner.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, [1, 3, 5, 2, 4, 6]);
    }

    #[test]
    fn nonzero_status_fails_the_start() {
        let mut scanner = Hpljm1005::open(Box::new(device(vec![pkt(0, 1, 0)])));
        let err = scanner.start(&settings(MODE_GRAY, 216.0, 100.0), &CancelToken::new()).unwrap_err();
        assert_eq!(err.kind(), scan_core::ErrorKind::Device);
    }
}

//! Mustek `BearPaw` 1200F (MA-1509 chip) over USB.
//!
//! A port of SANE's `ma1509` backend. Untested on real hardware.
//!
//! The protocol is 8-byte SCSI-style commands over bulk endpoints. Lamp warm-up
//! takes 30 seconds from the moment the device is opened. SANE's custom gamma
//! vectors aren't exposed; the driver always sends the identity table SANE uses
//! when custom gamma is off. SANE has code for a transparency adapter but its
//! source option is permanently inactive, so only the flatbed is offered.

#![forbid(unsafe_code)]
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "scan geometry is bounded by the bed and fits the protocol's fields"
)]

use std::time::{Duration, Instant};

use scan_core::driver::{ByteQueue, device_error, invalid, mode, protocol, resolve};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, ExtraOption, MM_PER_INCH, OptionKind, OptionValue,
    Parameters, PixelLayout, Result, ScanSettings, SourceCaps, SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};

const TIMEOUT: Duration = Duration::from_secs(30);
const INQ_LEN: usize = 0x60;
const GAMMA_SIZE: usize = 1024;
const BUFFER_SIZE: usize = 128 * 1024;
const WARMUP: Duration = Duration::from_secs(30);
const WRITE_LIMIT: usize = 64 * 1024;
const READ_LIMIT: usize = 256 * 1024;
const CAL_PPL: usize = 5312;
const CAL_LINES: usize = 40;
const RESOLUTIONS: [u32; 9] = [50, 100, 150, 200, 300, 400, 450, 500, 600];
const INQUIRY: [u8; 8] = [0x12, 0x01, 0x00, 0x00, 0x00, 0x00, INQ_LEN as u8, 0x00];
const TEST_UNIT_READY: [u8; 8] = [0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00];
const SET_WINDOW: [u8; 8] = [0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00];
const MODE_LINEART: &str = "Lineart";
const MODE_COLOR: &str = "Color";
const DEFAULT_THRESHOLD: i32 = 128;

/// This backend, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: "ma1509",
    find: |vid, pid| (vid == 0x055f && pid == 0x0010).then_some(UsbModel { vendor: "Mustek", model: "BearPaw 1200F" }),
    open: |usb| Ok(Box::new(Ma1509::open(usb)?)),
    interface: 0,
};

fn capabilities() -> Capabilities {
    let threshold = ExtraOption {
        name: "threshold".into(),
        title: "Lineart threshold".into(),
        kind: OptionKind::Int { min: 0, max: 255 },
        default: OptionValue::Int(DEFAULT_THRESHOLD),
    };
    Capabilities {
        sources: vec![SourceCaps {
            name: "Flatbed".into(),
            kind: SourceKind::Flatbed,
            width_mm: 211.3,
            height_mm: 296.7,
            modes: vec![
                mode(MODE_COLOR, Channels::Rgb, 8, &RESOLUTIONS),
                mode("Gray", Channels::Gray, 8, &RESOLUTIONS),
                mode(MODE_LINEART, Channels::Gray, 1, &RESOLUTIONS),
            ],
            gamma: None,
            extras: vec![threshold],
        }],
    }
}

/// `set_window`'s geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    dpi: u16,
    tlx: u16,
    tly: u16,
    width: u16,
    height: u16,
    depth: u8,
    threshold: u8,
}

impl Window {
    fn from_settings(caps: &Capabilities, s: &ScanSettings) -> Result<Self> {
        let r = resolve(caps, s)?;
        let ppmm = f64::from(r.dpi) / MM_PER_INCH;
        let right = (s.area.x + s.area.width).min(r.source.width_mm);
        let bottom = (s.area.y + s.area.height).min(r.source.height_mm);
        let round = |mm: f64| (mm * ppmm + 0.5) as u16;
        let lineart = r.mode.layout.depth == 1;
        let quantum = if lineart { 64 } else { 8 };
        let width = (round(right - s.area.x) / quantum * quantum).max(quantum);
        let threshold = match s.extra("threshold") {
            None => DEFAULT_THRESHOLD as u8,
            Some(OptionValue::Int(v)) if (0..=255).contains(v) => *v as u8,
            Some(_) => return Err(invalid("bad value for threshold")),
        };
        let depth = match (lineart, r.mode.layout.channels) {
            (true, _) => 1,
            (false, Channels::Rgb) => 24,
            (false, Channels::Gray) => 8,
        };
        Ok(Self {
            dpi: r.dpi as u16,
            tlx: round(s.area.x),
            tly: round(s.area.y),
            width,
            height: round(bottom - s.area.y),
            depth,
            threshold,
        })
    }

    fn lineart(self) -> bool {
        self.depth == 1
    }

    /// The `set_window` packet.
    fn packet(self) -> [u8; 0x30] {
        let mut p = [0u8; 0x30];
        p[2..4].copy_from_slice(&self.dpi.to_be_bytes());
        p[8..10].copy_from_slice(&self.tlx.to_be_bytes());
        p[12..14].copy_from_slice(&self.tly.to_be_bytes());
        p[14] = 0x14;
        p[15] = 0xc0;
        p[16..18].copy_from_slice(&self.width.to_be_bytes());
        p[18] = 0x28;
        p[19] = 0x20;
        p[20..22].copy_from_slice(&self.height.to_be_bytes());
        p[23] = if self.lineart() { self.threshold } else { 0x80 };
        p[26] = self.depth;
        if !self.lineart() {
            p[41] = 0x02;
        }
        p
    }

    fn parameters(self) -> Parameters {
        let width = usize::from(self.width);
        let (channels, depth, bpl) = match self.depth {
            1 => (Channels::Gray, 1, width / 8),
            8 => (Channels::Gray, 8, width),
            _ => (Channels::Rgb, 8, width * 3),
        };
        Parameters {
            layout: PixelLayout { channels, depth },
            pixels_per_line: width,
            bytes_per_line: bpl,
            lines: usize::from(self.height),
        }
    }

    /// `start_read_data`'s size: one byte per pixel, even in colour.
    fn read_size(self) -> u32 {
        let total = u32::from(self.width) * u32::from(self.height);
        if self.lineart() { total / 8 } else { total }
    }
}

/// The shading correction for one sensor element from its average white level.
fn shading_factor(average: u32) -> u8 {
    let average = average.clamp(1, 255);
    (256 * 256 / average).saturating_sub(256).min(255) as u8
}

#[derive(Debug)]
struct Active {
    lineart: bool,
    remaining: usize,
    to_fetch: usize,
    cancel: CancelToken,
}

/// An open `BearPaw` 1200F.
#[derive(Debug)]
pub struct Ma1509 {
    usb: Box<dyn UsbTransport>,
    info: DeviceInfo,
    caps: Capabilities,
    lamp_on_since: Instant,
    active: Option<Active>,
    queue: ByteQueue,
}

impl Ma1509 {
    /// Opens the scanner, checks it's the expected model and turns on the lamp.
    ///
    /// # Errors
    /// Fails if the device doesn't answer like a `BearPaw` 1200F.
    pub fn open(usb: Box<dyn UsbTransport>) -> Result<Self> {
        let info = DRIVER.device_info(usb.ids());
        let mut s = Self {
            usb,
            info,
            caps: capabilities(),
            lamp_on_since: Instant::now(),
            active: None,
            queue: ByteQueue::new(),
        };
        let mut ready = [0u8; 4];
        s.cmd_read(TEST_UNIT_READY, &mut ready)?;
        let mut inquiry = [0u8; INQ_LEN];
        s.cmd_read(INQUIRY, &mut inquiry)?;
        if inquiry[0] & 0x1f != 0x06 {
            return Err(protocol("the device doesn't report itself as a scanner"));
        }
        if &inquiry[44..48] != b" B06" {
            return Err(device_error("unsupported MA-1509 model"));
        }
        s.turn_lamp(true)?;
        s.turn_lamp(true)?;
        Ok(s)
    }

    /// `ma1509_cmd` for a command that reads. Keeps reading until `data` is
    /// full; the C code spins forever on an empty read, here that's an error.
    fn cmd_read(&mut self, cmd: [u8; 8], data: &mut [u8]) -> Result<()> {
        self.usb.bulk_out(&cmd, TIMEOUT)?;
        let mut done = 0;
        while done < data.len() {
            let end = data.len().min(done + READ_LIMIT);
            let n = self.usb.bulk_in(&mut data[done..end], TIMEOUT)?;
            if n == 0 {
                return Err(protocol("the scanner sent no data"));
            }
            done += n;
        }
        Ok(())
    }

    /// `ma1509_cmd` for a command that writes.
    fn cmd_write(&mut self, cmd: [u8; 8], data: &[u8]) -> Result<()> {
        self.usb.bulk_out(&cmd, TIMEOUT)?;
        for chunk in data.chunks(WRITE_LIMIT) {
            self.usb.bulk_out(chunk, TIMEOUT)?;
        }
        Ok(())
    }

    fn turn_lamp(&mut self, on: bool) -> Result<()> {
        let mut p = [0u8; 0x30];
        p[0x28] = if on { 0x01 } else { 0x02 };
        self.cmd_write(SET_WINDOW, &p)?;
        self.lamp_on_since = Instant::now();
        Ok(())
    }

    /// Waits out what's left of the lamp's warm-up.
    fn warm_up(&mut self, cancel: &CancelToken) -> Result<()> {
        let mut left = WARMUP.saturating_sub(self.lamp_on_since.elapsed());
        while !left.is_zero() {
            cancel.check()?;
            let step = left.min(Duration::from_secs(1));
            self.usb.sleep(step);
            left -= step;
        }
        Ok(())
    }

    /// `calibration`: scans a white strip and uploads each channel's shading.
    fn calibrate(&mut self) -> Result<()> {
        let total = CAL_LINES * CAL_PPL;
        let mut buffer = vec![0u8; total * 3];
        let mut cmd = [0u8; 8];
        cmd[0] = 0x28;
        cmd[1] = 0x01;
        cmd[2] = 0x01;
        cmd[4] = (total >> 16) as u8;
        cmd[5] = (total >> 8) as u8;
        cmd[6] = total as u8;
        self.cmd_read(cmd, &mut buffer)?;

        let mut cmd = [0u8; 8];
        cmd[0] = 0x2a;
        cmd[2] = 0x01;
        cmd[5] = (CAL_PPL >> 8) as u8;
        cmd[6] = CAL_PPL as u8;
        let mut shading = vec![0u8; CAL_PPL];
        for color in 1..4usize {
            cmd[4] = color as u8;
            for (column, out) in shading.iter_mut().enumerate() {
                let sum: u32 =
                    (0..CAL_LINES).map(|line| u32::from(buffer[line * CAL_PPL * 3 + column * 3 + color - 1])).sum();
                *out = shading_factor(sum / CAL_LINES as u32);
            }
            self.cmd_write(cmd, &shading)?;
        }
        Ok(())
    }

    fn send_gamma(&mut self) -> Result<()> {
        let table: Vec<u8> = (0..GAMMA_SIZE).map(|i| (i * 256 / GAMMA_SIZE) as u8).collect();
        let mut cmd = [0u8; 8];
        cmd[0] = 0x2a;
        cmd[2] = 0x03;
        cmd[5] = (GAMMA_SIZE >> 8) as u8;
        cmd[6] = GAMMA_SIZE as u8;
        for color in 1..4 {
            cmd[4] = color;
            self.cmd_write(cmd, &table)?;
        }
        Ok(())
    }

    fn stop_scan(&mut self) -> Result<()> {
        self.usb.bulk_out(&[0x1b, 0x01, 0x00, 0, 0, 0, 0, 0], TIMEOUT)?;
        Ok(())
    }
}

impl Device for Ma1509 {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        Ok(Window::from_settings(&self.caps, settings)?.parameters())
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let window = Window::from_settings(&self.caps, settings)?;
        self.finish();
        self.warm_up(cancel)?;
        self.cmd_write(SET_WINDOW, &window.packet())?;
        let mut ready = [0u8; 4];
        self.cmd_read(TEST_UNIT_READY, &mut ready)?;
        if !window.lineart() {
            self.calibrate()?;
            self.send_gamma()?;
        }
        self.usb.bulk_out(&[0x1b, 0x01, 0x01, 0, 0, 0, 0, 0], TIMEOUT)?;
        let size = window.read_size().to_be_bytes();
        self.usb.bulk_out(&[0x28, 0x01, 0x00, size[0], size[1], size[2], size[3], 0], TIMEOUT)?;
        let params = window.parameters();
        let total = params.lines * params.bytes_per_line;
        self.active =
            Some(Active { lineart: window.lineart(), remaining: total, to_fetch: total, cancel: cancel.clone() });
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(active) = &self.active else { return Err(invalid("no scan is running")) };
        active.cancel.check()?;
        if active.remaining == 0 {
            return Ok(0);
        }
        if self.queue.is_empty() {
            let (want, lineart) = (BUFFER_SIZE.min(active.to_fetch), active.lineart);
            if want == 0 {
                return Err(protocol("the scanner sent less data than announced"));
            }
            let mut block = vec![0u8; want];
            let n = self.usb.bulk_in(&mut block, TIMEOUT)?;
            if n == 0 {
                return Err(protocol("the scanner sent no data"));
            }
            block.truncate(n);
            if lineart {
                // The scanner sends 1 for white; the API wants 1 for black.
                for b in &mut block {
                    *b = !*b;
                }
            }
            self.queue.push(&block);
            if let Some(active) = &mut self.active {
                active.to_fetch -= n;
            }
        }
        let n = self.queue.pop_into(buf);
        if let Some(active) = &mut self.active {
            active.remaining -= n;
        }
        Ok(n)
    }

    fn finish(&mut self) {
        if self.active.take().is_some() {
            self.queue.clear();
            // Best effort: nobody is left to report a failure to.
            let _ = self.stop_scan();
        }
    }
}

impl Drop for Ma1509 {
    fn drop(&mut self) {
        self.finish();
        let _ = self.turn_lamp(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::Area;
    use scan_core::driver::{full_area_settings, scan_frame};
    use scan_usb::mock::{Mock, Transfer};

    fn device(data_byte: u8) -> Mock {
        Mock::new(0x055f, 0x0010, move |t| match t {
            Transfer::BulkIn(len) if *len == INQ_LEN => {
                let mut r = vec![0u8; INQ_LEN];
                r[0] = 0x06;
                r[44..48].copy_from_slice(b" B06");
                Ok(r)
            }
            Transfer::BulkIn(len) => Ok(vec![data_byte; *len]),
            _ => Ok(vec![]),
        })
    }

    #[test]
    fn other_models_are_rejected() {
        let mock = Mock::new(0x055f, 0x0010, |t| match t {
            Transfer::BulkIn(len) => {
                let mut r = vec![0u8; *len];
                r[0] = 0x06;
                Ok(r)
            }
            _ => Ok(vec![]),
        });
        assert!(Ma1509::open(Box::new(mock)).is_err());
    }

    #[test]
    fn window_packet_matches_set_window() {
        let caps = capabilities();
        let mut s = full_area_settings(&caps, MODE_COLOR, 100);
        s.area = Area { x: 25.4, y: 50.8, width: 101.6, height: 25.4 };
        let w = Window::from_settings(&caps, &s).unwrap();
        assert_eq!((w.tlx, w.tly, w.width, w.height), (100, 200, 400, 100));
        let p = w.packet();
        assert_eq!(&p[2..4], [0, 100]);
        assert_eq!(&p[8..10], [0, 100]);
        assert_eq!(&p[12..14], [0, 200]);
        assert_eq!(&p[14..16], [0x14, 0xc0]);
        assert_eq!(&p[16..18], [1, 144]);
        assert_eq!(&p[18..20], [0x28, 0x20]);
        assert_eq!((p[23], p[26], p[40], p[41]), (0x80, 24, 0, 2));
    }

    #[test]
    fn widths_round_down_to_the_mode_quantum() {
        let caps = capabilities();
        let mut s = full_area_settings(&caps, MODE_LINEART, 100);
        s.area = Area { x: 0.0, y: 0.0, width: 30.0, height: 10.0 };
        let w = Window::from_settings(&caps, &s).unwrap();
        assert_eq!(w.width, 64, "118 px rounds down to a multiple of 64");
        let params = w.parameters();
        assert_eq!((params.bytes_per_line, params.layout.depth), (8, 1));
        assert_eq!(w.packet()[41], 0, "lineart has no extra flag");
        s.resolution = 600;
        s.area.width = 0.1;
        assert_eq!(Window::from_settings(&caps, &s).unwrap().width, 64, "never zero");
    }

    #[test]
    fn shading_factors_are_clamped() {
        assert_eq!(shading_factor(0), 255);
        assert_eq!(shading_factor(255), 1);
        assert_eq!(shading_factor(128), 255);
        assert_eq!(shading_factor(200), 71);
    }

    fn small(caps: &Capabilities, mode: &str) -> ScanSettings {
        let mut s = full_area_settings(caps, mode, 50);
        s.area = Area { x: 0.0, y: 0.0, width: 40.0, height: 2.0 };
        s
    }

    #[test]
    fn lineart_skips_calibration_and_inverts() {
        let mock = device(0x0f);
        let log = mock.log();
        let mut scanner = Ma1509::open(Box::new(mock)).unwrap();
        let cancel_free = small(scanner.capabilities(), MODE_LINEART);
        let (params, image) = scan_frame(&mut scanner, &cancel_free).unwrap();
        assert_eq!(image.len(), params.lines * params.bytes_per_line);
        assert!(image.iter().all(|&b| b == 0xf0));
        let log = log.lock().unwrap();
        assert!(!log.iter().any(|t| matches!(t, Transfer::BulkOut(c) if c[0] == 0x2a)));
        assert!(log.contains(&Transfer::BulkOut(vec![0x1b, 1, 0, 0, 0, 0, 0, 0])), "scan is stopped");
    }

    #[test]
    fn color_uploads_shading_and_gamma_per_channel() {
        let mock = device(200);
        let log = mock.log();
        let mut scanner = Ma1509::open(Box::new(mock)).unwrap();
        let settings = small(scanner.capabilities(), MODE_COLOR);
        let (params, image) = scan_frame(&mut scanner, &settings).unwrap();
        assert_eq!(image.len(), params.lines * params.bytes_per_line);
        let log = log.lock().unwrap();
        let uploads: Vec<(u8, u8)> = log
            .iter()
            .filter_map(|t| match t {
                Transfer::BulkOut(c) if c.len() == 8 && c[0] == 0x2a => Some((c[2], c[4])),
                _ => None,
            })
            .collect();
        assert_eq!(uploads, [(1, 1), (1, 2), (1, 3), (3, 1), (3, 2), (3, 3)]);
        assert!(log.contains(&Transfer::BulkOut(vec![shading_factor(200); CAL_PPL])));
    }

    #[test]
    fn warm_up_waits_for_the_lamp_and_honours_cancel() {
        let mut scanner = Ma1509::open(Box::new(device(0))).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(scanner.warm_up(&cancel).unwrap_err().is_cancelled());
    }

    #[test]
    fn dropping_turns_the_lamp_off() {
        let mock = device(0);
        let log = mock.log();
        drop(Ma1509::open(Box::new(mock)).unwrap());
        let log = log.lock().unwrap();
        let Some(Transfer::BulkOut(last)) = log.last() else { panic!() };
        assert_eq!(last[0x28], 0x02);
    }
}

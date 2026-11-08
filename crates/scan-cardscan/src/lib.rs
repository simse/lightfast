//! Corex `CardScan` 600c and 800c business card scanners over USB.
//!
//! A port of SANE's `cardscan` backend. Untested on real hardware.
//!
//! These are small sheet-fed scanners with a fixed 1208-pixel line and no
//! settings besides colour or gray. The host applies the device's stored
//! calibration to every line. The page ends when the scanner has reported
//! no paper for [`MAX_PAPERLESS_LINES`] lines, so [`Parameters::lines`] is an
//! upper bound. SANE's `has_cal_buffer` and `lines_per_block` config-file
//! options stay at their defaults.

#![forbid(unsafe_code)]
#![expect(clippy::cast_possible_truncation, reason = "the calibration maths stays within a byte")]

use std::time::Duration;

use scan_core::driver::{ByteQueue, device_error, invalid, mode};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, Error, ErrorKind, Parameters, PixelLayout, Result,
    ScanSettings, SourceCaps, SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};

const HEADER_SIZE: usize = 64;
const PIXELS_PER_LINE: usize = 1208;
const CAL_COLOR_SIZE: usize = PIXELS_PER_LINE * 3;
const CAL_GRAY_SIZE: usize = PIXELS_PER_LINE;
const MAX_PAPERLESS_LINES: usize = 210;
const LINES_PER_BLOCK: usize = 16;
const TIMEOUT: Duration = Duration::from_millis(10_000);
/// The sensor's resolution. SANE doesn't report one; this is what the 1208
/// pixels across a 4-inch card come to.
const DPI: u32 = 300;
/// SANE reports an unknown length; this bounds a card or receipt.
const MAX_LINES: usize = 4200;
const MODE_COLOR: &str = "Color";

/// This backend, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: "cardscan",
    find: |vid, pid| match (vid, pid) {
        (0x08f0, 0x0005) => Some(UsbModel { vendor: "CardScan", model: "800c" }),
        (0x08f0, 0x0002) => Some(UsbModel { vendor: "CardScan", model: "600c" }),
        (0x0451, 0x6250) => Some(UsbModel { vendor: "Sanford", model: "800c" }),
        _ => None,
    },
    open: |usb| Ok(Box::new(CardScan::open(usb)?)),
    interface: 0,
};

fn capabilities() -> Capabilities {
    let mm = |px: usize| f64::from(u32::try_from(px).unwrap_or(u32::MAX)) / f64::from(DPI) * scan_core::MM_PER_INCH;
    Capabilities {
        sources: vec![SourceCaps {
            name: "Feeder".into(),
            kind: SourceKind::Feeder,
            width_mm: mm(PIXELS_PER_LINE),
            height_mm: mm(MAX_LINES),
            modes: vec![mode(MODE_COLOR, Channels::Rgb, 8, &[DPI]), mode("Gray", Channels::Gray, 8, &[DPI])],
            gamma: None,
            extras: vec![],
        }],
    }
}

/// `byte` corrected for a sensor element's black level and white range.
fn corrected(byte: u8, black: u8, white_range: u8) -> u8 {
    let byte = byte.saturating_sub(black);
    if byte >= white_range { 255 } else { (u32::from(byte) * 255 / u32::from(white_range)) as u8 }
}

#[derive(Debug)]
struct Calibration {
    color_black: Vec<u8>,
    color_white: Vec<u8>,
    gray_black: Vec<u8>,
    gray_white: Vec<u8>,
}

impl Calibration {
    /// Parses `load_calibration`'s reply: for each of R, G, B, then gray, a
    /// black and a white line. Stored white becomes white minus black.
    fn parse(reply: &[u8]) -> Self {
        let line = |index: usize| {
            let start = HEADER_SIZE + index * PIXELS_PER_LINE;
            &reply[start..start + PIXELS_PER_LINE]
        };
        let mut color_black = Vec::with_capacity(CAL_COLOR_SIZE);
        let mut color_white = Vec::with_capacity(CAL_COLOR_SIZE);
        for channel in 0..3 {
            color_black.extend_from_slice(line(channel * 2));
            color_white.extend_from_slice(line(channel * 2 + 1));
        }
        let gray_black = line(6).to_vec();
        let gray_white = line(7).to_vec();
        let minus = |white: &[u8], black: &[u8]| -> Vec<u8> {
            white.iter().zip(black).map(|(w, b)| w.wrapping_sub(*b)).collect()
        };
        Self {
            color_white: minus(&color_white, &color_black),
            gray_white: minus(&gray_white, &gray_black),
            color_black,
            gray_black,
        }
    }

    /// Corrects a block of gray lines (`read_from_scanner_gray`).
    fn gray(&self, block: &[u8]) -> Vec<u8> {
        block
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                let j = i % PIXELS_PER_LINE;
                corrected(b, self.gray_black[j], self.gray_white[j])
            })
            .collect()
    }

    /// Corrects a block of colour lines (`read_from_scanner_color`). Each line
    /// holds the channels as separate runs, in B, G, R order.
    fn color(&self, block: &[u8]) -> Vec<u8> {
        let stride = PIXELS_PER_LINE * 3;
        let mut out = vec![0; block.len()];
        for (line_in, line_out) in block.chunks_exact(stride).zip(out.chunks_exact_mut(stride)) {
            for j in 0..PIXELS_PER_LINE {
                for k in 0..3 {
                    let offset = PIXELS_PER_LINE * (2 - k) + j;
                    line_out[j * 3 + k] =
                        corrected(line_in[offset], self.color_black[offset], self.color_white[offset]);
                }
            }
        }
        out
    }
}

#[derive(Debug)]
struct Active {
    rgb: bool,
    paperless_lines: usize,
    ended: bool,
    cancel: CancelToken,
}

/// An open card scanner.
#[derive(Debug)]
pub struct CardScan {
    usb: Box<dyn UsbTransport>,
    info: DeviceInfo,
    caps: Capabilities,
    calibration: Calibration,
    active: Option<Active>,
    queue: ByteQueue,
}

fn frame(rgb: bool) -> Parameters {
    let channels = if rgb { Channels::Rgb } else { Channels::Gray };
    Parameters {
        layout: PixelLayout { channels, depth: 8 },
        pixels_per_line: PIXELS_PER_LINE,
        bytes_per_line: PIXELS_PER_LINE * channels.count(),
        lines: MAX_LINES,
    }
}

impl CardScan {
    /// Opens the scanner and reads its calibration.
    ///
    /// # Errors
    /// Fails if the calibration can't be read.
    pub fn open(mut usb: Box<dyn UsbTransport>) -> Result<Self> {
        let info = DRIVER.device_info(usb.ids());
        let mut reply = vec![0u8; HEADER_SIZE + CAL_COLOR_SIZE * 2 + CAL_GRAY_SIZE * 2];
        do_cmd(usb.as_mut(), &[0x45, 0x00, 0x00], &mut reply)?;
        Ok(Self {
            usb,
            info,
            caps: capabilities(),
            calibration: Calibration::parse(&reply),
            active: None,
            queue: ByteQueue::new(),
        })
    }

    fn is_rgb(&self, settings: &ScanSettings) -> Result<bool> {
        let r = scan_core::driver::resolve(&self.caps, settings)?;
        Ok(r.mode.layout.channels == Channels::Rgb)
    }

    /// Waits for the lamp, checking for paper (`heat_lamp_*`). The C code polls
    /// up to 10 times with no delay and reports busy if the lamp stays cold.
    fn heat_lamp(&mut self, rgb: bool) -> Result<()> {
        let gray_cmd: &[u8] = &[0x12, 0x06, 0x00, 0x00, 0x01, 0x60, 0x00, 0x61, 0x00];
        let color_cmd: &[u8] = &[0x18, 0x07, 0x00, 0x00, 0x01, 0x60, 0x00, 0x61, 0x00, 0x07];
        let (cmd, samples) = if rgb { (color_cmd, 3) } else { (gray_cmd, 1) };
        let mut reply = vec![0u8; HEADER_SIZE + samples];
        for _ in 0..10 {
            do_cmd(self.usb.as_mut(), cmd, &mut reply)?;
            if reply[1] == 0 {
                return Err(device_error("the feeder is empty"));
            }
            if reply[HEADER_SIZE..].iter().all(|&s| s < 0x20) {
                return Ok(());
            }
        }
        Err(Error::new(ErrorKind::Busy, "the scanner's lamp is still warming up"))
    }

    /// Reads and corrects a block of lines; counts it if the paper has gone.
    fn read_block(&mut self, rgb: bool) -> Result<bool> {
        let lines = LINES_PER_BLOCK as u8;
        let (cmd, size) = if rgb {
            (vec![0x18, 0x07, 0x00, 0x01, lines, 0x60, 0x00, 0x18, 0x05, 0x07], LINES_PER_BLOCK * PIXELS_PER_LINE * 3)
        } else {
            (vec![0x12, 0x06, 0x00, 0x01, lines, 0x60, 0x00, 0x18, 0x05], LINES_PER_BLOCK * PIXELS_PER_LINE)
        };
        let mut reply = vec![0u8; HEADER_SIZE + size];
        do_cmd(self.usb.as_mut(), &cmd, &mut reply)?;
        let block = &reply[HEADER_SIZE..];
        let corrected = if rgb { self.calibration.color(block) } else { self.calibration.gray(block) };
        self.queue.push(&corrected);
        Ok(reply[1] == 0)
    }

    fn power_down(&mut self) {
        let mut reply = [0u8; 6];
        for _ in 0..5 {
            if do_cmd(self.usb.as_mut(), &[0x21, 0x02, 0x00, 0x0a, 0x00], &mut reply).is_err() {
                break;
            }
        }
    }
}

/// `do_cmd`: writes a command and reads a reply into a zeroed `reply`. A short
/// reply isn't an error; the rest stays zero.
fn do_cmd(usb: &mut dyn UsbTransport, command: &[u8], reply: &mut [u8]) -> Result<()> {
    usb.bulk_out(command, TIMEOUT)?;
    reply.fill(0);
    usb.bulk_in(reply, TIMEOUT)?;
    Ok(())
}

impl Device for CardScan {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        Ok(frame(self.is_rgb(settings)?))
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let rgb = self.is_rgb(settings)?;
        self.finish();
        cancel.check()?;
        self.heat_lamp(rgb)?;
        self.active = Some(Active { rgb, paperless_lines: 0, ended: false, cancel: cancel.clone() });
        Ok(frame(rgb))
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(active) = &self.active else { return Err(invalid("no scan is running")) };
        active.cancel.check()?;
        if self.queue.is_empty() {
            let (rgb, ended, paperless) = (active.rgb, active.ended, active.paperless_lines);
            if ended {
                return Ok(0);
            }
            if paperless >= MAX_PAPERLESS_LINES {
                self.power_down();
                if let Some(active) = &mut self.active {
                    active.ended = true;
                }
                return Ok(0);
            }
            let no_paper = self.read_block(rgb)?;
            if let (true, Some(active)) = (no_paper, &mut self.active) {
                active.paperless_lines += LINES_PER_BLOCK;
            }
        }
        Ok(self.queue.pop_into(buf))
    }

    fn finish(&mut self) {
        self.active = None;
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::driver::{full_area_settings, scan_frame};
    use scan_usb::mock::{Mock, Transfer};

    const CAL_LEN: usize = HEADER_SIZE + CAL_COLOR_SIZE * 2 + CAL_GRAY_SIZE * 2;

    /// Black level 10 and white 110 everywhere: range 100. Paper for `pages`
    /// blocks. Lamp ready immediately.
    fn device(mut blocks_with_paper: usize) -> Mock {
        let mut last = vec![];
        Mock::new(0x08f0, 0x0005, move |t| match t {
            Transfer::BulkOut(cmd) => {
                last.clone_from(cmd);
                Ok(vec![])
            }
            Transfer::BulkIn(len) if *len == CAL_LEN => {
                let mut r = vec![0; CAL_LEN];
                for (i, line) in r[HEADER_SIZE..].as_chunks_mut::<PIXELS_PER_LINE>().0.iter_mut().enumerate() {
                    line.fill(if i % 2 == 0 { 10 } else { 110 });
                }
                Ok(r)
            }
            Transfer::BulkIn(len) if *len < HEADER_SIZE => Ok(vec![0; *len]),
            Transfer::BulkIn(len) => {
                let mut r = vec![60u8; *len];
                let paper = blocks_with_paper > 0;
                if last[3] == 0x01 && paper {
                    blocks_with_paper -= 1;
                }
                r[1] = u8::from(paper || last[3] == 0);
                r[HEADER_SIZE..].fill(if last[3] == 0 { 0 } else { 60 });
                Ok(r)
            }
            _ => Ok(vec![]),
        })
    }

    #[test]
    fn correction_clamps_and_scales() {
        assert_eq!(corrected(5, 10, 100), 0);
        assert_eq!(corrected(60, 10, 100), 127);
        assert_eq!(corrected(200, 10, 100), 255);
        assert_eq!(corrected(0, 0, 0), 255);
    }

    #[test]
    fn colour_runs_become_rgb_pixels_in_reverse_channel_order() {
        let mut reply = vec![0u8; CAL_LEN];
        for line in reply[HEADER_SIZE..].as_chunks_mut::<PIXELS_PER_LINE>().0.iter_mut().skip(1).step_by(2) {
            line.fill(255);
        }
        let cal = Calibration::parse(&reply);
        let mut block = vec![0u8; PIXELS_PER_LINE * 3];
        block[0] = 1; // first run: blue
        block[PIXELS_PER_LINE] = 2; // green
        block[PIXELS_PER_LINE * 2] = 3; // red
        assert_eq!(&cal.color(&block)[..3], [3, 2, 1]);
    }

    #[test]
    fn page_ends_after_paperless_lines_and_powers_down() {
        let mock = device(2);
        let log = mock.log();
        let mut scanner = CardScan::open(Box::new(mock)).unwrap();
        let settings = full_area_settings(scanner.capabilities(), "Gray", DPI);
        let (params, image) = scan_frame(&mut scanner, &settings).unwrap();
        assert_eq!(params.bytes_per_line, PIXELS_PER_LINE);
        // 2 blocks with paper, then 14 paperless ones to reach 210 lines.
        let lines = image.len() / PIXELS_PER_LINE;
        assert_eq!(lines, (2 + 14) * LINES_PER_BLOCK);
        assert!(image.iter().all(|&b| b == 127));
        let log = log.lock().unwrap();
        let downs = log.iter().filter(|t| matches!(t, Transfer::BulkOut(c) if c[0] == 0x21)).count();
        assert_eq!(downs, 5);
    }

    #[test]
    fn empty_feeder_fails_the_start() {
        let mock = Mock::new(0x08f0, 0x0002, |t| match t {
            Transfer::BulkIn(len) => Ok(vec![0; *len]),
            _ => Ok(vec![]),
        });
        let mut scanner = CardScan::open(Box::new(mock)).unwrap();
        let settings = full_area_settings(scanner.capabilities(), MODE_COLOR, DPI);
        let err = scanner.start(&settings, &CancelToken::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Device);
    }

    #[test]
    fn recognises_all_models() {
        assert_eq!((DRIVER.find)(0x0451, 0x6250).unwrap().vendor, "Sanford");
        assert_eq!((DRIVER.find)(0x08f0, 0x0002).unwrap().model, "600c");
    }
}

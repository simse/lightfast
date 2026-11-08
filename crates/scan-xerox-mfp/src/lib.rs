//! Samsung, Xerox and Dell laser MFPs over USB and TCP.
//!
//! A port of SANE's `xerox_mfp` backend. Untested on real hardware.
//!
//! The protocol is small SCSI-like commands. The device reports its abilities
//! in an inquiry reply, then sends the image in blocks: plain lines, separate
//! colour bands, or a JPEG per block on models that offer it. The host trims
//! each page to the requested size and pads a short page with white.
//!
//! A TCP device has no discovery in SANE, only a config line. Here it's the
//! device name `xerox_mfp_tcp:<host>[:<port>]`.
//!
//! Deviations from SANE: the ADF and Auto sources are only offered when the
//! inquiry reports an ADF, and a JPEG that won't decode is an error rather
//! than a white page.

#![forbid(unsafe_code)]
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "protocol fields, and geometry bounded by the bed"
)]

mod link;
mod models;

use std::time::Duration;

use scan_core::driver::{NetDriver, invalid, mode, protocol, resolve};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, Error, ErrorKind, ExtraOption, MM_PER_INCH, OptionKind,
    OptionValue, Parameters, PixelLayout, Result, ScanSettings, SourceCaps, SourceKind,
};
use scan_usb::{UsbDriver, UsbModel, UsbTransport};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::link::{Link, TcpLink, UsbLink};

const BACKEND: &str = "xerox_mfp";
const TCP_BACKEND: &str = "xerox_mfp_tcp";
const PNT_PER_MM: f64 = 1200.0 / MM_PER_INCH;
const PADDING_SIZE: usize = 16;
const DATASIZE: usize = 0x1_0000;
const DATAMASK: usize = DATASIZE - 1;
const USB_BLOCK_SIZE: usize = 512;

const CMD_ABORT: u8 = 0x06;
const CMD_INQUIRY: u8 = 0x12;
const CMD_RESERVE_UNIT: u8 = 0x16;
const CMD_RELEASE_UNIT: u8 = 0x17;
const CMD_SET_WINDOW: u8 = 0x24;
const CMD_READ: u8 = 0x28;
const CMD_READ_IMAGE: u8 = 0x29;
const CMD_OBJECT_POSITION: u8 = 0x31;
const REQ_CODE_A: u8 = 0x1b;
const REQ_CODE_B: u8 = 0xa8;
const RES_CODE: u8 = 0xa8;
const STATUS_CHECK: u8 = 0x02;
const STATUS_CANCEL: u8 = 0x04;
const STATUS_BUSY: u8 = 0x08;
const MSG_PRODUCT_INFO: u8 = 0x10;
const MSG_SCANNING_PARAM: u8 = 0x30;
const MSG_END_BLOCK: u8 = 0x81;
const STATE_NO_ERROR: u16 = 0x001;
const STATE_NO_DOCUMENT: u16 = 0x010;
const STATE_DOCUMENT_JAM: u16 = 0x020;
const STATE_COVER_OPEN: u16 = 0x040;
const STATE_WARMING: u16 = 0x080;
const STATE_LOCKING: u16 = 0x100;
const STATE_INVALID_AREA: u16 = 0x200;
const MODE_LINEART: u8 = 0x00;
const MODE_HALFTONE: u8 = 0x01;
const MODE_GRAY8: u8 = 0x03;
const MODE_RGB24: u8 = 0x05;
const DOC_ADF: u8 = 0x20;
const DOC_FLATBED: u8 = 0x40;
const DOC_AUTO: u8 = 0x80;

const INQ_DPI_BITS: [u32; 20] = [75, 150, 0, 0, 200, 300, 0, 0, 600, 0, 0, 1200, 100, 0, 0, 2400, 0, 4800, 0, 9600];
const RES_DPI_CODES: [u32; 14] = [75, 0, 150, 0, 0, 300, 0, 600, 1200, 200, 100, 2400, 4800, 9600];

const SOURCE_FLATBED: &str = "Flatbed";
const SOURCE_ADF: &str = "ADF";
const SOURCE_AUTO: &str = "Auto";
const NAME_LINEART: &str = "Lineart";
const NAME_HALFTONE: &str = "Halftone";
const NAME_GRAY: &str = "Gray";
const NAME_COLOR: &str = "Color";
const JPEG_YES: &str = "Yes";
const JPEG_NO: &str = "No";

/// This backend over USB, for the registry.
pub const DRIVER: UsbDriver = UsbDriver {
    name: BACKEND,
    find: |vid, pid| {
        models::MODELS.iter().find(|m| m.0 == vid && m.1 == pid).map(|m| UsbModel { vendor: m.2, model: m.3 })
    },
    open: |usb| Ok(Box::new(XeroxMfp::open_usb(usb)?)),
    interface: 0,
};

/// This backend over TCP, for the registry. SANE has no discovery for it.
pub const TCP_DRIVER: NetDriver = NetDriver {
    name: TCP_BACKEND,
    discover: Vec::new,
    open: |address| {
        let name = format!("{TCP_BACKEND}:{address}");
        Ok(Box::new(XeroxMfp::open_link(Box::new(TcpLink::connect(address)?), name)?))
    },
};

/// How a command turned out, once its reply was well-formed.
#[derive(Debug)]
enum State {
    Good,
    Busy,
    Failed(Error),
}

/// `resolv_state`.
fn resolve_state(state: u16) -> State {
    let device = |m: &str| State::Failed(Error::new(ErrorKind::Device, m));
    if state & STATE_DOCUMENT_JAM != 0 {
        device("the document is jammed")
    } else if state & STATE_NO_DOCUMENT != 0 {
        device("no document is loaded")
    } else if state & STATE_COVER_OPEN != 0 {
        device("the cover is open")
    } else if state & STATE_INVALID_AREA != 0 {
        State::Failed(invalid("the scanner rejected the scan area"))
    } else if state & STATE_WARMING != 0 {
        State::Failed(Error::new(ErrorKind::Busy, "the lamp is warming up, please retry"))
    } else if state & STATE_LOCKING != 0 {
        device("the scanner mechanism is locked for transport")
    } else if state & !STATE_NO_ERROR != 0 {
        State::Busy
    } else {
        State::Good
    }
}

/// The parts of the inquiry reply the driver uses (`dev_inquiry`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inquiry {
    vendor: String,
    model: String,
    dpis: Vec<u32>,
    max_win_width: u32,
    max_len_adf: u32,
    max_len_fb: u32,
    line_order: u8,
    compression_types: u8,
    has_adf: bool,
    doc_loaded: bool,
}

fn be32(res: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([res[at], res[at + 1], res[at + 2], res[at + 3]])
}

fn adf_status(res: &[u8]) -> (bool, bool) {
    let has_adf = res[0x26] & 0x03 != 0;
    (has_adf, res[0x35] == 0x02 && has_adf)
}

impl Inquiry {
    fn parse(res: &[u8]) -> Result<Self> {
        if res[3] != MSG_PRODUCT_INFO {
            return Err(protocol("unexpected inquiry reply"));
        }
        let text = &res[4..0x24];
        let vendor_len = text.iter().position(|&b| b == 0 || b == b' ').unwrap_or(text.len());
        let rest = &text[vendor_len..];
        let start = rest.iter().position(|&b| b != 0 && b != b' ').unwrap_or(rest.len());
        let model = &rest[start..];
        let model_len = model.iter().position(|&b| b == 0).unwrap_or(model.len());
        let string = |b: &[u8]| String::from_utf8_lossy(b).trim_end().to_owned();

        let resolutions = u32::from(res[0x37]) << 16 | u32::from(res[0x24]) << 8 | u32::from(res[0x25]);
        let mut dpis: Vec<u32> = INQ_DPI_BITS
            .iter()
            .enumerate()
            .filter(|&(i, &dpi)| dpi != 0 && resolutions & (1 << i) != 0)
            .map(|(_, &dpi)| dpi)
            .collect();
        dpis.sort_unstable();
        if dpis.is_empty() {
            return Err(protocol("the scanner reports no resolutions"));
        }
        let (has_adf, doc_loaded) = adf_status(res);
        Ok(Self {
            vendor: string(&text[..vendor_len]),
            model: string(&model[..model_len]),
            dpis,
            max_win_width: be32(res, 0x28),
            max_len_adf: be32(res, 0x38),
            max_len_fb: be32(res, 0x3c),
            line_order: res[0x31],
            compression_types: res[0x32],
            has_adf,
            doc_loaded,
        })
    }

    /// `isSupportedDevice`: colour JPEG, minus models known to get it wrong.
    fn supports_jpeg(&self) -> bool {
        const PREFIXES: [&str; 3] = ["SCX-4500W", "C460", "M288x"];
        const CONTAINS: [&str; 6] = ["SCX-472", "WorkCentre 3220", "WorkCentre 3225", "CLX-3170", "4x24", "4x28"];
        self.compression_types & (1 << 6) != 0
            && !PREFIXES.iter().any(|p| self.model.starts_with(p))
            && !CONTAINS.iter().any(|c| self.model.contains(c))
    }
}

fn capabilities(inq: &Inquiry) -> Capabilities {
    let mm = |points: u32| f64::from(points) / PNT_PER_MM;
    let threshold = ExtraOption {
        name: "threshold".into(),
        title: "Threshold (%)".into(),
        kind: OptionKind::Int { min: 30, max: 70 },
        default: OptionValue::Int(50),
    };
    let mut extras = vec![threshold];
    if inq.supports_jpeg() {
        extras.push(ExtraOption {
            name: "jpeg".into(),
            title: "JPEG compression".into(),
            kind: OptionKind::Choice(vec![JPEG_YES.into(), JPEG_NO.into()]),
            default: OptionValue::Choice(JPEG_YES.into()),
        });
    }
    let source = |name: &str, kind, length| SourceCaps {
        name: name.into(),
        kind,
        width_mm: mm(inq.max_win_width),
        height_mm: mm(length),
        modes: vec![
            mode(NAME_COLOR, Channels::Rgb, 8, &inq.dpis),
            mode(NAME_GRAY, Channels::Gray, 8, &inq.dpis),
            mode(NAME_LINEART, Channels::Gray, 1, &inq.dpis),
            mode(NAME_HALFTONE, Channels::Gray, 1, &inq.dpis),
        ],
        gamma: None,
        extras: extras.clone(),
    };
    let mut sources = vec![source(SOURCE_FLATBED, SourceKind::Flatbed, inq.max_len_fb)];
    if inq.has_adf {
        sources.push(source(SOURCE_ADF, SourceKind::Feeder, inq.max_len_adf));
        let auto_length = if inq.doc_loaded { inq.max_len_adf } else { inq.max_len_fb };
        sources.push(source(SOURCE_AUTO, SourceKind::Flatbed, auto_length));
    }
    Capabilities { sources }
}

/// Rounds through SANE's 16.16 fixed point, truncating like `SANE_FIX`.
fn fixed(v: f64) -> f64 {
    f64::from((v * 65536.0) as i32) / 65536.0
}

/// `fix_window` and `set_parameters`: the scan as the device sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    dpi: u32,
    composition: u8,
    threshold: u8,
    doc_source: u8,
    win_width: i32,
    win_len: i32,
    off_x: f64,
    off_y: f64,
    jpeg: bool,
    /// The trimmed output's geometry before the device's first block.
    pixels: i32,
    bytes: i32,
    lines: i32,
}

impl Window {
    fn new(caps: &Capabilities, inq: &Inquiry, s: &ScanSettings) -> Result<Self> {
        let r = resolve(caps, s)?;
        let composition = match r.mode.name.as_str() {
            NAME_LINEART => MODE_LINEART,
            NAME_HALFTONE => MODE_HALFTONE,
            NAME_GRAY => MODE_GRAY8,
            _ => MODE_RGB24,
        };
        let doc_source = match r.source.name.as_str() {
            SOURCE_ADF => DOC_ADF,
            SOURCE_AUTO => DOC_AUTO,
            _ => DOC_FLATBED,
        };
        let threshold = match s.extra("threshold") {
            None => 50,
            Some(OptionValue::Int(v)) => (*v).clamp(30, 70),
            Some(_) => return Err(invalid("bad value for threshold")),
        };
        let jpeg_wanted = match s.extra("jpeg") {
            None => true,
            Some(OptionValue::Choice(v)) => v == JPEG_YES,
            Some(_) => return Err(invalid("bad value for jpeg")),
        };
        let jpeg = inq.supports_jpeg() && jpeg_wanted;

        let left = fixed(s.area.x);
        let top = fixed(s.area.y);
        let right = fixed((s.area.x + s.area.width).min(r.source.width_mm));
        let bottom = fixed((s.area.y + s.area.height).min(r.source.height_mm));
        let win_width = ((right - left) * PNT_PER_MM) as i32;
        let win_len = ((bottom - top) * PNT_PER_MM) as i32;
        if win_width <= 0 || win_len <= 0 {
            return Err(invalid("the scan area is empty"));
        }

        let dpi = f64::from(r.dpi);
        let pixels = (f64::from(win_width) / (1180.0 / dpi)) as i32;
        let base = if jpeg { 1180.0 } else { 1213.9 };
        let lines = (f64::from(win_len) / (base / dpi)) as i32;
        let bytes = match composition {
            MODE_LINEART | MODE_HALFTONE => (pixels + 7) / 8,
            MODE_RGB24 => pixels * 3,
            _ => pixels,
        };
        Ok(Self {
            dpi: r.dpi,
            composition,
            threshold: ((threshold - 30) / 10) as u8,
            doc_source,
            win_width,
            win_len,
            off_x: left / MM_PER_INCH,
            off_y: top / MM_PER_INCH,
            jpeg,
            pixels,
            bytes,
            lines,
        })
    }

    /// `dev_set_window`'s command.
    fn command(&self) -> [u8; 0x19] {
        let mut cmd = [0u8; 0x19];
        cmd[..5].copy_from_slice(&[REQ_CODE_A, REQ_CODE_B, CMD_SET_WINDOW, 0x13, MSG_SCANNING_PARAM]);
        cmd[0x05..0x09].copy_from_slice(&self.win_width.to_be_bytes());
        cmd[0x09..0x0d].copy_from_slice(&self.win_len.to_be_bytes());
        let code = RES_DPI_CODES.iter().position(|&d| d == self.dpi).unwrap_or(0) as u8;
        cmd[0x0d] = code;
        cmd[0x0e] = code;
        cmd[0x0f] = self.off_x.floor() as u8;
        cmd[0x10] = ((self.off_x - self.off_x.floor()) * 100.0) as u8;
        cmd[0x11] = self.off_y.floor() as u8;
        cmd[0x12] = ((self.off_y - self.off_y.floor()) * 100.0) as u8;
        cmd[0x13] = self.composition;
        if self.composition == MODE_RGB24 && self.jpeg {
            cmd[0x14] = 0x6;
        }
        cmd[0x16] = self.threshold;
        cmd[0x17] = self.doc_source;
        cmd
    }

    fn parameters(&self) -> Parameters {
        let layout = match self.composition {
            MODE_LINEART | MODE_HALFTONE => PixelLayout { channels: Channels::Gray, depth: 1 },
            MODE_RGB24 => PixelLayout { channels: Channels::Rgb, depth: 8 },
            _ => PixelLayout { channels: Channels::Gray, depth: 8 },
        };
        Parameters {
            layout,
            pixels_per_line: self.pixels as usize,
            bytes_per_line: self.bytes as usize,
            lines: self.lines as usize,
        }
    }

    fn sourcing_from_adf(&self, doc_loaded: bool) -> bool {
        self.doc_source == DOC_ADF || (self.doc_source == DOC_AUTO && doc_loaded)
    }

    /// `sane_start`'s adjustment to the width the device says it will send.
    fn fit_to(&mut self, device_pixels: i32) {
        self.pixels = self.pixels.min(device_pixels);
        match self.composition {
            MODE_RGB24 => self.bytes = self.pixels * 3,
            MODE_LINEART | MODE_HALFTONE => {
                self.bytes = (self.pixels + 7) / 8;
                self.pixels = self.bytes * 8;
            }
            _ => self.bytes = self.pixels,
        }
    }
}

fn decode_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    JpegDecoder::new_with_options(ZCursor::new(data), options)
        .decode()
        .map_err(|e| protocol(format!("bad JPEG from the scanner: {e:?}")))
}

/// The state of a page being read (`struct device`'s scanning fields).
#[derive(Debug)]
struct Page {
    window: Window,
    cancel: CancelToken,
    /// Set once the page has ended; the unit may stay reserved for the next.
    done: bool,
    reading: bool,
    final_block: bool,
    blocklen: i64,
    vertical: i32,
    horizontal: i32,
    bytes_per_line: i32,
    device_pixels: i32,
    ulines: i32,
    y_off: i32,
    dataindex: i32,
    dataoff: usize,
    datalen: usize,
    total_img_size: i64,
    total_out_size: i64,
    jpeg_in: Vec<u8>,
    decoded: Vec<u8>,
    decoded_at: usize,
}

/// An open MFP.
#[derive(Debug)]
pub struct XeroxMfp {
    link: Box<dyn Link>,
    info: DeviceInfo,
    caps: Capabilities,
    inquiry: Inquiry,
    reserved: bool,
    data: Vec<u8>,
    page: Option<Page>,
    /// Counts traffic, to detect a read pass that can't make progress.
    activity: u64,
}

/// Does one command (`dev_command`). `Err` means the reply was unusable; the
/// device has then been flushed.
fn command(link: &mut dyn Link, cmd: &[u8], reply_len: usize, res: &mut [u8; 1024]) -> Result<State> {
    let send_len = if cmd[2] == CMD_SET_WINDOW { 25 } else { usize::from(cmd[3]) + 4 };
    let result = (|| {
        link.send(&cmd[..send_len])?;
        if cmd[2] == CMD_READ_IMAGE {
            return Ok(());
        }
        let n = link.recv_reply(&mut res[..reply_len])?;
        if n < reply_len || res[0] != RES_CODE || usize::from(res[2]) + 3 != n {
            return Err(protocol("malformed reply from the scanner"));
        }
        Ok(())
    })();
    if let Err(e) = result {
        link.flush();
        return Err(e);
    }
    if cmd[2] == CMD_READ_IMAGE {
        return Ok(State::Good);
    }
    if matches!(cmd[2], CMD_SET_WINDOW | CMD_OBJECT_POSITION | CMD_READ | CMD_RESERVE_UNIT) {
        match res[1] {
            STATUS_BUSY => return Ok(State::Busy),
            STATUS_CANCEL => return Ok(State::Failed(Error::cancelled())),
            STATUS_CHECK => {
                let at = if cmd[2] == CMD_READ { 12 } else { 4 };
                return Ok(resolve_state(u16::from(res[at]) << 8 | u16::from(res[at + 1])));
            }
            _ => {}
        }
    }
    Ok(State::Good)
}

impl XeroxMfp {
    fn open_usb(mut usb: Box<dyn UsbTransport>) -> Result<Self> {
        let name = DRIVER.device_name(usb.ids().vendor_id, usb.ids().product_id, usb.ids().serial.as_deref());
        if std::env::var("SANE_XEROX_USB_HALT_WORKAROUND").is_ok_and(|v| v.parse::<i32>().is_ok_and(|n| n != 0)) {
            usb.clear_halt()?;
        }
        Self::open_link(Box::new(UsbLink(usb)), name)
    }

    fn open_link(mut link: Box<dyn Link>, name: String) -> Result<Self> {
        let mut res = [0u8; 1024];
        let cmd = [REQ_CODE_A, REQ_CODE_B, CMD_INQUIRY, 0];
        command(link.as_mut(), &cmd, 70, &mut res)?;
        let inquiry = Inquiry::parse(&res)?;
        let info = DeviceInfo { name, vendor: inquiry.vendor.clone(), model: inquiry.model.clone() };
        Ok(Self {
            link,
            info,
            caps: capabilities(&inquiry),
            inquiry,
            reserved: false,
            data: vec![0; DATASIZE],
            page: None,
            activity: 0,
        })
    }

    /// `dev_cmd`.
    fn cmd(&mut self, code: u8, res: &mut [u8; 1024]) -> Result<State> {
        self.activity += 1;
        let reply = if code == CMD_INQUIRY { 70 } else { 32 };
        command(self.link.as_mut(), &[REQ_CODE_A, REQ_CODE_B, code, 0], reply, res)
    }

    /// `dev_cmd_wait`: repeats a command while the device is busy, backing off
    /// from 10 ms to a second.
    fn cmd_wait(&mut self, code: u8, cancel: &CancelToken, res: &mut [u8; 1024]) -> Result<()> {
        let mut sleep_ms = 10;
        loop {
            cancel.check()?;
            match self.cmd(code, res)? {
                State::Good => return Ok(()),
                State::Failed(e) => return Err(e),
                State::Busy => {
                    self.link.pause(Duration::from_millis(sleep_ms));
                    if sleep_ms < 1000 {
                        sleep_ms = (sleep_ms * if sleep_ms < 100 { 10 } else { 2 }).min(1000);
                    }
                }
            }
        }
    }

    /// `dev_stop`: releases the unit if it's reserved.
    fn release(&mut self) {
        if self.reserved {
            self.reserved = false;
            let mut res = [0u8; 1024];
            let _ = self.cmd(CMD_RELEASE_UNIT, &mut res);
        }
    }

    /// `ret_cancel`.
    fn abort(&mut self) {
        let mut res = [0u8; 1024];
        let _ = self.cmd(CMD_ABORT, &mut res);
        self.release();
    }

    /// `dev_acquire`: asks for the next block.
    fn acquire(&mut self, page: &mut Page) -> Result<()> {
        let mut res = [0u8; 1024];
        let cancel = page.cancel.clone();
        self.cmd_wait(CMD_READ, &cancel, &mut res)?;
        page.vertical = i32::from(res[8]) << 8 | i32::from(res[9]);
        page.horizontal = i32::from(res[10]) << 8 | i32::from(res[11]);
        page.blocklen = i64::from(be32(&res, 4));
        page.final_block = res[3] == MSG_END_BLOCK;
        page.device_pixels = page.horizontal;
        page.bytes_per_line = page.horizontal;
        match page.window.composition {
            MODE_RGB24 => page.bytes_per_line *= 3,
            MODE_LINEART | MODE_HALFTONE => page.device_pixels *= 8,
            _ => {}
        }
        if page.bytes_per_line as usize > DATASIZE {
            self.abort();
            return Err(protocol("the scanner's lines are too long"));
        }
        if page.bytes_per_line == 0 && page.blocklen > 0 {
            return Err(protocol("the scanner announced data with no line length"));
        }
        page.reading = false;
        page.dataindex = 0;
        page.datalen = 0;
        page.dataoff = 0;
        Ok(())
    }

    /// `copy_plain_trim`: copies lines, dropping what's past the requested
    /// width and height. Returns the bytes used from the ring and the output.
    fn copy_plain_trim(&self, page: &mut Page, buf: &mut [u8]) -> (usize, usize) {
        let linesize = page.bytes_per_line.max(1);
        let mut k = page.dataindex;
        let mut out = 0;
        let mut j = 0;
        while j < page.datalen && out < buf.len() {
            let x = k % linesize;
            let y = k / linesize;
            if y >= page.vertical {
                break;
            }
            if x < page.window.bytes && y + page.y_off < page.window.lines {
                buf[out] = self.data[(page.dataoff + j) & DATAMASK];
                out += 1;
            }
            j += 1;
            k += 1;
        }
        page.dataindex = k;
        (j, out)
    }

    /// `copy_mix_bands_trim`: interleaves lines sent as R, G, B runs.
    fn copy_mix_bands_trim(&self, page: &mut Page, buf: &mut [u8]) -> (usize, usize) {
        let linesize = page.bytes_per_line.max(1);
        let y_off = page.dataindex / linesize;
        let mut k = page.dataindex;
        let have = (page.datalen as i32) / linesize * linesize - k % linesize;
        let mut out = 0;
        let mut j = 0;
        while j < have && out < buf.len() {
            let band = (k % 3) * page.horizontal;
            let x = k % linesize / 3;
            let y = k / linesize - y_off;
            if x < page.window.pixels && y + y_off + page.y_off < page.window.lines {
                let at = page.dataoff as i32 + band + x + y * linesize;
                buf[out] = self.data[at as usize & DATAMASK];
                out += 1;
            }
            j += 1;
            k += 1;
        }
        page.dataindex = k;
        (((k / linesize - y_off) * linesize) as usize, out)
    }

    /// `copy_decompress_data`.
    fn copy_decoded(page: &mut Page, buf: &mut [u8]) -> usize {
        let rest = &page.decoded[page.decoded_at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        page.decoded_at += n;
        if page.decoded_at == page.decoded.len() {
            page.decoded.clear();
            page.decoded_at = 0;
        }
        n
    }

    /// `DATAROOM` rounded down to whole USB blocks, capped to what's announced.
    fn next_transfer(page: &Page) -> usize {
        let tail = (page.dataoff + page.datalen) & DATAMASK;
        let room = if tail < page.dataoff {
            page.dataoff - tail
        } else if page.datalen == DATASIZE {
            0
        } else {
            DATASIZE - tail
        };
        let room = room & !(USB_BLOCK_SIZE - 1);
        usize::try_from(page.blocklen).unwrap_or(0).min(room)
    }

    /// One `sane_read`. Returns the bytes written, or `None` at the page's end.
    fn read_pass(&mut self, page: &mut Page, buf: &mut [u8]) -> Result<Option<usize>> {
        let rgb = page.window.composition == MODE_RGB24;
        let mut written = 0;
        if page.blocklen == 0 && (page.datalen <= PADDING_SIZE || page.final_block) {
            let diff = page.total_img_size - page.total_out_size;
            let limit = buf.len().min(usize::try_from(diff.max(0)).unwrap_or(usize::MAX));
            if rgb && page.window.jpeg && !page.decoded.is_empty() {
                if diff != 0 {
                    let n = Self::copy_decoded(page, &mut buf[..limit]);
                    page.total_out_size += n as i64;
                    return Ok(Some(n));
                }
            } else if !rgb && diff > 0 {
                let (used, n) = self.copy_plain_trim(page, &mut buf[..limit]);
                if used > 0 {
                    page.datalen -= used;
                    page.dataoff = (page.dataoff + used) & DATAMASK;
                    page.total_out_size += n as i64;
                    return Ok(Some(n));
                }
            }
            if page.final_block {
                let slack = page.total_img_size - page.total_out_size;
                if slack > 0 {
                    // A short page is padded with white.
                    let n = buf.len().min(usize::try_from(slack).unwrap_or(usize::MAX));
                    buf[..n].fill(255);
                    page.total_out_size += n as i64;
                    return Ok(Some(n));
                }
                return Ok(None);
            }
            self.acquire(page)?;
        }

        if !page.reading {
            page.cancel.check()?;
            let mut res = [0u8; 1024];
            self.cmd(CMD_READ_IMAGE, &mut res)?;
            page.reading = true;
            page.ulines += page.vertical;
            page.y_off = page.ulines - page.vertical;
        }

        loop {
            let mut want = Self::next_transfer(page);
            while want > 0 && page.blocklen > 0 {
                let tail = (page.dataoff + page.datalen) & DATAMASK;
                let n = self.link.recv_data(&mut self.data[tail..tail + want])?;
                self.activity += u64::from(n > 0);
                page.datalen += n;
                page.blocklen -= n as i64;
                if n == 0 {
                    break;
                }
                want = Self::next_transfer(page);
            }

            let rest = &mut buf[written..];
            let (cleared, out) = if rgb && self.inquiry.line_order != 0 {
                if page.window.jpeg {
                    let start = page.dataoff;
                    page.jpeg_in.extend_from_slice(&self.data[start..start + page.datalen]);
                    let cleared = page.datalen;
                    if page.blocklen == 0 {
                        let jpeg = std::mem::take(&mut page.jpeg_in);
                        page.decoded = decode_jpeg(&jpeg)?;
                        page.decoded_at = 0;
                    }
                    (cleared, Self::copy_decoded(page, rest))
                } else {
                    self.copy_mix_bands_trim(page, rest)
                }
            } else {
                self.copy_plain_trim(page, rest)
            };
            page.datalen -= cleared.min(page.datalen);
            page.dataoff = (page.dataoff + cleared) & DATAMASK;
            written += out;
            page.total_out_size += out as i64;
            if page.bytes_per_line > 0 && page.dataindex / page.bytes_per_line + page.y_off >= page.window.lines {
                page.datalen = 0;
                page.dataoff = 0;
            }
            if cleared == 0 || written == buf.len() || page.blocklen == 0 {
                break;
            }
        }
        Ok(Some(written))
    }
}

impl Device for XeroxMfp {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        Ok(Window::new(&self.caps, &self.inquiry, settings)?.parameters())
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        let mut window = Window::new(&self.caps, &self.inquiry, settings)?;
        // A finished ADF page stays reserved, for the next one.
        if self.page.as_ref().is_some_and(|p| !p.done) {
            self.finish();
        }
        self.page = None;
        let mut res = [0u8; 1024];

        if !self.reserved {
            if self.inquiry.has_adf && matches!(window.doc_source, DOC_AUTO | DOC_ADF) {
                self.cmd(CMD_INQUIRY, &mut res)?;
                let (has_adf, doc_loaded) = adf_status(&res);
                self.inquiry.has_adf = has_adf;
                self.inquiry.doc_loaded = doc_loaded;
            }
            self.cmd_wait(CMD_RESERVE_UNIT, cancel, &mut res)?;
            self.reserved = true;
            self.activity += 1;
            match command(self.link.as_mut(), &window.command(), 32, &mut res) {
                Ok(State::Good | State::Busy) => {}
                Ok(State::Failed(e)) | Err(e) => {
                    self.release();
                    return Err(e);
                }
            }
        }
        if let Err(e) = self.cmd_wait(CMD_OBJECT_POSITION, cancel, &mut res) {
            self.release();
            return Err(e);
        }

        let mut page = Page {
            window,
            cancel: cancel.clone(),
            done: false,
            reading: false,
            final_block: false,
            blocklen: 0,
            vertical: 0,
            horizontal: 0,
            bytes_per_line: 0,
            device_pixels: 0,
            ulines: 0,
            y_off: 0,
            dataindex: 0,
            dataoff: 0,
            datalen: 0,
            total_img_size: 0,
            total_out_size: 0,
            jpeg_in: Vec::new(),
            decoded: Vec::new(),
            decoded_at: 0,
        };
        if let Err(e) = self.acquire(&mut page) {
            self.abort();
            return Err(e);
        }
        window.fit_to(page.device_pixels);
        page.window = window;
        page.total_img_size = i64::from(window.bytes) * i64::from(window.lines);
        self.page = Some(page);
        Ok(window.parameters())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(mut page) = self.page.take() else { return Err(invalid("no scan is running")) };
        let result = (|| {
            if page.done {
                return Ok(0);
            }
            loop {
                page.cancel.check()?;
                let before = self.activity;
                match self.read_pass(&mut page, buf)? {
                    Some(0) if self.activity == before => {
                        return Err(protocol("the scanner stopped sending data"));
                    }
                    Some(0) => {}
                    Some(n) => return Ok(n),
                    None => {
                        page.done = true;
                        return Ok(0);
                    }
                }
            }
        })();
        let finished = page.done;
        let adf = page.window.sourcing_from_adf(self.inquiry.doc_loaded);
        self.page = Some(page);
        if finished && !adf {
            self.release();
        }
        result
    }

    fn finish(&mut self) {
        let Some(mut page) = self.page.take() else { return };
        if page.done {
            // Leave any ADF reservation for the next page; `Drop` ends it.
            self.page = Some(page);
            return;
        }
        // Mid-page: drain the block being read, then abort (`usb_dev_close`).
        if page.reading {
            loop {
                let want = Self::next_transfer(&page);
                if want == 0 || page.blocklen <= 0 {
                    break;
                }
                let mut sink = vec![0u8; want];
                match self.link.recv_data(&mut sink) {
                    Ok(n) if n > 0 => page.blocklen -= n as i64,
                    _ => break,
                }
            }
        }
        self.abort();
    }
}

impl Drop for XeroxMfp {
    fn drop(&mut self) {
        let unfinished = self.page.as_ref().is_some_and(|p| !p.done);
        if unfinished {
            self.finish();
        } else if self.reserved {
            self.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_core::Area;
    use scan_core::driver::scan_frame;
    use scan_usb::mock::{Mock, Transfer};
    use std::sync::{Arc, Mutex};

    /// Bytes for an inquiry reply. 600 and 300 dpi; A4 bed; optional ADF.
    fn inquiry_reply(model: &str, line_order: u8, compression: u8, adf: bool) -> Vec<u8> {
        let mut r = vec![0u8; 70];
        r[0] = RES_CODE;
        r[2] = 67;
        r[3] = MSG_PRODUCT_INFO;
        let name = format!("Samsung {model}");
        r[4..4 + name.len()].copy_from_slice(name.as_bytes());
        // Bits 5 (300 dpi) and 8 (600 dpi).
        r[0x24] = 0b0000_0001;
        r[0x25] = 0b0010_0000;
        r[0x26] = u8::from(adf);
        r[0x28..0x2c].copy_from_slice(&10200u32.to_be_bytes());
        r[0x2c..0x30].copy_from_slice(&14000u32.to_be_bytes());
        r[0x31] = line_order;
        r[0x32] = compression;
        r[0x35] = if adf { 2 } else { 0 };
        r[0x38..0x3c].copy_from_slice(&16800u32.to_be_bytes());
        r[0x3c..0x40].copy_from_slice(&14000u32.to_be_bytes());
        r
    }

    fn reply_32(status: u8) -> Vec<u8> {
        let mut r = vec![0u8; 32];
        r[0] = RES_CODE;
        r[1] = status;
        r[2] = 29;
        r
    }

    fn read_reply(blocklen: u32, lines: u16, per_line: u16, last: bool) -> Vec<u8> {
        let mut r = reply_32(0);
        r[3] = if last { MSG_END_BLOCK } else { 0x80 };
        r[4..8].copy_from_slice(&blocklen.to_be_bytes());
        r[8..10].copy_from_slice(&lines.to_be_bytes());
        r[10..12].copy_from_slice(&per_line.to_be_bytes());
        r
    }

    /// Simulates the command set; READ replies and data come from the script.
    fn device(inquiry: Vec<u8>, mut reads: Vec<Vec<u8>>, mut data: Vec<Vec<u8>>) -> Mock {
        let mut last = 0u8;
        Mock::new(0x04e8, 0x3324, move |t| match t {
            Transfer::BulkOut(cmd) => {
                last = cmd[2];
                Ok(vec![])
            }
            Transfer::BulkIn(_) => match last {
                CMD_INQUIRY => Ok(inquiry.clone()),
                CMD_READ => Ok(reads.remove(0)),
                CMD_READ_IMAGE => Ok(data.remove(0)),
                _ => Ok(reply_32(0)),
            },
            _ => Ok(vec![]),
        })
    }

    fn sent_commands(log: &scan_usb::mock::Log) -> Vec<u8> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|t| match t {
                Transfer::BulkOut(c) => Some(c[2]),
                _ => None,
            })
            .collect()
    }

    fn settings(mode: &str, source: &str, width: f64, height: f64) -> ScanSettings {
        ScanSettings {
            source: source.into(),
            mode: mode.into(),
            resolution: 300,
            area: Area { x: 0.0, y: 0.0, width, height },
            gamma: None,
            extras: vec![],
        }
    }

    #[test]
    fn inquiry_is_parsed() {
        let inq = Inquiry::parse(&inquiry_reply("SCX-4x16 Series", 1, 0, true)).unwrap();
        assert_eq!((inq.vendor.as_str(), inq.model.as_str()), ("Samsung", "SCX-4x16 Series"));
        assert_eq!(inq.dpis, [300, 600]);
        assert_eq!((inq.has_adf, inq.doc_loaded, inq.line_order), (true, true, 1));
        assert_eq!((inq.max_win_width, inq.max_len_fb, inq.max_len_adf), (10200, 14000, 16800));
        let mut wrong = inquiry_reply("X", 0, 0, false);
        wrong[3] = 0;
        assert!(Inquiry::parse(&wrong).is_err());
    }

    #[test]
    fn jpeg_support_excludes_listed_models() {
        let inq = |m: &str, c| Inquiry::parse(&inquiry_reply(m, 1, c, false)).unwrap();
        assert!(inq("M2070", 1 << 6).supports_jpeg());
        assert!(!inq("M2070", 0).supports_jpeg());
        assert!(!inq("C460 Series", 1 << 6).supports_jpeg());
        assert!(!inq("SCX-4728", 1 << 6).supports_jpeg());
        assert!(!inq("Xerox 4x24", 1 << 6).supports_jpeg());
    }

    #[test]
    fn adf_sources_need_an_adf() {
        let inq = Inquiry::parse(&inquiry_reply("A", 0, 0, false)).unwrap();
        assert_eq!(capabilities(&inq).sources.len(), 1);
        let inq = Inquiry::parse(&inquiry_reply("A", 0, 0, true)).unwrap();
        assert_eq!(capabilities(&inq).sources.len(), 3);
    }

    #[test]
    fn set_window_command_matches_dev_set_window() {
        let inq = Inquiry::parse(&inquiry_reply("A", 0, 0, false)).unwrap();
        let caps = capabilities(&inq);
        let mut s = settings(NAME_LINEART, SOURCE_FLATBED, 50.8, 25.4);
        s.area.x = 25.4 * 1.5;
        s.area.y = 25.4 * 2.25;
        s.extras = vec![("threshold".into(), OptionValue::Int(60))];
        let w = Window::new(&caps, &inq, &s).unwrap();
        let c = w.command();
        assert_eq!(&c[..5], [0x1b, 0xa8, 0x24, 0x13, 0x30]);
        assert_eq!(i32::from_be_bytes(c[5..9].try_into().unwrap()), w.win_width);
        assert_eq!(i32::from_be_bytes(c[9..13].try_into().unwrap()), w.win_len);
        assert!((w.win_width - 2400).abs() <= 1, "{}", w.win_width);
        assert_eq!((c[0x0d], c[0x0e]), (5, 5), "300 dpi's code");
        assert_eq!(c[0x0f], 1);
        assert!((49..=50).contains(&c[0x10]), "{}", c[0x10]);
        assert_eq!(c[0x11], 2);
        assert!((24..=25).contains(&c[0x12]), "{}", c[0x12]);
        assert_eq!((c[0x13], c[0x14], c[0x16], c[0x17]), (MODE_LINEART, 0, 3, DOC_FLATBED));
    }

    #[test]
    fn flatbed_gray_is_trimmed_padded_and_released() {
        // 25.4 mm wide at 300 dpi comes to 305 px with SANE's 1180 base. The
        // device sends 300-pixel lines, so the frame narrows to 300.
        let lines = vec![7u8; 300 * 2];
        let inq = inquiry_reply("A", 0, 0, false);
        let reads = vec![read_reply(lines.len() as u32, 2, 300, true)];
        let mock = device(inq, reads, vec![lines]);
        let log = mock.log();
        let mut scanner = XeroxMfp::open_usb(Box::new(mock)).unwrap();
        let s = settings(NAME_GRAY, SOURCE_FLATBED, 25.4, 2.0);
        let (params, image) = scan_frame(&mut scanner, &s).unwrap();
        assert_eq!(params.bytes_per_line, 300);
        assert_eq!(image.len(), params.bytes_per_line * params.lines);
        assert_eq!(&image[..2], [7, 7]);
        assert_eq!(*image.last().unwrap(), 255, "the short page is padded white");
        assert_eq!(
            sent_commands(&log),
            [
                CMD_INQUIRY,
                CMD_RESERVE_UNIT,
                CMD_SET_WINDOW,
                CMD_OBJECT_POSITION,
                CMD_READ,
                CMD_READ_IMAGE,
                CMD_RELEASE_UNIT
            ]
        );
    }

    #[test]
    fn colour_bands_are_mixed_when_the_device_has_a_line_order() {
        // Two pixels per line: R R G G B B.
        let inq = inquiry_reply("A", 1, 0, false);
        let block = vec![1, 2, 3, 4, 5, 6];
        let reads = vec![read_reply(6, 1, 2, true)];
        let mut scanner = XeroxMfp::open_usb(Box::new(device(inq, reads, vec![block]))).unwrap();
        let s = settings(NAME_COLOR, SOURCE_FLATBED, 25.4, 0.5);
        let (params, image) = scan_frame(&mut scanner, &s).unwrap();
        assert_eq!(params.pixels_per_line, 2);
        assert_eq!(&image[..6], [1, 3, 5, 2, 4, 6]);
    }

    #[test]
    fn adf_pages_keep_the_unit_reserved_until_dropped() {
        let inq = inquiry_reply("A", 0, 0, true);
        let reads = vec![read_reply(100, 1, 100, true)];
        let mock = device(inq, reads, vec![vec![9; 100]]);
        let log = mock.log();
        let mut scanner = XeroxMfp::open_usb(Box::new(mock)).unwrap();
        let s = settings(NAME_GRAY, SOURCE_ADF, 10.0, 1.0);
        scan_frame(&mut scanner, &s).unwrap();
        assert!(!sent_commands(&log).contains(&CMD_RELEASE_UNIT));
        drop(scanner);
        let sent = sent_commands(&log);
        assert_eq!(&sent[sent.len() - 2..], [CMD_ABORT, CMD_RELEASE_UNIT]);
    }

    #[test]
    fn stopping_mid_page_drains_and_aborts() {
        let inq = inquiry_reply("A", 0, 0, false);
        let reads = vec![read_reply(1024, 8, 128, true)];
        let script = vec![vec![1; 512], vec![2; 512]];
        let mock = device(inq, reads, script);
        let log = mock.log();
        let mut scanner = XeroxMfp::open_usb(Box::new(mock)).unwrap();
        let s = settings(NAME_GRAY, SOURCE_FLATBED, 10.0, 5.0);
        scanner.start(&s, &CancelToken::new()).unwrap();
        scanner.finish();
        let sent = sent_commands(&log);
        assert_eq!(&sent[sent.len() - 2..], [CMD_ABORT, CMD_RELEASE_UNIT]);
    }

    #[test]
    fn busy_is_retried_and_other_states_fail() {
        let mut replies = vec![reply_32(STATUS_BUSY), reply_32(0)];
        let inq = inquiry_reply("A", 0, 0, false);
        let last = Arc::new(Mutex::new(0u8));
        let mock = Mock::new(0x04e8, 0x3324, move |t| match t {
            Transfer::BulkOut(c) => {
                *last.lock().unwrap() = c[2];
                Ok(vec![])
            }
            Transfer::BulkIn(_) => match *last.lock().unwrap() {
                CMD_INQUIRY => Ok(inq.clone()),
                CMD_RESERVE_UNIT => Ok(replies.remove(0)),
                CMD_OBJECT_POSITION => {
                    let mut r = reply_32(STATUS_CHECK);
                    r[4..6].copy_from_slice(&STATE_NO_DOCUMENT.to_be_bytes());
                    Ok(r)
                }
                _ => Ok(reply_32(0)),
            },
            _ => Ok(vec![]),
        });
        let log = mock.log();
        let mut scanner = XeroxMfp::open_usb(Box::new(mock)).unwrap();
        let s = settings(NAME_GRAY, SOURCE_FLATBED, 10.0, 10.0);
        let err = scanner.start(&s, &CancelToken::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Device);
        let sent = sent_commands(&log);
        #[allow(clippy::naive_bytecount, reason = "a handful of commands in a test")]
        let reserves = sent.iter().filter(|&&c| c == CMD_RESERVE_UNIT).count();
        assert_eq!(reserves, 2);
        assert_eq!(*sent.last().unwrap(), CMD_RELEASE_UNIT);
    }

    #[test]
    fn state_bits_map_like_resolv_state() {
        assert!(matches!(resolve_state(STATE_NO_ERROR), State::Good));
        assert!(matches!(resolve_state(0x400), State::Busy));
        let State::Failed(e) = resolve_state(STATE_WARMING) else { panic!() };
        assert_eq!(e.kind(), ErrorKind::Busy, "warming up isn't retried, as with SANE_STATUS_WARMING_UP");
        let State::Failed(e) = resolve_state(STATE_INVALID_AREA) else { panic!() };
        assert_eq!(e.kind(), ErrorKind::Invalid);
        for bit in [STATE_DOCUMENT_JAM, STATE_COVER_OPEN, STATE_LOCKING] {
            assert!(matches!(resolve_state(bit), State::Failed(_)));
        }
    }

    #[test]
    fn a_bad_jpeg_is_an_error() {
        assert!(decode_jpeg(&[1, 2, 3]).is_err());
    }

    #[test]
    fn config_ids_are_known() {
        let m = (DRIVER.find)(0x04e8, 0x3409).unwrap();
        assert_eq!((m.vendor, m.model), ("Samsung", "SCX-4x16 Series"));
        assert_eq!(models::MODELS.len(), 83);
    }
}

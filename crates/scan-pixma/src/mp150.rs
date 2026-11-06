//! The mp150 subdriver (`pixma_mp150.c`), generation 5 flatbed path.
//!
//! A scan is: the XML `StartJob`/`ModeShift` dialog, `start_session`, three
//! gamma tables, `scan_param`, `start_scan`; then polling status until the
//! firmware has calibrated, and reading image blocks until the device flags
//! the last one. Ending a scan, finished or not, sends `abort_session` and
//! the XML `EndJob`.

use std::time::Duration;

use crate::image::{self, Lineart};
use crate::models::{Buttons, Model};
use crate::protocol::{CmdBuf, Io, PResult, PixmaError, be32, set_be16, set_be32};

const CMD_START_SESSION: u16 = 0xdb20;
const CMD_GAMMA: u16 = 0xee20;
const CMD_ABORT_SESSION: u16 = 0xef20;
const CMD_READ_IMAGE: u16 = 0xd420;
const CMD_ERROR_INFO: u16 = 0xff20;
const CMD_SCAN_PARAM_3: u16 = 0xd820;
const CMD_SCAN_START_3: u16 = 0xd920;
const CMD_STATUS_3: u16 = 0xda20;

/// How long [`Mp150::start_job`] waits for a previous job to clear.
const START_JOB_PATIENCE: Duration = Duration::from_secs(30);

/// Largest image block the device is asked for.
pub(crate) const IMAGE_BLOCK_SIZE: usize = 512 * 1024;

/// Block flags: end of scan, no more paper, no more data.
const BLOCK_END: u8 = 0x08;
const BLOCK_NO_DATA: u8 = 0x20;
const BLOCK_FLAGS: u8 = 0x38;

const XML_START_1: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\
<cmd xmlns:ivec=\"http://www.canon.com/ns/cmd/2008/07/common/\">\
<ivec:contents><ivec:operation>StartJob</ivec:operation>\
<ivec:param_set servicetype=\"scan\"><ivec:jobID>00000001</ivec:jobID>\
<ivec:bidi>1</ivec:bidi></ivec:param_set></ivec:contents></cmd>";

const XML_START_2: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\
<cmd xmlns:ivec=\"http://www.canon.com/ns/cmd/2008/07/common/\" xmlns:vcn=\"http://www.canon.com/ns/cmd/2008/07/canon/\">\
<ivec:contents><ivec:operation>VendorCmd</ivec:operation>\
<ivec:param_set servicetype=\"scan\"><ivec:jobID>00000001</ivec:jobID>\
<vcn:ijoperation>ModeShift</vcn:ijoperation><vcn:ijmode>1</vcn:ijmode>\
</ivec:param_set></ivec:contents></cmd>";

const XML_END: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\
<cmd xmlns:ivec=\"http://www.canon.com/ns/cmd/2008/07/common/\">\
<ivec:contents><ivec:operation>EndJob</ivec:operation>\
<ivec:param_set servicetype=\"scan\"><ivec:jobID>00000001</ivec:jobID>\
</ivec:param_set></ivec:contents></cmd>";

/// Pixma scan modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Color,
    Gray,
    Color48,
    Gray16,
    Lineart,
}

/// When the firmware recalibrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Calibrate {
    /// On the first scan after opening the device.
    Once,
    Always,
    Never,
}

/// A fully resolved scan (`pixma_scan_param_t`), in pixels at `xdpi`.
#[derive(Debug, Clone)]
pub(crate) struct ScanParam {
    pub(crate) mode: Mode,
    pub(crate) channels: u32,
    pub(crate) depth: u32,
    pub(crate) software_lineart: bool,
    pub(crate) xdpi: u32,
    pub(crate) ydpi: u32,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Left crop inside the raw line, to align `x` to 32 hardware pixels.
    pub(crate) xs: u32,
    /// Raw line width in hardware pixels.
    pub(crate) wx: u32,
    /// Bytes per output line, before lineart packing.
    pub(crate) line_size: u64,
    /// Bytes in the whole output image.
    pub(crate) image_size: u64,
    /// 1024 little-endian `u16`s.
    pub(crate) gamma_table: Vec<u8>,
    pub(crate) calibrate: Calibrate,
    pub(crate) lineart: Lineart,
}

impl ScanParam {
    fn is_gray16(&self) -> bool {
        self.mode == Mode::Gray16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Warmup,
    Scanning,
    Transferring,
    Finished,
}

/// A front-panel button press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ButtonEvent {
    /// 1 starts a scan, 2 ends one.
    pub(crate) button: u8,
    /// The model's target code, e.g. copy or PDF.
    pub(crate) target: u8,
}

/// The driver state for one open device (`mp150_t`).
#[derive(Debug)]
pub(crate) struct Mp150 {
    pub(crate) model: &'static Model,
    pub(crate) io: Io,
    cb: CmdBuf,
    state: State,
    current_status: [u8; 16],
    last_block: u8,
    /// Shrink factor for resolutions below the hardware minimum.
    pub(crate) scale: u32,
    /// Raw bytes received but not yet processed into whole lines.
    raw: Vec<u8>,
    /// Scratch buffer for the bulk of an image block.
    block: Vec<u8>,
    pub(crate) last_button: Option<ButtonEvent>,
}

impl Mp150 {
    pub(crate) fn new(model: &'static Model, io: Io) -> Self {
        Self {
            model,
            io,
            cb: CmdBuf::new(),
            state: State::Idle,
            current_status: [0; 16],
            last_block: 0,
            scale: 1,
            raw: Vec::new(),
            block: Vec::new(),
            last_button: None,
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// Sends an XML dialog message; `Ok(false)` if the device said no.
    fn send_xml_dialog(&mut self, message: &str) -> PResult<Result<(), String>> {
        let mut reply = vec![0u8; 1024];
        let n = self.io.transaction(message.as_bytes(), &mut reply)?;
        let reply = String::from_utf8_lossy(&reply[..n]);
        log::trace!(sent = message, reply:% = reply; "XML dialog");
        let field = |tag: &str| {
            let open = format!("<ivec:{tag}>");
            let start = reply.find(&open)? + open.len();
            let end = reply[start..].find('<')? + start;
            Some(reply[start..end].trim().to_owned())
        };
        if field("response").as_deref() == Some("OK") {
            return Ok(Ok(()));
        }
        Ok(Err(field("response_detail").filter(|d| !d.is_empty()).unwrap_or_else(|| "no reason given".into())))
    }

    fn xml_dialog_or_fail(&mut self, message: &str) -> PResult<()> {
        self.send_xml_dialog(message)?.map_err(|detail| PixmaError::Device(describe_xml_detail(&detail)))
    }

    /// Sends `StartJob`. Straight after a scan the device refuses with
    /// `PCScanning` until it has finished its own cleanup; C SANE fails the
    /// scan there, this waits for the device instead.
    fn start_job(&mut self, cancel: &scan_core::CancelToken) -> PResult<()> {
        let started = std::time::Instant::now();
        loop {
            match self.send_xml_dialog(XML_START_1)? {
                Ok(()) => return Ok(()),
                Err(detail) if detail == "PCScanning" && started.elapsed() < START_JOB_PATIENCE => {
                    log::debug!(waited:? = started.elapsed(); "device still finishing the last job");
                    if cancel.is_cancelled() {
                        return Err(PixmaError::Cancelled);
                    }
                    self.io.sleep(Duration::from_millis(500));
                }
                Err(detail) => return Err(PixmaError::Device(describe_xml_detail(&detail))),
            }
        }
    }

    fn start_session(&mut self) -> PResult<()> {
        self.cb.new_cmd(CMD_START_SESSION, 0, 0);
        self.cb.exec(&mut self.io)
    }

    fn start_scan_3(&mut self) -> PResult<()> {
        self.cb.new_cmd(CMD_SCAN_START_3, 0, 0);
        self.cb.exec(&mut self.io)
    }

    fn is_calibrated(&self) -> bool {
        self.current_status[0] & 0x03 != 0
    }

    fn drain_bulk_in(&mut self) {
        let mut buf = vec![0u8; IMAGE_BLOCK_SIZE];
        while self.io.read(&mut buf).is_ok() {}
    }

    fn abort_session(&mut self) -> PResult<()> {
        self.cb.exec_short(&mut self.io, CMD_ABORT_SESSION)
    }

    fn send_gamma_table(&mut self, sp: &ScanParam) -> PResult<()> {
        let data = self.cb.new_cmd(CMD_GAMMA, 1024 * 2 + 8, 0);
        self.cb.buf[data] = 0x10;
        set_be16(0x0804, &mut self.cb.buf[data + 2..]);
        self.cb.buf[data + 4..data + 4 + 2048].copy_from_slice(&sp.gamma_table);
        self.cb.exec(&mut self.io)
    }

    /// Hardware pixels per raw line, padded to a multiple of 32.
    pub(crate) fn calc_raw_width(&self, sp: &ScanParam) -> u32 {
        (sp.w * self.scale + sp.xs).div_ceil(32) * 32
    }

    /// Bytes per processed line group: `scale` raw lines of `wx` pixels, in
    /// the format the scanner sends (48-bit colour for 16-bit gray).
    fn cis_line_size(&self, sp: &ScanParam) -> usize {
        let per_line = if sp.wx == 0 { sp.line_size } else { sp.line_size / u64::from(sp.w) * u64::from(sp.wx) };
        let bytes = per_line * u64::from(self.scale) * if sp.is_gray16() { 3 } else { 1 };
        usize::try_from(bytes).expect("line size fits in memory")
    }

    fn send_scan_param(&mut self, sp: &ScanParam, calibrate: u8) -> PResult<()> {
        let scale = self.scale;
        let xdpi = sp.xdpi * scale;
        let ydpi = sp.xdpi * scale;
        let x = sp.x * scale;
        let y = sp.y * scale;
        let wx = self.calc_raw_width(sp);
        let h = sp.h.min(self.model.height * sp.ydpi / 75) * scale;
        let to16 = |v: u32| u16::try_from(v | 0x8000).map_err(|_| PixmaError::Invalid(format!("{v} dpi is too high")));
        let (xdpi16, ydpi16) = (to16(xdpi)?, to16(ydpi)?);
        let d = self.cb.new_cmd(CMD_SCAN_PARAM_3, 0x38, 0);
        let buf = &mut self.cb.buf[d..d + 0x38];
        buf[0x00] = 0x01; // flatbed
        buf[0x01] = 0x01;
        buf[0x02] = 0x01;
        buf[0x05] = calibrate;
        set_be16(xdpi16, &mut buf[0x08..]);
        set_be16(ydpi16, &mut buf[0x0a..]);
        set_be32(x - sp.xs, &mut buf[0x0c..]);
        set_be32(y, &mut buf[0x10..]);
        set_be32(wx, &mut buf[0x14..]);
        set_be32(h, &mut buf[0x18..]);
        buf[0x1c] = if sp.channels != 1 || sp.is_gray16() { 0x08 } else { 0x04 };
        let bits = if sp.software_lineart { 8 } else { sp.depth } * if sp.is_gray16() { 3 } else { sp.channels };
        buf[0x1d] = u8::try_from(bits).unwrap_or(u8::MAX);
        buf[0x1f] = 0x01;
        buf[0x20] = 0xff;
        buf[0x21] = 0x81;
        buf[0x23] = 0x02;
        buf[0x24] = 0x01;
        buf[0x30] = 0x01;
        self.cb.exec(&mut self.io)
    }

    fn query_status_3(&mut self) -> PResult<()> {
        let data = self.cb.new_cmd(CMD_STATUS_3, 0, 8);
        self.cb.exec(&mut self.io)?;
        self.current_status[..8].copy_from_slice(&self.cb.buf[data..data + 8]);
        Ok(())
    }

    /// Reads one image block into `self.raw`; returns the bytes added and the
    /// block header.
    fn read_image_block(&mut self) -> PResult<(usize, [u8; 16])> {
        const HLEN: usize = 16;
        let mut cmd = [0u8; 16];
        set_be16(CMD_READ_IMAGE, &mut cmd);
        let request = if self.last_block & BLOCK_NO_DATA == 0 { IMAGE_BLOCK_SIZE / 65536 * 65536 + 8 } else { 32 + 8 };
        set_be32(u32::try_from(request).expect("block size fits u32"), &mut cmd[0xc..]);

        self.state = State::Transferring;
        let first = self.io.transaction(&cmd, &mut self.cb.buf[..512])?;
        let mut header = [0u8; HLEN];
        let n = first.min(HLEN);
        header[..n].copy_from_slice(&self.cb.buf[..n]);
        let mut datalen = 0;
        if first >= HLEN {
            datalen = first - HLEN;
            self.raw.extend_from_slice(&self.cb.buf[HLEN..first]);
            if first == 512 {
                self.block.resize(IMAGE_BLOCK_SIZE - 512 + HLEN, 0);
                let more = self.io.read(&mut self.block)?;
                self.raw.extend_from_slice(&self.block[..more]);
                datalen += more;
            }
        }
        self.state = State::Scanning;
        self.cb.set_raw_result(first);
        self.cb.check_result()?;
        if first < HLEN {
            return Err(PixmaError::Protocol(format!("{first}-byte image block header")));
        }
        Ok((datalen, header))
    }

    fn read_error_info(&mut self) -> PResult<()> {
        self.cb.new_cmd(CMD_ERROR_INFO, 0, 16);
        self.cb.exec(&mut self.io)
    }

    /// Waits up to `timeout_ms` for an interrupt packet and decodes button
    /// presses. Returns whether a packet arrived.
    fn handle_interrupt(&mut self, timeout_ms: i32) -> PResult<bool> {
        let mut buf = [0u8; 64];
        let len = match self.io.wait_interrupt(&mut buf, timeout_ms) {
            Ok(len) => len,
            Err(PixmaError::Timeout) => return Ok(false),
            Err(e) => return Err(e),
        };
        if len % 16 != 0 {
            log::warn!(len; "unexpected interrupt packet length");
            return Err(PixmaError::Protocol(format!("{len}-byte interrupt packet")));
        }
        match self.model.buttons {
            Buttons::Target13 { finish } => {
                let target = buf[0x13];
                if target != 0 {
                    let button = if target == finish { 2 } else { 1 };
                    log::info!(button, target; "scanner button pressed");
                    self.last_button = Some(ButtonEvent { button, target: target & 0x0f });
                }
            }
        }
        Ok(true)
    }

    fn wait_until_ready(&mut self, cancel: &scan_core::CancelToken) -> PResult<()> {
        let mut tries = 120;
        self.query_status_3()?;
        while !self.is_calibrated() {
            let r = self.handle_interrupt(1000);
            if cancel.is_cancelled() {
                return Err(PixmaError::Cancelled);
            }
            r?;
            self.query_status_3()?;
            tries -= 1;
            if tries == 0 {
                log::warn!("timed out waiting for calibration");
                return Err(PixmaError::Timeout);
            }
        }
        Ok(())
    }

    /// Starts a scan (`mp150_scan`). On failure the session is ended.
    pub(crate) fn scan(&mut self, sp: &ScanParam, calibrate: u8, cancel: &scan_core::CancelToken) -> PResult<()> {
        if self.state != State::Idle {
            return Err(PixmaError::Busy);
        }
        self.start_job(cancel)?;
        self.xml_dialog_or_fail(XML_START_2)?;
        // Discard button presses queued before the scan.
        while matches!(self.handle_interrupt(0), Ok(true)) {}

        let mut result = self.start_session();
        let mut tries = 10;
        while matches!(result, Err(PixmaError::Busy)) && tries > 0 {
            tries -= 1;
            if cancel.is_cancelled() {
                result = Err(PixmaError::Cancelled);
                break;
            }
            log::info!(tries; "scanner busy, retrying");
            self.io.sleep(Duration::from_secs(1));
            result = self.start_session();
        }
        if matches!(result, Err(PixmaError::Busy | PixmaError::Timeout)) {
            log::warn!("scanner hangs? sending abort_session");
            self.drain_bulk_in();
            // Its result doesn't matter; start_session decides.
            if let Err(e) = self.abort_session() {
                log::debug!(error:% = e; "abort_session failed");
            }
            self.io.sleep(Duration::from_millis(500));
            result = self.start_session();
        }
        self.state = State::Warmup;
        if result.is_ok() {
            for _ in 0..3 {
                result = self.send_gamma_table(sp);
                if result.is_err() {
                    break;
                }
            }
        }
        if result.is_ok() {
            result = self.send_scan_param(sp, calibrate);
        }
        if result.is_ok() {
            result = self.start_scan_3();
        }
        if result.is_err() {
            self.last_block = BLOCK_FLAGS;
            self.finish_scan();
        }
        result
    }

    /// Reads blocks until some processed output is available, appending it
    /// to `out` (`mp150_fill_buffer`). Returns 0 at the end of the image.
    pub(crate) fn fill_buffer(
        &mut self,
        sp: &ScanParam,
        out: &mut Vec<u8>,
        cancel: &scan_core::CancelToken,
    ) -> PResult<usize> {
        if self.state == State::Warmup {
            self.wait_until_ready(cancel)?;
            // Not needed, but Windows' driver waits too.
            self.io.sleep(Duration::from_secs(1));
            self.state = State::Scanning;
            self.last_block = 0;
            self.raw.clear();
        }
        let start = out.len();
        loop {
            if cancel.is_cancelled() {
                return Err(PixmaError::Cancelled);
            }
            if self.last_block & (BLOCK_END | BLOCK_NO_DATA) == BLOCK_END | BLOCK_NO_DATA {
                self.state = State::Finished;
                return Ok(0);
            }
            let (received, header) = match self.read_image_block() {
                Ok(r) => r,
                Err(e) => {
                    self.last_block = BLOCK_FLAGS;
                    if matches!(e, PixmaError::Cancelled)
                        && let Err(info) = self.read_error_info()
                    {
                        log::debug!(error:% = info; "reading error info failed");
                    }
                    return Err(e);
                }
            };
            let block_size = be32(&header[12..]);
            self.last_block = header[8] & BLOCK_FLAGS;
            if header[8] & !BLOCK_FLAGS != 0 {
                log::warn!(header:? = header; "unexpected result header");
            }
            if usize::try_from(block_size).ok() != Some(received) {
                log::warn!(block_size, received; "image block size mismatch");
            }
            if block_size == 0 {
                // No image data yet.
                self.io.sleep(Duration::from_millis(10));
            }
            self.post_process(sp, out);
            if out.len() > start {
                return Ok(out.len() - start);
            }
        }
    }

    /// Turns every complete raw line group into output pixels.
    fn post_process(&mut self, sp: &ScanParam, out: &mut Vec<u8>) {
        let c = usize::try_from(
            if sp.is_gray16() { 3 } else { sp.channels } * if sp.software_lineart { 8 } else { sp.depth } / 8,
        )
        .expect("small");
        let (w, xs, wx) = (sp.w as usize, sp.xs as usize, sp.wx as usize);
        let (cw, cx) = (c * w, c * xs);
        let scale = self.scale as usize;
        let line_size = self.cis_line_size(sp);
        if line_size == 0 {
            return;
        }
        let lines = self.raw.len() / line_size;
        let mut line = Vec::with_capacity(cw);
        for i in 0..lines {
            let src = &self.raw[i * line_size..(i + 1) * line_size];
            line.clear();
            if scale > 1 {
                image::shrink(&mut line, src, xs, w, wx, scale, c);
            } else {
                line.extend_from_slice(&src[cx..cx + cw]);
            }
            if sp.software_lineart {
                image::binarize_line(out, &mut line, w, &sp.lineart);
            } else if sp.is_gray16() {
                image::rgb_to_gray(out, &line, w, c);
            } else {
                out.extend_from_slice(&line);
            }
        }
        self.raw.drain(..lines * line_size);
    }

    /// Ends the current scan, if any (`mp150_finish_scan`).
    pub(crate) fn finish_scan(&mut self) {
        match self.state {
            State::Idle => return,
            State::Transferring => self.drain_bulk_in(),
            State::Scanning | State::Warmup | State::Finished => {}
        }
        if let Err(e) = self.abort_session() {
            log::warn!(error:% = e; "abort_session failed");
        }
        match self.send_xml_dialog(XML_END) {
            Ok(Ok(())) => {}
            Ok(Err(detail)) => log::warn!(detail:% = detail; "EndJob refused"),
            Err(e) => log::warn!(error:% = e; "EndJob failed"),
        }
        self.state = State::Idle;
        self.raw.clear();
    }
}

/// Explains the device's reasons for refusing a job, as SANE does.
fn describe_xml_detail(detail: &str) -> String {
    match detail {
        "DeviceBusy" => "The scanner is busy or not initialised yet. Check its USB power or try another port.".into(),
        "ScannerCarriageLockError" => "The scanner's carriage is locked. Unlock it (see the scanner's manual).".into(),
        "PCScanning" => "The scanner still considers a previous scan unfinished. Disconnect and reconnect it.".into(),
        "DeviceCheckError" => "The scanner detected a fault. Contact the repair center.".into(),
        other => format!("The scanner refused the scan: {other}"),
    }
}

//! Framing of pixma commands and transactions (`pixma_common.c`).
//!
//! A transaction is one bulk write followed by one bulk read. Commands start
//! with a 16-byte header (`u16be` command code, length at offset 14); data
//! after the header ends in a checksum byte making the data sum to zero.
//! Results start with an 8-byte header whose first `u16be` is the status.

use std::fmt;
use std::time::Duration;

use scan_usb::UsbTransport;

/// Status word of a successful command.
const STATUS_OK: u16 = 0x0606;
/// Status word of a command the device refused or aborted.
const STATUS_FAILED: u16 = 0x1515;
/// Status word while the device is busy.
const STATUS_BUSY: u16 = 0x1414;

const BULKIN_TIMEOUT: Duration = Duration::from_millis(1000);
const BULKOUT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Read attempts for a command's result. Starting a session while the head
/// is still parking after a cancelled scan takes about 5 seconds to answer.
const RESULT_ATTEMPTS: u32 = 8;

pub(crate) const CMD_HEADER_LEN: usize = 16;
pub(crate) const RES_HEADER_LEN: usize = 8;
const CMD_LEN_FIELD: usize = 14;
/// Size of the command buffer: the largest command is a 4096-entry gamma
/// table plus headers.
pub(crate) const CMDBUF_SIZE: usize = 4096 + 24;

/// A failed pixma operation, mirroring the backend's `PIXMA_E*` codes.
#[derive(Debug)]
pub(crate) enum PixmaError {
    /// `PIXMA_ETIMEDOUT`.
    Timeout,
    /// `PIXMA_EBUSY`: status `0x1414`.
    Busy,
    /// `PIXMA_ECANCELED`: status `0x1515`, or a cancel request.
    Cancelled,
    /// `PIXMA_EPROTO`.
    Protocol(String),
    /// `PIXMA_EINVAL`.
    Invalid(String),
    /// The device rejected the scan job with a reason of its own.
    Device(String),
    /// Any other transfer failure.
    Usb(scan_usb::Error),
}

impl fmt::Display for PixmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("the scanner didn't respond in time"),
            Self::Busy => f.write_str("the scanner is busy"),
            Self::Cancelled => f.write_str("scan cancelled"),
            Self::Protocol(m) => write!(f, "unexpected reply from the scanner: {m}"),
            Self::Invalid(m) | Self::Device(m) => f.write_str(m),
            Self::Usb(e) => e.fmt(f),
        }
    }
}

impl From<scan_usb::Error> for PixmaError {
    fn from(e: scan_usb::Error) -> Self {
        match e {
            scan_usb::Error::Timeout => Self::Timeout,
            other => Self::Usb(other),
        }
    }
}

impl From<PixmaError> for scan_core::Error {
    fn from(e: PixmaError) -> Self {
        use scan_core::ErrorKind as K;
        let kind = match &e {
            PixmaError::Timeout => K::Timeout,
            PixmaError::Busy => K::Busy,
            PixmaError::Cancelled => K::Cancelled,
            PixmaError::Protocol(_) => K::Protocol,
            PixmaError::Invalid(_) => K::Invalid,
            PixmaError::Device(_) => K::Device,
            PixmaError::Usb(scan_usb::Error::Disconnected | scan_usb::Error::NotFound) => K::NotFound,
            PixmaError::Usb(scan_usb::Error::Access(_)) => K::Access,
            PixmaError::Usb(_) => K::Io,
        };
        scan_core::Error::new(kind, e.to_string())
    }
}

pub(crate) type PResult<T> = Result<T, PixmaError>;

pub(crate) fn be16(buf: &[u8]) -> u16 {
    u16::from_be_bytes([buf[0], buf[1]])
}

pub(crate) fn be32(buf: &[u8]) -> u32 {
    u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
}

pub(crate) fn set_be16(v: u16, buf: &mut [u8]) {
    buf[..2].copy_from_slice(&v.to_be_bytes());
}

pub(crate) fn set_be32(v: u32, buf: &mut [u8]) {
    buf[..4].copy_from_slice(&v.to_be_bytes());
}

fn sum_bytes(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |s, &b| s.wrapping_add(b))
}

/// Maps a result status word to an error.
fn map_status(status: u16) -> PResult<()> {
    match status {
        STATUS_OK => Ok(()),
        STATUS_FAILED => Err(PixmaError::Cancelled),
        STATUS_BUSY => Err(PixmaError::Busy),
        other => Err(PixmaError::Protocol(format!("status 0x{other:04x}"))),
    }
}

/// The USB side of one open scanner (`pixma_io_sanei.c`).
#[derive(Debug)]
pub(crate) struct Io {
    pub(crate) usb: Box<dyn UsbTransport>,
}

impl Io {
    /// Writes `data`; a short write counts as a timeout, as in SANE.
    pub(crate) fn write(&mut self, data: &[u8]) -> PResult<()> {
        let n = self.usb.bulk_out(data, BULKOUT_TIMEOUT)?;
        if n != data.len() {
            log::warn!(written = n, len = data.len(); "incomplete write");
            return Err(PixmaError::Timeout);
        }
        Ok(())
    }

    pub(crate) fn read(&mut self, buf: &mut [u8]) -> PResult<usize> {
        Ok(self.usb.bulk_in(buf, BULKIN_TIMEOUT)?)
    }

    /// Waits for an interrupt packet; `timeout_ms` below 100 waits 100 ms,
    /// a negative one waits indefinitely.
    pub(crate) fn wait_interrupt(&mut self, buf: &mut [u8], timeout_ms: i32) -> PResult<usize> {
        let ms = if timeout_ms < 0 { u64::from(u32::MAX) } else { u64::from(timeout_ms.max(100).unsigned_abs()) };
        Ok(self.usb.interrupt_in(buf, Duration::from_millis(ms))?)
    }

    pub(crate) fn sleep(&mut self, d: Duration) {
        self.usb.sleep(d);
    }

    /// Writes `cmd` and reads the result into `result`, retrying the read
    /// while it times out. Returns the result length.
    pub(crate) fn transaction(&mut self, cmd: &[u8], result: &mut [u8]) -> PResult<usize> {
        self.write(cmd)?;
        let mut attempts = RESULT_ATTEMPTS;
        loop {
            match self.read(result) {
                Err(PixmaError::Timeout) if attempts > 1 => attempts -= 1,
                Err(e) => {
                    log::warn!(cmd = be16(cmd), error:% = e; "no response to command");
                    return Err(e);
                }
                ok => return ok,
            }
        }
    }
}

/// A command being built and its result (`pixma_cmdbuf_t`).
#[derive(Debug)]
pub(crate) struct CmdBuf {
    pub(crate) buf: Vec<u8>,
    cmdlen: usize,
    expected_reslen: usize,
    reslen: usize,
}

impl CmdBuf {
    pub(crate) fn new() -> Self {
        Self { buf: vec![0; CMDBUF_SIZE], cmdlen: 0, expected_reslen: 0, reslen: 0 }
    }

    /// Starts command `cmd` with `dataout` parameter bytes and `datain`
    /// result bytes. Returns the offset of the parameters (if any) or else of
    /// the result data in [`Self::buf`].
    pub(crate) fn new_cmd(&mut self, cmd: u16, dataout: usize, datain: usize) -> usize {
        let cmdlen = CMD_HEADER_LEN + dataout;
        let reslen = RES_HEADER_LEN + datain;
        assert!(cmdlen <= CMDBUF_SIZE && reslen <= CMDBUF_SIZE, "command too large");
        self.buf[..cmdlen].fill(0);
        self.cmdlen = cmdlen;
        self.expected_reslen = reslen;
        set_be16(cmd, &mut self.buf);
        let len = u16::try_from(dataout + datain).expect("lengths fit the command buffer");
        set_be16(len, &mut self.buf[CMD_LEN_FIELD..]);
        if dataout == 0 { RES_HEADER_LEN } else { CMD_HEADER_LEN }
    }

    /// Sends the command and checks its result (`pixma_exec`).
    pub(crate) fn exec(&mut self, io: &mut Io) -> PResult<()> {
        if self.cmdlen > CMD_HEADER_LEN {
            let end = self.cmdlen - 1;
            self.buf[end] = sum_bytes(&self.buf[CMD_HEADER_LEN..end]).wrapping_neg();
        }
        let cmd = self.buf[..self.cmdlen].to_vec();
        self.reslen = io.transaction(&cmd, &mut self.buf[..self.expected_reslen])?;
        self.check_result()
    }

    /// Sends a command without parameters or result data.
    pub(crate) fn exec_short(&mut self, io: &mut Io, cmd: u16) -> PResult<()> {
        self.new_cmd(cmd, 0, 0);
        self.exec(io)
    }

    /// Records the length of a result read by hand, as `read_image_block`
    /// does; such results aren't length-checked.
    pub(crate) fn set_raw_result(&mut self, len: usize) {
        self.reslen = len;
        self.expected_reslen = 0;
    }

    /// Validates the result header, length and checksum
    /// (`pixma_check_result`).
    pub(crate) fn check_result(&self) -> PResult<()> {
        let len = self.reslen;
        if len < RES_HEADER_LEN {
            return Err(PixmaError::Protocol(format!("{len}-byte result")));
        }
        let status = map_status(be16(&self.buf));
        if self.expected_reslen != 0 {
            if len == self.expected_reslen {
                if sum_bytes(&self.buf[RES_HEADER_LEN..len]) != 0 {
                    return Err(PixmaError::Protocol("bad result checksum".into()));
                }
            } else if len != RES_HEADER_LEN {
                // A command cut short (e.g. by the cancel button) returns
                // only a header; anything else is malformed.
                return Err(PixmaError::Protocol(format!("{len}-byte result, {} expected", self.expected_reslen)));
            }
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_carry_length_and_checksum() {
        let mut cb = CmdBuf::new();
        let ofs = cb.new_cmd(0xde20, 4, 0);
        assert_eq!(ofs, CMD_HEADER_LEN);
        cb.buf[ofs] = 0x10;
        cb.buf[ofs + 1] = 0x20;
        let end = cb.cmdlen - 1;
        cb.buf[end] = sum_bytes(&cb.buf[CMD_HEADER_LEN..end]).wrapping_neg();
        assert_eq!(&cb.buf[..2], [0xde, 0x20]);
        assert_eq!(be16(&cb.buf[14..]), 4);
        assert_eq!(sum_bytes(&cb.buf[CMD_HEADER_LEN..cb.cmdlen]), 0);
    }

    #[test]
    fn results_are_checked() {
        let mut cb = CmdBuf::new();
        let ofs = cb.new_cmd(0xda20, 0, 8);
        assert_eq!(ofs, RES_HEADER_LEN);
        cb.buf[..16].copy_from_slice(&[6, 6, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0xfc]);
        cb.reslen = 16;
        assert!(cb.check_result().is_ok());
        cb.buf[15] = 0;
        assert!(matches!(cb.check_result(), Err(PixmaError::Protocol(_))));
        // Header only, as when a command is cut short: the status decides.
        cb.buf[..2].copy_from_slice(&[0x15, 0x15]);
        cb.reslen = 8;
        assert!(matches!(cb.check_result(), Err(PixmaError::Cancelled)));
        cb.buf[..2].copy_from_slice(&[0x14, 0x14]);
        assert!(matches!(cb.check_result(), Err(PixmaError::Busy)));
    }
}

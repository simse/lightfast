//! Dell 1600n multi-function printer over the network.
//!
//! A port of SANE's `dell1600n_net` backend. Untested on real hardware.
//!
//! This scanner works backwards. The host registers its name over UDP and
//! waits; the user picks that name on the printer's panel and presses Start;
//! the printer then asks the host over UDP to connect back over TCP, and pushes
//! every page of the job. So [`Device::start`] blocks until the user acts or
//! the [`CancelToken`] fires, the panel decides what gets scanned, and the
//! settings here pick nothing but which page comes next.
//!
//! SANE's `extra_scanner` config line is covered by opening the name
//! `dell1600n_net:<host>` directly.

#![forbid(unsafe_code)]
#![expect(clippy::cast_possible_truncation, reason = "packet fields are 16 bits by protocol")]

mod packet;

use std::collections::VecDeque;
use std::io::{ErrorKind as IoKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use scan_core::driver::{NetDriver, invalid, mode, protocol};
use scan_core::{
    CancelToken, Capabilities, Channels, Device, DeviceInfo, Error, ErrorKind, Parameters, PixelLayout, Result,
    ScanSettings, SourceCaps, SourceKind,
};

use crate::packet::{Page, Session, discovery_packet, parse_find_response, subscribe_packet};

const BACKEND: &str = "dell1600n_net";
const SCANNER_PORT: u16 = 1124;
const REG_REPLY_WAIT: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_secs(1);
const SOCK_BUF_SIZE: usize = 2048;
/// The name shown on the printer's panel. SANE uses the host name, which safe
/// std can't read, so this falls back to `$HOSTNAME` and then SANE's default.
const DEFAULT_REG_NAME: &str = "Sane";
/// The session's default resolution, used before the scanner reports a page.
const DEFAULT_DPI: u32 = 200;
const MODE_COLOR: &str = "Color";

/// This backend, for the registry.
pub const DRIVER: NetDriver =
    NetDriver { name: BACKEND, discover, open: |address| Ok(Box::new(Dell1600n::open(address)?)) };

fn io_error(e: &std::io::Error) -> Error {
    Error::new(ErrorKind::Io, e.to_string())
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), IoKind::WouldBlock | IoKind::TimedOut)
}

/// Broadcasts a discovery packet and collects replies for a moment.
fn discover() -> Vec<DeviceInfo> {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else { return vec![] };
    if socket.set_broadcast(true).is_err() || socket.set_read_timeout(Some(REG_REPLY_WAIT)).is_err() {
        return vec![];
    }
    if socket.send_to(&discovery_packet(), ("255.255.255.255", SCANNER_PORT)).is_err() {
        return vec![];
    }
    let deadline = Instant::now() + REG_REPLY_WAIT;
    let mut found = Vec::new();
    let mut buf = [0u8; SOCK_BUF_SIZE];
    while Instant::now() < deadline {
        let Ok(n) = socket.recv(&mut buf) else { break };
        if let Some((address, model)) = parse_find_response(&buf[..n]) {
            found.push(DeviceInfo { name: format!("{BACKEND}:{address}"), vendor: "Dell".into(), model });
        }
    }
    found
}

/// The default frame: A4 at the session's default resolution. The real one is
/// only known once the printer has sent a page.
fn default_frame() -> Parameters {
    let (width, height) = (1654, 2339);
    Parameters {
        layout: PixelLayout { channels: Channels::Rgb, depth: 8 },
        pixels_per_line: width,
        bytes_per_line: width * 3,
        lines: height,
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        sources: vec![SourceCaps {
            name: "Printer panel".into(),
            kind: SourceKind::Feeder,
            width_mm: 210.0,
            height_mm: 297.0,
            modes: vec![mode(MODE_COLOR, Channels::Rgb, 8, &[DEFAULT_DPI])],
            gamma: None,
            extras: vec![],
        }],
    }
}

fn frame_of(page: &Page) -> Parameters {
    Parameters {
        layout: PixelLayout { channels: Channels::Rgb, depth: 8 },
        pixels_per_line: page.width,
        bytes_per_line: page.width * 3,
        lines: page.height,
    }
}

/// An open Dell 1600n.
#[derive(Debug)]
pub struct Dell1600n {
    info: DeviceInfo,
    caps: Capabilities,
    address: SocketAddr,
    reg_name: String,
    /// Pages of the current job not yet started.
    pages: VecDeque<Page>,
    current: Option<(Page, usize)>,
}

impl Dell1600n {
    /// Resolves `host`. The printer isn't contacted until a scan starts.
    ///
    /// # Errors
    /// [`ErrorKind::Invalid`] if the host can't be resolved.
    pub fn open(host: &str) -> Result<Self> {
        let address = (host, SCANNER_PORT)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.find(SocketAddr::is_ipv4))
            .ok_or_else(|| invalid(format!("can't resolve {host}")))?;
        let info = DeviceInfo { name: format!("{BACKEND}:{host}"), vendor: "Dell".into(), model: "1600n".into() };
        let reg_name = std::env::var("HOSTNAME")
            .ok()
            .and_then(|h| h.split('.').next().map(str::to_owned))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_REG_NAME.to_owned());
        Ok(Self { info, caps: capabilities(), address, reg_name, pages: VecDeque::new(), current: None })
    }

    /// Registers with the printer and waits for it to send a job.
    fn wait_for_job(&mut self, cancel: &CancelToken) -> Result<()> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|e| io_error(&e))?;
        socket.connect(self.address).map_err(|e| io_error(&e))?;
        socket.set_read_timeout(Some(POLL)).map_err(|e| io_error(&e))?;
        let SocketAddr::V4(local) = socket.local_addr().map_err(|e| io_error(&e))? else {
            return Err(protocol("the printer needs an IPv4 connection"));
        };
        socket.send(&subscribe_packet(&self.reg_name, local.ip().octets())).map_err(|e| io_error(&e))?;

        let mut buf = [0u8; SOCK_BUF_SIZE];
        loop {
            cancel.check()?;
            match socket.recv(&mut buf) {
                Ok(0) => return Err(protocol("the printer closed the connection")),
                Ok(n) => {
                    if packet::requests_tcp_connection(&buf[..n])? {
                        let stream = TcpStream::connect(self.address).map_err(|e| io_error(&e))?;
                        stream.set_read_timeout(Some(POLL)).map_err(|e| io_error(&e))?;
                        self.pages.extend(run_session(stream, cancel)?);
                        return Ok(());
                    }
                }
                Err(e) if is_timeout(&e) => {}
                Err(e) => return Err(io_error(&e)),
            }
        }
    }
}

/// Answers the printer's TCP session until it hangs up; returns the pages.
fn run_session<S: Read + Write>(mut stream: S, cancel: &CancelToken) -> Result<Vec<Page>> {
    let mut session = Session::new();
    let mut pending = Vec::new();
    let mut buf = [0u8; SOCK_BUF_SIZE];
    loop {
        cancel.check()?;
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(io_error(&e)),
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(used) = session.process(&pending)? {
            for reply in session.take_replies() {
                stream.write_all(&reply).map_err(|e| io_error(&e))?;
            }
            pending.drain(..used);
            if pending.is_empty() {
                break;
            }
        }
        if session.ended() {
            break;
        }
    }
    Ok(session.into_pages())
}

impl Device for Dell1600n {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn parameters(&self, settings: &ScanSettings) -> Result<Parameters> {
        scan_core::driver::resolve(&self.caps, settings)?;
        Ok(self.pages.front().map_or_else(default_frame, frame_of))
    }

    fn start(&mut self, settings: &ScanSettings, cancel: &CancelToken) -> Result<Parameters> {
        scan_core::driver::resolve(&self.caps, settings)?;
        self.current = None;
        if self.pages.is_empty() {
            self.wait_for_job(cancel)?;
        }
        let page = self.pages.pop_front().ok_or_else(|| Error::new(ErrorKind::Device, "the printer sent no pages"))?;
        let params = frame_of(&page);
        self.current = Some((page, 0));
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some((page, offset)) = &mut self.current else { return Err(invalid("no scan is running")) };
        let rest = &page.rgb[*offset..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        *offset += n;
        Ok(n)
    }

    fn finish(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A stream that reads from a script and records what's written.
    #[derive(Debug)]
    struct Script {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_cancelled_session_stops_at_once() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let stream = Script { input: Cursor::new(vec![]), output: vec![] };
        assert!(run_session(stream, &cancel).unwrap_err().is_cancelled());
    }

    #[test]
    fn a_session_with_no_pages_returns_none() {
        let stream = Script { input: Cursor::new(vec![]), output: vec![] };
        assert_eq!(run_session(stream, &CancelToken::new()).unwrap(), [] as [packet::Page; 0]);
    }
}

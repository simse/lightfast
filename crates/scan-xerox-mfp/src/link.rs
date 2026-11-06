//! The two ways to reach the scanner (`xerox_mfp-usb.c`, `xerox_mfp-tcp.c`).

use std::io::{ErrorKind as IoKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use scan_core::driver::invalid;
use scan_core::{Error, ErrorKind, Result};
use scan_usb::UsbTransport;

const USB_TIMEOUT: Duration = Duration::from_secs(30);
const USB_FLUSH_TIMEOUT: Duration = Duration::from_millis(100);
const TCP_RECV_TIMEOUT: Duration = Duration::from_secs(1);
const TCP_DATA_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_DEFAULT_PORT: u16 = 9400;

pub(crate) trait Link: Send + std::fmt::Debug {
    fn send(&mut self, command: &[u8]) -> Result<()>;
    /// Reads a command's reply; returns its length, which may be short.
    fn recv_reply(&mut self, buf: &mut [u8]) -> Result<usize>;
    /// Reads image data; returns its length.
    fn recv_data(&mut self, buf: &mut [u8]) -> Result<usize>;
    /// Discards whatever the device is still sending.
    fn flush(&mut self);
    fn pause(&mut self, duration: Duration);
}

#[derive(Debug)]
pub(crate) struct UsbLink(pub Box<dyn UsbTransport>);

impl Link for UsbLink {
    fn send(&mut self, command: &[u8]) -> Result<()> {
        self.0.bulk_out(command, USB_TIMEOUT)?;
        Ok(())
    }

    fn recv_reply(&mut self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.0.bulk_in(buf, USB_TIMEOUT)?)
    }

    fn recv_data(&mut self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.0.bulk_in(buf, USB_TIMEOUT)?)
    }

    fn flush(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.0.bulk_in(&mut buf, USB_FLUSH_TIMEOUT), Ok(n) if n > 0) {}
    }

    fn pause(&mut self, duration: Duration) {
        self.0.sleep(duration);
    }
}

#[derive(Debug)]
pub(crate) struct TcpLink(pub TcpStream);

fn io_error(e: &std::io::Error) -> Error {
    Error::new(ErrorKind::Io, e.to_string())
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), IoKind::WouldBlock | IoKind::TimedOut)
}

impl TcpLink {
    /// Connects to `host` or `host:port`, port 9400 by default.
    pub(crate) fn connect(address: &str) -> Result<Self> {
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| invalid(format!("bad port in {address}")))?),
            None => (address, TCP_DEFAULT_PORT),
        };
        let target = (host, port).to_socket_addrs().map_err(|e| invalid(format!("can't resolve {host}: {e}")))?;
        let stream = target
            .into_iter()
            .next()
            .ok_or_else(|| invalid(format!("can't resolve {host}")))
            .and_then(|a| TcpStream::connect(a).map_err(|e| io_error(&e)))?;
        stream.set_read_timeout(Some(TCP_RECV_TIMEOUT)).map_err(|e| io_error(&e))?;
        Ok(Self(stream))
    }
}

impl Link for TcpLink {
    fn send(&mut self, command: &[u8]) -> Result<()> {
        self.0.write_all(command).map_err(|e| io_error(&e))
    }

    /// A timeout or hang-up gives a short reply rather than an error.
    fn recv_reply(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            match self.0.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == IoKind::Interrupted => {}
                Err(_) => break,
            }
        }
        Ok(got)
    }

    /// Fills `buf`, tolerating up to 30 idle seconds between arrivals.
    fn recv_data(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut got = 0;
        let mut last_data = Instant::now();
        while got < buf.len() {
            match self.0.read(&mut buf[got..]) {
                Ok(0) => return Err(Error::new(ErrorKind::Io, "the scanner closed the connection")),
                Ok(n) => {
                    got += n;
                    last_data = Instant::now();
                }
                Err(e) if e.kind() == IoKind::Interrupted => {}
                Err(e) if is_timeout(&e) && last_data.elapsed() < TCP_DATA_TIMEOUT => {}
                Err(e) => return Err(io_error(&e)),
            }
        }
        Ok(got)
    }

    fn flush(&mut self) {
        if self.0.set_nonblocking(true).is_ok() {
            let mut buf = [0u8; 256];
            while matches!(self.0.read(&mut buf), Ok(n) if n > 0) {}
            let _ = self.0.set_nonblocking(false);
        }
    }

    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn tcp_requests_round_trip_and_short_replies_are_not_errors() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut cmd = [0u8; 4];
            s.read_exact(&mut cmd).unwrap();
            s.write_all(&[0xa8, cmd[2]]).unwrap();
        });
        let mut link = TcpLink::connect(&format!("127.0.0.1:{port}")).unwrap();
        link.send(&[0x1b, 0xa8, 0x12, 0]).unwrap();
        let mut reply = [0u8; 8];
        assert_eq!(link.recv_reply(&mut reply).unwrap(), 2);
        assert_eq!(&reply[..2], [0xa8, 0x12]);
        server.join().unwrap();
        assert!(link.recv_data(&mut reply).is_err(), "a hang-up during data is an error");
    }

    #[test]
    fn bad_addresses_are_rejected() {
        assert!(TcpLink::connect("host:notaport").is_err());
    }
}

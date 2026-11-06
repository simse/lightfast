//! Replays USB sessions recorded by C SANE (`fakeusbout:`).
//!
//! The format is SANE's `sanei_usb` record/replay XML: a `<description>` of
//! the device followed by `<transactions>`, each a `control_tx`, `bulk_tx` or
//! `interrupt_tx` with hex payload. A failed read is recorded with
//! `error="timeout"` and no payload.
//!
//! Replay is strict: every write must match the recorded bytes and every
//! transfer must be the next recorded one, on the same endpoint and in the
//! same direction. Reads return the recorded data. That makes a replay a
//! golden test of the driver's traffic, not just its output.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::{DeviceIds, Error, Result, UsbTransport};

/// How a replayed session diverged from its recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The recording couldn't be read or parsed.
    Load(String),
    /// The driver issued a different transfer than the one recorded next.
    Mismatch { seq: u32, expected: String, got: String },
    /// The driver kept going after the last recorded transfer.
    End { got: String },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(m) => write!(f, "can't load recording: {m}"),
            Self::Mismatch { seq, expected, got } => {
                write!(f, "transaction {seq}: expected {expected}, driver issued {got}")
            }
            Self::End { got } => write!(f, "recording ended, driver issued {got}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Control { request_type: u8, request: u8, value: u16, index: u16 },
    BulkOut,
    BulkIn,
    InterruptIn,
}

#[derive(Debug, Clone)]
struct Transaction {
    seq: u32,
    kind: Kind,
    /// `None` for a read recorded as failed.
    data: Option<Vec<u8>>,
}

/// Summarises a payload for mismatch messages.
fn describe(kind: &str, data: &[u8]) -> String {
    let head: Vec<String> = data.iter().take(24).map(|b| format!("{b:02x}")).collect();
    let more = if data.len() > 24 { " …" } else { "" };
    format!("{kind} of {} bytes [{}{more}]", data.len(), head.join(" "))
}

fn parse_hex(text: &str) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 3 + 1);
    for token in text.split_ascii_whitespace() {
        out.push(u8::from_str_radix(token, 16).map_err(|e| format!("bad hex byte '{token}': {e}"))?);
    }
    Ok(out)
}

fn parse_num(s: &str) -> std::result::Result<u32, String> {
    let r = match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    };
    r.map_err(|e| format!("bad number '{s}': {e}"))
}

fn attr_num(node: roxmltree::Node<'_, '_>, name: &str) -> std::result::Result<u32, String> {
    let v = node.attribute(name).ok_or_else(|| format!("<{}> lacks '{name}'", node.tag_name().name()))?;
    parse_num(v)
}

fn narrow<T: TryFrom<u32>>(v: u32, what: &str) -> std::result::Result<T, String> {
    T::try_from(v).map_err(|_| format!("{what} {v} out of range"))
}

fn parse_transaction(node: roxmltree::Node<'_, '_>) -> std::result::Result<Option<Transaction>, String> {
    let tag = node.tag_name().name();
    let seq = attr_num(node, "seq")?;
    let dir_in = node.attribute("direction") == Some("IN");
    let kind = match tag {
        "control_tx" => Kind::Control {
            request_type: narrow(attr_num(node, "bmRequestType")?, "bmRequestType")?,
            request: narrow(attr_num(node, "bRequest")?, "bRequest")?,
            value: narrow(attr_num(node, "wValue")?, "wValue")?,
            index: narrow(attr_num(node, "wIndex")?, "wIndex")?,
        },
        "bulk_tx" if dir_in => Kind::BulkIn,
        "bulk_tx" => Kind::BulkOut,
        "interrupt_tx" if dir_in => Kind::InterruptIn,
        // Debug markers and unknown nodes carry no traffic.
        _ => return Ok(None),
    };
    let data = if node.attribute("error").is_some() { None } else { Some(parse_hex(node.text().unwrap_or(""))?) };
    Ok(Some(Transaction { seq, kind, data }))
}

/// Reads the serial number from the leading descriptor requests, the way
/// SANE's pixma backend does when it opens a device.
fn serial_from_descriptors(controls: &[Transaction]) -> Option<String> {
    let device = controls.iter().find_map(|t| match (t.kind, &t.data) {
        (Kind::Control { request: 6, value: 0x0100, .. }, Some(d)) if d.len() >= 18 => Some(d.clone()),
        _ => None,
    })?;
    let index = device[16];
    if index == 0 {
        return None;
    }
    let want = 0x0300 | u16::from(index);
    controls.iter().find_map(|t| match (t.kind, &t.data) {
        (Kind::Control { request: 6, value, .. }, Some(d)) if value == want && d.len() >= 2 => {
            let len = usize::from(d[0]).min(d.len());
            let units: Vec<u16> = d[2..len].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    })
}

/// A recorded session played back as a [`UsbTransport`].
#[derive(Debug)]
pub struct Replay {
    ids: DeviceIds,
    transactions: Vec<Transaction>,
    next: usize,
}

impl Replay {
    /// Loads a recording from a file.
    ///
    /// # Errors
    /// Fails if the file can't be read or isn't a SANE USB recording.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Replay(ReplayError::Load(format!("{}: {e}", path.display()))))?;
        Self::parse(&text)
    }

    /// Parses a recording held in memory.
    ///
    /// # Errors
    /// Fails if `xml` isn't a SANE USB recording.
    pub fn parse(xml: &str) -> Result<Self> {
        Self::parse_inner(xml).map_err(|m| Error::Replay(ReplayError::Load(m)))
    }

    fn parse_inner(xml: &str) -> std::result::Result<Self, String> {
        let options = roxmltree::ParsingOptions { nodes_limit: u32::MAX, ..Default::default() };
        let doc = roxmltree::Document::parse_with_options(xml, options).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        if root.tag_name().name() != "device_capture" {
            return Err(format!("root element is <{}>, not <device_capture>", root.tag_name().name()));
        }
        let child = |name: &str| root.children().find(|n| n.has_tag_name(name));
        let description = child("description").ok_or("no <description>")?;
        let vendor_id = narrow(attr_num(description, "id_vendor")?, "id_vendor")?;
        let product_id = narrow(attr_num(description, "id_product")?, "id_product")?;
        let mut transactions = Vec::new();
        for node in child("transactions").ok_or("no <transactions>")?.children().filter(roxmltree::Node::is_element) {
            transactions.extend(parse_transaction(node)?);
        }
        // Descriptor requests made while opening are the transport's job
        // (the OS caches them), not the driver's, so they are consumed here.
        // Vendor and class requests are the driver's own traffic.
        let leading = transactions
            .iter()
            .take_while(|t| matches!(t.kind, Kind::Control { request_type, .. } if request_type & 0x60 == 0))
            .count();
        let serial = serial_from_descriptors(&transactions[..leading]);
        Ok(Self { ids: DeviceIds { vendor_id, product_id, serial }, transactions, next: leading })
    }

    /// Recorded transfers not yet replayed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.transactions.len() - self.next
    }

    /// Whether the next recorded transfer is a bulk write starting with
    /// `prefix`. Lets tests find where a recorded session was cancelled.
    #[must_use]
    pub fn next_write_starts_with(&self, prefix: &[u8]) -> bool {
        self.transactions
            .get(self.next)
            .is_some_and(|t| t.kind == Kind::BulkOut && t.data.as_deref().is_some_and(|d| d.starts_with(prefix)))
    }

    /// Takes the next recorded transfer, which must be of kind `want`.
    fn take(&mut self, want: Kind, got: impl FnOnce() -> String) -> Result<Transaction> {
        let Some(t) = self.transactions.get(self.next) else {
            return Err(Error::Replay(ReplayError::End { got: got() }));
        };
        if t.kind != want {
            let expected = match (&t.kind, &t.data) {
                (Kind::BulkOut, Some(d)) => describe("bulk write", d),
                (kind, _) => format!("{kind:?}"),
            };
            return Err(Error::Replay(ReplayError::Mismatch { seq: t.seq, expected, got: got() }));
        }
        self.next += 1;
        Ok(t.clone())
    }

    fn read(&mut self, kind: Kind, name: &str, buf: &mut [u8]) -> Result<usize> {
        let t = self.take(kind, || format!("{name} of up to {} bytes", buf.len()))?;
        let Some(data) = t.data else { return Err(Error::Timeout) };
        if data.len() > buf.len() {
            return Err(Error::Replay(ReplayError::Mismatch {
                seq: t.seq,
                expected: format!("{name} of {} bytes", data.len()),
                got: format!("{name} of up to {} bytes", buf.len()),
            }));
        }
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

impl UsbTransport for Replay {
    fn ids(&self) -> &DeviceIds {
        &self.ids
    }

    fn bulk_out(&mut self, data: &[u8], _timeout: Duration) -> Result<usize> {
        let t = self.take(Kind::BulkOut, || describe("bulk write", data))?;
        match t.data {
            Some(expected) if expected == data => Ok(data.len()),
            Some(expected) => Err(Error::Replay(ReplayError::Mismatch {
                seq: t.seq,
                expected: describe("bulk write", &expected),
                got: describe("bulk write", data),
            })),
            None => Err(Error::Timeout),
        }
    }

    fn bulk_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        self.read(Kind::BulkIn, "bulk read", buf)
    }

    fn interrupt_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        self.read(Kind::InterruptIn, "interrupt read", buf)
    }

    fn control_in(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize> {
        let kind = Kind::Control { request_type, request, value, index };
        self.read(kind, "control read", buf)
    }

    fn control_out(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        _timeout: Duration,
    ) -> Result<usize> {
        let kind = Kind::Control { request_type, request, value, index };
        let t = self.take(kind, || describe("control write", data))?;
        match t.data {
            Some(expected) if expected == data => Ok(data.len()),
            Some(expected) => Err(Error::Replay(ReplayError::Mismatch {
                seq: t.seq,
                expected: describe("control write", &expected),
                got: describe("control write", data),
            })),
            None => Err(Error::Timeout),
        }
    }

    fn sleep(&mut self, _duration: Duration) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<device_capture backend="pixma">
  <description id_vendor="0x04a9" id_product="0x1912"/>
  <transactions>
    <control_tx seq="1" direction="IN" bmRequestType="0x80" bRequest="0x06" wValue="0x0100" wIndex="0x00" wLength="0x12">12 01 00 02 00 00 00 40 a9 04 12 19 00 01 01 02 03 01</control_tx>
    <control_tx seq="2" direction="IN" bmRequestType="0x80" bRequest="0x06" wValue="0x0300" wIndex="0x00" wLength="0x04">04 03 09 04</control_tx>
    <control_tx seq="3" direction="IN" bmRequestType="0x80" bRequest="0x06" wValue="0x0303" wIndex="0x0409" wLength="0x40">08 03 35 00 30 00 38 00 ff ff</control_tx>
    <bulk_tx seq="4" endpoint_number="7" direction="OUT">db 20
00 00</bulk_tx>
    <bulk_tx seq="5" endpoint_number="8" direction="IN">06 06 00</bulk_tx>
    <interrupt_tx seq="6" endpoint_number="9" direction="IN" error="timeout"/>
  </transactions>
</device_capture>"#;

    #[test]
    fn replays_a_session() {
        let mut r = Replay::parse(XML).unwrap();
        assert_eq!(r.ids(), &DeviceIds { vendor_id: 0x04a9, product_id: 0x1912, serial: Some("508".into()) });
        assert_eq!(r.remaining(), 3);
        assert!(r.next_write_starts_with(&[0xdb]));
        let t = Duration::from_secs(1);
        assert_eq!(r.bulk_out(&[0xdb, 0x20, 0, 0], t).unwrap(), 4);
        let mut buf = [0; 8];
        assert_eq!(r.bulk_in(&mut buf, t).unwrap(), 3);
        assert_eq!(&buf[..3], [6, 6, 0]);
        assert!(matches!(r.interrupt_in(&mut buf, t), Err(Error::Timeout)));
        assert!(matches!(r.bulk_in(&mut buf, t), Err(Error::Replay(ReplayError::End { .. }))));
    }

    #[test]
    fn rejects_divergent_traffic() {
        let t = Duration::from_secs(1);
        let mut r = Replay::parse(XML).unwrap();
        let err = r.bulk_out(&[0xdb, 0x21, 0, 0], t).unwrap_err();
        assert!(matches!(err, Error::Replay(ReplayError::Mismatch { seq: 4, .. })), "{err}");

        let mut r = Replay::parse(XML).unwrap();
        let err = r.bulk_in(&mut [0; 8], t).unwrap_err();
        assert!(matches!(err, Error::Replay(ReplayError::Mismatch { seq: 4, .. })), "{err}");

        let mut r = Replay::parse(XML).unwrap();
        r.bulk_out(&[0xdb, 0x20, 0, 0], t).unwrap();
        let err = r.bulk_in(&mut [0; 2], t).unwrap_err();
        assert!(matches!(err, Error::Replay(ReplayError::Mismatch { seq: 5, .. })), "short buffer: {err}");
    }

    #[test]
    fn vendor_control_transfers_are_the_drivers_traffic() {
        let xml = r#"<device_capture><description id_vendor="0x1" id_product="0x2"/><transactions>
            <control_tx seq="1" direction="IN" bmRequestType="0x80" bRequest="0x06" wValue="0x0100" wIndex="0" wLength="0x12">12 01</control_tx>
            <control_tx seq="2" direction="OUT" bmRequestType="0x40" bRequest="0x0c" wValue="0x87" wIndex="0x10" wLength="1">05</control_tx>
            <control_tx seq="3" direction="IN" bmRequestType="0xc0" bRequest="0x0c" wValue="0x07" wIndex="0x00" wLength="1">aa</control_tx>
        </transactions></device_capture>"#;
        let t = Duration::from_secs(1);
        let mut r = Replay::parse(xml).unwrap();
        assert_eq!(r.remaining(), 2);
        assert!(r.control_out(0x40, 0x0c, 0x88, 0x10, &[5], t).is_err(), "wrong wValue must not match");
        let mut r = Replay::parse(xml).unwrap();
        assert!(r.control_out(0x40, 0x0c, 0x87, 0x10, &[6], t).is_err(), "wrong payload must not match");
        let mut r = Replay::parse(xml).unwrap();
        r.control_out(0x40, 0x0c, 0x87, 0x10, &[5], t).unwrap();
        let mut b = [0];
        assert_eq!(r.control_in(0xc0, 0x0c, 0x07, 0, &mut b, t).unwrap(), 1);
        assert_eq!(b, [0xaa]);
    }

    #[test]
    fn rejects_other_documents() {
        assert!(Replay::parse("<foo/>").is_err());
        assert!(Replay::parse("not xml").is_err());
    }
}

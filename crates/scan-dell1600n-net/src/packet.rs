//! The printer's message format and the session state it drives.
//!
//! A packet is an 8-byte header holding the body's length in its last two
//! bytes, then a run of items: a type byte, a 16-bit length and a name, a value
//! type byte, a 16-bit length and a value. Everything is big-endian.

use scan_core::Result;
use scan_core::driver::protocol;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

const COMPRESSION_G4: u32 = 0x08;
const COMPRESSION_JPEG: u32 = 0x20;

/// `InitPacket`, `AppendMessageToPacket` and `FinalisePacket`.
struct Packet(Vec<u8>);

impl Packet {
    fn new(kind: u8) -> Self {
        Self(vec![2, 0, kind, 2, 0, 0, 0, 0])
    }

    fn add(mut self, message_type: u8, name: &str, value_type: u8, value: &[u8]) -> Self {
        self.0.push(message_type);
        self.0.extend_from_slice(&(name.len() as u16).to_be_bytes());
        self.0.extend_from_slice(name.as_bytes());
        self.0.push(value_type);
        self.0.extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.0.extend_from_slice(value);
        self
    }

    fn finish(mut self) -> Vec<u8> {
        let body = (self.0.len() - 8) as u16;
        self.0[6..8].copy_from_slice(&body.to_be_bytes());
        self.0
    }
}

pub(crate) fn discovery_packet() -> Vec<u8> {
    Packet::new(0x01).add(0x25, "std-scan-discovery-all", 0x02, &[0]).finish()
}

pub(crate) fn subscribe_packet(name: &str, ip: [u8; 4]) -> Vec<u8> {
    Packet::new(0x01)
        .add(0x22, "std-scan-subscribe-user-name", 0x0b, name.as_bytes())
        .add(0x22, "std-scan-subscribe-ip-address", 0x0a, &ip)
        .finish()
}

/// The whole packet's length if `data` holds a complete one.
fn complete_len(data: &[u8]) -> Option<usize> {
    let size = usize::from(*data.get(6)?) << 8 | usize::from(*data.get(7)?);
    (data.len() >= size + 8).then_some(size + 8)
}

#[derive(Debug, Clone, Copy)]
struct Item<'a> {
    name: &'a [u8],
    value: &'a [u8],
    /// Everything after the value, up to the end of the packet.
    rest: &'a [u8],
}

/// The next item in `body`, or `None` if it runs past the end. The C code
/// doesn't check and reads whatever follows.
fn next_item(body: &[u8]) -> Option<Item<'_>> {
    let name_size = usize::from(*body.get(1)?) << 8 | usize::from(*body.get(2)?);
    let name_end = 3 + name_size;
    let name = body.get(3..name_end)?;
    let value_size = usize::from(*body.get(name_end + 1)?) << 8 | usize::from(*body.get(name_end + 2)?);
    let value_start = name_end + 3;
    let value = body.get(value_start..value_start + value_size)?;
    Some(Item { name, value, rest: &body[value_start + value_size..] })
}

/// `strncmp(known, name, name_size)`: the name is matched as a prefix.
fn is(item: &Item<'_>, known: &str) -> bool {
    !item.name.is_empty() && known.as_bytes().starts_with(item.name)
}

/// A discovery reply's IP address and model, without any `Dell ` prefix.
pub(crate) fn parse_find_response(data: &[u8]) -> Option<(String, String)> {
    let len = complete_len(data)?;
    let mut body = &data[8..len];
    let mut address = None;
    let mut model = "1600n".to_owned();
    while let Some(item) = next_item(body) {
        body = item.rest;
        if is(&item, "std-scan-discovery-ip") && item.value.len() >= 4 {
            let v = item.value;
            address = Some(format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]));
        } else if is(&item, "std-scan-discovery-model-name") {
            String::from_utf8_lossy(item.value).trim_end_matches('\0').clone_into(&mut model);
        }
    }
    let model = model.strip_prefix("Dell ").map_or(model.clone(), str::to_owned);
    Some((address?, model))
}

/// Whether a UDP packet asks the host to connect back over TCP.
pub(crate) fn requests_tcp_connection(data: &[u8]) -> Result<bool> {
    let len = complete_len(data).ok_or_else(|| protocol("incomplete packet from the printer"))?;
    let mut body = &data[8..len];
    while let Some(item) = next_item(body) {
        body = item.rest;
        if is(&item, "std-scan-request-tcp-connection") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn be_u32(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

/// A decoded page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

fn decode_jpeg(data: &[u8]) -> Result<Page> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
    let rgb = decoder.decode().map_err(|e| protocol(format!("bad JPEG from the printer: {e:?}")))?;
    let info = decoder.info().ok_or_else(|| protocol("JPEG without headers"))?;
    Ok(Page { width: usize::from(info.width), height: usize::from(info.height), rgb })
}

/// Decodes CCITT G4. As in the TIFF the C code wraps it in, white is 0, and
/// the output is RGB.
fn decode_g4(data: &[u8], width: u32, height: u32) -> Result<Page> {
    let too_big = || protocol("G4 page dimensions out of range");
    let w = u16::try_from(width).map_err(|_| too_big())?;
    let h = u16::try_from(height).map_err(|_| too_big())?;
    let (width, height) = (usize::from(w), usize::from(h));
    let mut rgb = Vec::with_capacity(width * height * 3);
    fax::decoder::decode_g4(data.iter().copied(), w, Some(h), |line| {
        for pel in fax::decoder::pels(line, w) {
            let v = if pel == fax::Color::White { 255 } else { 0 };
            rgb.extend_from_slice(&[v, v, v]);
        }
    });
    // A truncated page is padded white, as libtiff's RGBA reader leaves it.
    rgb.resize(width * height * 3, 255);
    Ok(Page { width, height, rgb })
}

/// The printer's TCP session (`ProcessTcpResponse`).
#[derive(Debug)]
pub(crate) struct Session {
    replies: Vec<Vec<u8>>,
    xres: [u8; 2],
    yres: [u8; 2],
    composition: [u8; 4],
    brightness: u8,
    compression: u32,
    file_type: [u8; 4],
    pixel_width: u32,
    pixel_height: u32,
    page_data: Vec<u8>,
    pages: Vec<Page>,
    ended: bool,
}

impl Session {
    pub(crate) fn new() -> Self {
        Self {
            replies: Vec::new(),
            xres: 200u16.to_be_bytes(),
            yres: 200u16.to_be_bytes(),
            composition: 1u32.to_be_bytes(),
            brightness: 0x80,
            compression: COMPRESSION_G4,
            file_type: 2u32.to_be_bytes(),
            pixel_width: 0,
            pixel_height: 0,
            page_data: Vec::new(),
            pages: Vec::new(),
            ended: false,
        }
    }

    pub(crate) fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.replies)
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    pub(crate) fn into_pages(self) -> Vec<Page> {
        self.pages
    }

    fn respond(&mut self, name: &str) {
        self.replies.push(Packet::new(0x02).add(0x22, name, 0x05, &[0; 4]).finish());
    }

    fn client_prefs(&mut self) {
        let zero = [0u8; 4];
        let packet = Packet::new(0x02)
            .add(0x22, "std-scan-getclientpref-x1", 0x05, &zero)
            .add(0x22, "std-scan-getclientpref-x2", 0x05, &zero)
            .add(0x22, "std-scan-getclientpref-y1", 0x05, &zero)
            .add(0x22, "std-scan-getclientpref-y2", 0x05, &zero)
            .add(0x22, "std-scan-getclientpref-xresolution", 0x04, &self.xres)
            .add(0x22, "std-scan-getclientpref-yresolution", 0x04, &self.yres)
            .add(0x22, "std-scan-getclientpref-image-composition", 0x06, &self.composition)
            .add(0x22, "std-scan-getclientpref-brightness", 0x02, &[self.brightness])
            .add(0x22, "std-scan-getclientpref-image-compression", 0x06, &self.compression.to_be_bytes())
            .add(0x22, "std-scan-getclientpref-file-type", 0x06, &self.file_type)
            .add(0x22, "std-scan-getclientpref-paper-size-detect", 0x06, &zero)
            .add(0x22, "std-scan-getclientpref-paper-scanner-type", 0x06, &zero)
            .finish();
        self.replies.push(packet);
    }

    /// `ProcessPageData`.
    fn finish_page(&mut self) -> Result<()> {
        if self.page_data.is_empty() {
            return Ok(());
        }
        let page = match self.compression {
            COMPRESSION_JPEG => decode_jpeg(&self.page_data)?,
            COMPRESSION_G4 => decode_g4(&self.page_data, self.pixel_width, self.pixel_height)?,
            other => return Err(protocol(format!("unknown image compression {other:#x}"))),
        };
        self.pages.push(page);
        Ok(())
    }

    /// Handles one packet at the start of `data`. Returns the bytes consumed,
    /// or `None` if more data is needed.
    pub(crate) fn process(&mut self, data: &[u8]) -> Result<Option<usize>> {
        let Some(len) = complete_len(data) else { return Ok(None) };
        let mut body = &data[8..len];
        let mut process_image = false;
        while let Some(item) = next_item(body) {
            body = item.rest;
            if is(&item, "std-scan-session-open") {
                self.respond("std-scan-session-open-response");
            } else if is(&item, "std-scan-getclientpref") {
                self.client_prefs();
            } else if is(&item, "std-scan-document-start") {
                self.respond("std-scan-document-start-response");
            } else if is(&item, "std-scan-document-file-type") {
                if let Some(v) = item.value.get(..4) {
                    self.file_type.copy_from_slice(v);
                }
            } else if is(&item, "std-scan-document-image-compression") {
                self.compression = be_u32(item.value).unwrap_or(self.compression);
            } else if is(&item, "std-scan-document-xresolution") {
                if let Some(v) = item.value.get(..2) {
                    self.xres.copy_from_slice(v);
                }
            } else if is(&item, "std-scan-document-yresolution") {
                if let Some(v) = item.value.get(..2) {
                    self.yres.copy_from_slice(v);
                }
            } else if is(&item, "std-scan-page-widthpixel") {
                self.pixel_width = be_u32(item.value).unwrap_or(0);
            } else if is(&item, "std-scan-page-heightpixel") {
                self.pixel_height = be_u32(item.value).unwrap_or(0);
            } else if is(&item, "std-scan-page-start") {
                self.respond("std-scan-page-start-response");
                self.page_data.clear();
                self.pixel_width = 0;
                self.pixel_height = 0;
            } else if is(&item, "std-scan-page-end") {
                process_image = true;
                self.respond("std-scan-page-end-response");
            } else if is(&item, "std-scan-document-end") {
                self.respond("std-scan-document-end-response");
                self.page_data.clear();
            } else if is(&item, "std-scan-session-end") {
                self.respond("std-scan-session-end-response");
                self.ended = true;
            } else if is(&item, "std-scan-scandata-error") {
                // The image data follows the item with an 8-byte header of its own.
                let Some(size) = item.rest.get(6..8) else { break };
                let size = usize::from(size[0]) << 8 | usize::from(size[1]);
                let Some(chunk) = item.rest.get(8..8 + size) else { break };
                self.page_data.extend_from_slice(chunk);
                body = &item.rest[8 + size..];
            }
        }
        if process_image {
            self.finish_page()?;
        }
        Ok(Some(len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, value: &[u8]) -> Vec<u8> {
        Packet::new(0).add(0x22, name, 0x05, value).finish()[8..].to_vec()
    }

    fn packet(items: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = items.concat();
        let mut p = vec![2, 0, 2, 2, 0, 0];
        p.extend_from_slice(&(body.len() as u16).to_be_bytes());
        p.extend_from_slice(&body);
        p
    }

    #[test]
    fn discovery_packet_bytes() {
        let p = discovery_packet();
        assert_eq!(&p[..8], [2, 0, 1, 2, 0, 0, 0, 29]);
        assert_eq!(p[8], 0x25);
        assert_eq!(&p[9..11], [0, 22]);
        assert_eq!(&p[11..33], b"std-scan-discovery-all");
        assert_eq!(&p[33..], [0x02, 0, 1, 0]);
    }

    #[test]
    fn find_response_gives_address_and_model() {
        let p = packet(&[
            item("std-scan-discovery-ip", &[10, 0, 0, 7]),
            item("std-scan-discovery-model-name", b"Dell 1600n"),
        ]);
        assert_eq!(parse_find_response(&p), Some(("10.0.0.7".to_owned(), "1600n".to_owned())));
        assert_eq!(parse_find_response(&packet(&[item("other", &[1])])), None);
        assert_eq!(parse_find_response(&p[..p.len() - 1]), None, "incomplete packet");
    }

    #[test]
    fn truncated_items_end_parsing_instead_of_overrunning() {
        let mut body = item("std-scan-discovery-ip", &[1, 2, 3, 4]);
        body.truncate(body.len() - 2);
        assert_eq!(parse_find_response(&packet(&[body])), None);
    }

    #[test]
    fn session_answers_and_decodes_a_g4_page() {
        let mut s = Session::new();
        let open = packet(&[item("std-scan-session-open", &[])]);
        assert_eq!(s.process(&open).unwrap(), Some(open.len()));
        let replies = s.take_replies();
        assert!(replies[0].windows(30).any(|w| w == b"std-scan-session-open-response"));

        let prefs = packet(&[item("std-scan-getclientpref", &[])]);
        s.process(&prefs).unwrap();
        let reply = &s.take_replies()[0];
        let res = b"std-scan-getclientpref-xresolution";
        let at = reply.windows(res.len()).position(|w| w == res).unwrap();
        assert_eq!(&reply[at + res.len()..at + res.len() + 5], [0x04, 0, 2, 0, 200]);

        s.process(&packet(&[item("std-scan-page-start", &[])])).unwrap();
        s.process(&packet(&[
            item("std-scan-page-widthpixel", &8u32.to_be_bytes()),
            item("std-scan-page-heightpixel", &2u32.to_be_bytes()),
        ]))
        .unwrap();
        // Two all-white G4 lines are two "V0" codes, each the single bit 1.
        let mut data = item("std-scan-scandata-error", &[]);
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0b1100_0000]);
        s.process(&packet(&[data])).unwrap();
        s.process(&packet(&[item("std-scan-page-end", &[])])).unwrap();
        s.process(&packet(&[item("std-scan-session-end", &[])])).unwrap();
        assert!(s.ended());
        let pages = s.into_pages();
        assert_eq!((pages[0].width, pages[0].height), (8, 2));
        assert_eq!(pages[0].rgb, vec![255; 8 * 2 * 3]);
    }

    #[test]
    fn partial_packets_wait_for_more_data() {
        let mut s = Session::new();
        let p = packet(&[item("std-scan-session-open", &[])]);
        assert_eq!(s.process(&p[..p.len() - 1]).unwrap(), None);
    }

    #[test]
    fn unknown_compression_is_an_error() {
        let mut s = Session::new();
        s.compression = 0x99;
        s.page_data = vec![1];
        assert!(s.finish_page().is_err());
    }
}

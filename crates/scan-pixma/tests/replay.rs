//! Golden tests: replay USB sessions recorded from C SANE driving a `LiDE`
//! 400 and check that this driver issues exactly the recorded transfers and
//! produces exactly the image C SANE produced.
//!
//! Recordings live in `tests/recordings/pixma-lide400` at the workspace
//! root; see the README there for how each was made.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use scan_core::{Area, CancelToken, Device, OptionValue, ScanSettings};
use scan_pixma::Pixma;
use scan_usb::{DeviceIds, Replay, UsbTransport};

fn recordings() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/recordings/pixma-lide400")
}

/// A replay the test can still inspect after handing it to the driver.
#[derive(Debug, Clone)]
struct Shared(Arc<Mutex<Replay>>, DeviceIds);

impl Shared {
    fn open(name: &str) -> Self {
        let replay = Replay::open(recordings().join(format!("{name}.xml"))).expect("recording loads");
        let ids = replay.ids().clone();
        Self(Arc::new(Mutex::new(replay)), ids)
    }

    fn lock(&self) -> MutexGuard<'_, Replay> {
        self.0.lock().unwrap()
    }
}

impl UsbTransport for Shared {
    fn ids(&self) -> &DeviceIds {
        &self.1
    }
    fn bulk_out(&mut self, data: &[u8], t: Duration) -> scan_usb::Result<usize> {
        self.lock().bulk_out(data, t)
    }
    fn bulk_in(&mut self, buf: &mut [u8], t: Duration) -> scan_usb::Result<usize> {
        self.lock().bulk_in(buf, t)
    }
    fn interrupt_in(&mut self, buf: &mut [u8], t: Duration) -> scan_usb::Result<usize> {
        self.lock().interrupt_in(buf, t)
    }
    fn control_in(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        t: Duration,
    ) -> scan_usb::Result<usize> {
        self.lock().control_in(request_type, request, value, index, buf, t)
    }
    fn control_out(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        t: Duration,
    ) -> scan_usb::Result<usize> {
        self.lock().control_out(request_type, request, value, index, data, t)
    }
    fn sleep(&mut self, _: Duration) {}
}

/// The pixel data of a PNM file written by `scanimage`, with 16-bit
/// samples converted from PNM's big-endian to host order.
fn pnm_pixels(path: &Path) -> (Vec<u8>, usize, usize) {
    let data = std::fs::read(path).unwrap();
    let mut fields = Vec::new();
    let mut i = 0;
    let wanted = if data.starts_with(b"P4") { 3 } else { 4 };
    while fields.len() < wanted {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        if data[i] == b'#' {
            while data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).into_owned());
    }
    let mut pixels = data[i + 1..].to_vec();
    if fields.get(3).is_some_and(|max| max == "65535") {
        for px in pixels.as_chunks_mut::<2>().0 {
            px.swap(0, 1);
        }
    }
    (pixels, fields[1].parse().unwrap(), fields[2].parse().unwrap())
}

struct Case {
    name: &'static str,
    mode: &'static str,
    dpi: u32,
    /// x, y, width, height in mm; `None` for the full bed.
    area: Option<(f64, f64, f64, f64)>,
    gamma: Option<f64>,
    calibrate: Option<&'static str>,
}

const fn case(name: &'static str, mode: &'static str, dpi: u32, area: (f64, f64, f64, f64)) -> Case {
    Case { name, mode, dpi, area: Some(area), gamma: None, calibrate: None }
}

fn settings(dev: &Pixma, c: &Case) -> ScanSettings {
    let bed = &dev.capabilities().sources[0];
    let (x, y, width, height) = c.area.unwrap_or((0.0, 0.0, bed.width_mm, bed.height_mm));
    ScanSettings {
        source: "Flatbed".into(),
        mode: c.mode.into(),
        resolution: c.dpi,
        area: Area { x, y, width, height },
        gamma: c.gamma,
        extras: c.calibrate.map(|v| ("calibrate".to_owned(), OptionValue::Choice(v.into()))).into_iter().collect(),
    }
}

/// Reads a whole frame.
fn read_frame(dev: &mut Pixma) -> Vec<u8> {
    let mut image = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = dev.read(&mut buf).unwrap_or_else(|e| panic!("read failed: {e}"));
        if n == 0 {
            break;
        }
        image.extend_from_slice(&buf[..n]);
    }
    dev.finish();
    image
}

fn check(c: &Case) {
    let usb = Shared::open(c.name);
    let mut dev = Pixma::open(Box::new(usb.clone())).unwrap();
    assert_eq!(dev.info().name, "pixma:04A91912_508624");
    let s = settings(&dev, c);
    let params = dev.start(&s, &CancelToken::new()).unwrap_or_else(|e| panic!("{}: start failed: {e}", c.name));
    let image = read_frame(&mut dev);
    drop(dev);

    let (expected, width, height) = pnm_pixels(&recordings().join(format!("{}.pnm", c.name)));
    assert_eq!((params.pixels_per_line, params.lines), (width, height), "{}: frame size", c.name);
    assert_eq!(params.bytes_per_line * params.lines, expected.len(), "{}: parameters", c.name);
    assert_eq!(image.len(), expected.len(), "{}: image length", c.name);
    if let Some(at) = image.iter().zip(&expected).position(|(a, b)| a != b) {
        panic!("{}: image differs from C SANE's at byte {at}", c.name);
    }
    assert_eq!(usb.lock().remaining(), 0, "{}: recorded transfers left over", c.name);
}

#[test]
fn gray_150() {
    check(&case("gray-150", "Gray", 150, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn lineart_300() {
    check(&case("lineart-300", "Lineart", 300, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn color_300() {
    check(&case("color-300", "Color", 300, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn gray_300() {
    check(&case("gray-300", "Gray", 300, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn color48_300() {
    check(&case("color48-300", "48 bits color", 300, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn gray16_300() {
    check(&case("gray16-300", "16 bits gray", 300, (0.0, 0.0, 50.0, 50.0)));
}

#[test]
fn color_300_offset() {
    check(&case("color-300-offset", "Color", 300, (100.0, 150.0, 30.0, 30.0)));
}

#[test]
fn color_600() {
    check(&case("color-600", "Color", 600, (60.0, 60.0, 25.0, 25.0)));
}

#[test]
fn color_1200() {
    check(&case("color-1200", "Color", 1200, (60.0, 60.0, 12.5, 12.5)));
}

#[test]
fn color_2400() {
    check(&case("color-2400", "Color", 2400, (60.0, 60.0, 6.25, 6.25)));
}

#[test]
fn color_4800() {
    check(&case("color-4800", "Color", 4800, (60.0, 60.0, 3.125, 3.125)));
}

#[test]
fn color48_4800() {
    check(&case("color48-4800", "48 bits color", 4800, (60.0, 60.0, 3.125, 3.125)));
}

#[test]
fn gray_4800() {
    check(&case("gray-4800", "Gray", 4800, (60.0, 60.0, 3.125, 3.125)));
}

#[test]
fn color_150_gamma1() {
    check(&Case { gamma: Some(1.0), ..case("color-150-gamma1", "Color", 150, (0.0, 0.0, 50.0, 50.0)) });
}

#[test]
fn color_150_calibrate_always() {
    check(&Case {
        calibrate: Some("Always"),
        ..case("color-150-calibrate-always", "Color", 150, (0.0, 0.0, 50.0, 50.0))
    });
}

#[test]
fn color_150_calibrate_never() {
    check(&Case {
        calibrate: Some("Never"),
        ..case("color-150-calibrate-never", "Color", 150, (0.0, 0.0, 50.0, 50.0))
    });
}

#[test]
fn color_75_full_page() {
    check(&Case { area: None, ..case("color-75-fullpage", "Color", 75, (0.0, 0.0, 0.0, 0.0)) });
}

/// The recording was stopped by SIGINT after 8 s. Cancelling at the point
/// where C SANE did must end the session with the same transfers.
#[test]
fn cancelled_scan() {
    let usb = Shared::open("cancel-color-300-fullpage");
    let mut dev = Pixma::open(Box::new(usb.clone())).unwrap();
    let c = Case { area: None, ..case("cancel", "Color", 300, (0.0, 0.0, 0.0, 0.0)) };
    let cancel = CancelToken::new();
    dev.start(&settings(&dev, &c), &cancel).unwrap();
    let mut buf = vec![0u8; 1 << 12];
    let err = loop {
        if usb.lock().next_write_starts_with(&[0xef, 0x20]) {
            cancel.cancel();
        }
        match dev.read(&mut buf) {
            Ok(0) => panic!("scan ended without being cancelled"),
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    assert!(err.is_cancelled(), "{err}");
    dev.finish();
    assert_eq!(usb.lock().remaining(), 0, "recorded transfers left over");
}

/// A second scan straight after the first is refused by the device
/// (`PCScanning`). C SANE gives up there; this driver retries `StartJob`,
/// which runs past the end of the recording. Page 1 must match, and the
/// driver must have sent exactly what C SANE did up to the refusal.
#[test]
fn second_scan_is_refused_like_in_sane() {
    let usb = Shared::open("batch2-gray-150-page2-fails");
    let mut dev = Pixma::open(Box::new(usb.clone())).unwrap();
    let c = case("batch2-gray-150-page2-fails", "Gray", 150, (0.0, 0.0, 50.0, 50.0));
    let s = settings(&dev, &c);
    dev.start(&s, &CancelToken::new()).unwrap();
    let page1 = read_frame(&mut dev);
    let (expected, ..) = pnm_pixels(&recordings().join("batch2-gray-150-page2-fails-1.pnm"));
    assert!(page1 == expected, "page 1 differs from C SANE's");
    let err = dev.start(&s, &CancelToken::new()).expect_err("the device refuses page 2");
    assert!(err.to_string().contains("recording ended, driver issued bulk write"), "retries StartJob: {err}");
    assert_eq!(usb.lock().remaining(), 0, "recorded transfers left over");
}

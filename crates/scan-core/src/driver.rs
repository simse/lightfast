//! Helpers shared by backends: turning [`ScanSettings`] into pixels, common
//! mode descriptions, and buffering decoded image bytes for [`Device::read`].
//!
//! [`Device::read`]: crate::Device::read

use std::collections::VecDeque;

use crate::{
    Area, CancelToken, Capabilities, Channels, Device, Error, ErrorKind, MM_PER_INCH, ModeCaps, Parameters,
    PixelLayout, Result, ScanSettings, SourceCaps,
};

/// An [`ErrorKind::Invalid`] error.
pub fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Invalid, message)
}

/// An [`ErrorKind::Protocol`] error.
pub fn protocol(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Protocol, message)
}

/// An [`ErrorKind::Device`] error.
pub fn device_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Device, message)
}

/// Millimetres to pixels at `dpi`, rounded down like SANE's `SANE_UNFIX` maths.
#[must_use]
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "non-negative and bounded by the bed")]
pub fn mm_to_px(mm: f64, dpi: u32) -> u32 {
    (mm.max(0.0) * f64::from(dpi) / MM_PER_INCH) as u32
}

/// A [`ModeCaps`] for `name`.
#[must_use]
pub fn mode(name: &str, channels: Channels, depth: u32, resolutions: &[u32]) -> ModeCaps {
    ModeCaps { name: name.into(), layout: PixelLayout { channels, depth }, resolutions: resolutions.to_vec() }
}

/// A network backend. USB backends are matched by ID; these find devices by
/// asking the network, and open them by the address in their name.
#[derive(Debug, Clone, Copy)]
pub struct NetDriver {
    /// The backend's name, used as the device name prefix.
    pub name: &'static str,
    /// Looks for devices on the local network. Never fails: a network problem
    /// just means nothing was found.
    pub discover: fn() -> Vec<crate::DeviceInfo>,
    /// Opens the device at the part of its name after `<name>:`.
    pub open: fn(&str) -> Result<Box<dyn Device>>,
}

/// `settings` checked against `caps`, with its area in pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    pub source: &'a SourceCaps,
    pub mode: &'a ModeCaps,
    pub dpi: u32,
    /// Left edge in pixels at `dpi`.
    pub x: u32,
    /// Top edge in pixels at `dpi`.
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Checks `settings` against `caps` and converts its area to pixels, clamping
/// the area to the source's bed.
///
/// # Errors
/// [`ErrorKind::Invalid`] for an unknown source or mode, an unsupported
/// resolution, or an empty area.
pub fn resolve<'a>(caps: &'a Capabilities, settings: &ScanSettings) -> Result<Request<'a>> {
    let source = caps.source(&settings.source).ok_or_else(|| invalid(format!("no source {}", settings.source)))?;
    let mode = source.mode(&settings.mode).ok_or_else(|| invalid(format!("no mode {}", settings.mode)))?;
    let dpi = settings.resolution;
    if !mode.resolutions.contains(&dpi) {
        return Err(invalid(format!("{dpi} dpi is not supported in {} mode", mode.name)));
    }
    let area = &settings.area;
    let right = (area.x + area.width).min(source.width_mm);
    let bottom = (area.y + area.height).min(source.height_mm);
    if area.x < 0.0 || area.y < 0.0 || right <= area.x || bottom <= area.y {
        return Err(invalid("the scan area is empty"));
    }
    let x = mm_to_px(area.x, dpi);
    let y = mm_to_px(area.y, dpi);
    let width = mm_to_px(right, dpi) - x;
    let height = mm_to_px(bottom, dpi) - y;
    if width == 0 || height == 0 {
        return Err(invalid("the scan area is empty"));
    }
    Ok(Request { source, mode, dpi, x, y, width, height })
}

/// Settings for the whole of `caps`' first source in `mode` at `dpi`.
///
/// # Panics
/// If the device reports no sources, which [`Capabilities`] rules out.
#[must_use]
pub fn full_area_settings(caps: &Capabilities, mode: &str, dpi: u32) -> ScanSettings {
    let source = &caps.sources[0];
    ScanSettings {
        source: source.name.clone(),
        mode: mode.into(),
        resolution: dpi,
        area: Area { x: 0.0, y: 0.0, width: source.width_mm, height: source.height_mm },
        gamma: None,
        extras: vec![],
    }
}

/// Starts a frame and reads it to the end, for tests and simple frontends.
///
/// # Errors
/// Any error from the device.
pub fn scan_frame(device: &mut dyn Device, settings: &ScanSettings) -> Result<(Parameters, Vec<u8>)> {
    let params = device.start(settings, &CancelToken::new())?;
    let mut image = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match device.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => image.extend_from_slice(&buf[..n]),
            Err(e) => {
                device.finish();
                return Err(e);
            }
        }
    }
    device.finish();
    Ok((params, image))
}

/// Decoded image bytes waiting for [`Device::read`](crate::Device::read).
#[derive(Debug, Default)]
pub struct ByteQueue(VecDeque<u8>);

impl ByteQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Moves queued bytes into `buf`; returns how many.
    pub fn pop_into(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.0.len());
        for (slot, byte) in buf.iter_mut().zip(self.0.drain(..n)) {
            *slot = byte;
        }
        n
    }
}

/// Packs 8-bit gray samples into lineart bits (1 = black, MSB first).
/// A sample at or below `threshold` is black.
#[must_use]
pub fn gray_to_lineart(gray: &[u8], threshold: u8) -> Vec<u8> {
    let mut out = vec![0u8; gray.len().div_ceil(8)];
    for (i, &g) in gray.iter().enumerate() {
        if g <= threshold {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;

    fn caps() -> Capabilities {
        Capabilities {
            sources: vec![SourceCaps {
                name: "Flatbed".into(),
                kind: SourceKind::Flatbed,
                width_mm: 215.9,
                height_mm: 297.0,
                modes: vec![mode("Color", Channels::Rgb, 8, &[75, 300])],
                gamma: None,
                extras: vec![],
            }],
        }
    }

    fn settings(area: Area, resolution: u32) -> ScanSettings {
        ScanSettings { source: "Flatbed".into(), mode: "Color".into(), resolution, area, gamma: None, extras: vec![] }
    }

    #[test]
    fn resolves_and_clamps_the_area() {
        let caps = caps();
        let r = resolve(&caps, &settings(Area { x: 25.4, y: 0.0, width: 1000.0, height: 25.4 }, 300)).unwrap();
        assert_eq!((r.x, r.y, r.height), (300, 0, 300));
        assert_eq!(r.width, 2550 - 300);
    }

    #[test]
    fn rejects_bad_settings() {
        let caps = caps();
        let area = Area { x: 0.0, y: 0.0, width: 10.0, height: 10.0 };
        assert!(resolve(&caps, &settings(area, 600)).is_err());
        assert!(resolve(&caps, &settings(Area { width: 0.0, ..area }, 300)).is_err());
    }

    #[test]
    fn queue_round_trips() {
        let mut q = ByteQueue::new();
        q.push(&[1, 2, 3]);
        let mut b = [0; 2];
        assert_eq!(q.pop_into(&mut b), 2);
        assert_eq!(b, [1, 2]);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn lineart_is_msb_first_and_one_is_black() {
        assert_eq!(gray_to_lineart(&[0, 255, 0, 255, 255, 255, 255, 255, 0], 127), vec![0b1010_0000, 0b1000_0000]);
    }
}

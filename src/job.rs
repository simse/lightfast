//! Scan job description, size estimation and preflight checks.
//!
//! Everything here is pure computation on millimetres and pixel counts; it
//! never touches the device, so the UI can call it on every frame to show
//! live estimates.

use std::fmt;
use std::path::PathBuf;

use crate::raster::PixelFormat;

/// Millimetres per inch, exact by definition.
const MM_PER_INCH: f64 = 25.4;

/// Raw size above which TIFF output switches to `BigTIFF`.
///
/// Classic TIFF uses 32-bit offsets (4 GiB max); we leave headroom for the
/// header, IFD and strip tables.
pub const BIGTIFF_THRESHOLD: u64 = 3_500_000_000;

/// A rectangle on the scanner bed, in millimetres from the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Area {
    /// Creates the area with top-left corner (`x`, `y`) and size `w` × `h`.
    #[must_use]
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Returns this area moved and shrunk so it lies within a `bed_w` × `bed_h` bed.
    ///
    /// The result is at least 1 mm on each side.
    #[must_use]
    pub fn clamped(self, bed_w: f64, bed_h: f64) -> Self {
        let w = self.w.clamp(1.0, bed_w);
        let h = self.h.clamp(1.0, bed_h);
        Self { x: self.x.clamp(0.0, bed_w - w), y: self.y.clamp(0.0, bed_h - h), w, h }
    }

    /// The smallest area containing both `self` and `other`.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let (x0, y0) = (self.x.min(other.x), self.y.min(other.y));
        let (x1, y1) = ((self.x + self.w).max(other.x + other.w), (self.y + self.h).max(other.y + other.h));
        Self { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// Whether the point (`x`, `y`) lies within this area, edges included.
    #[must_use]
    pub fn contains(self, x: f64, y: f64) -> bool {
        (self.x..=self.x + self.w).contains(&x) && (self.y..=self.y + self.h).contains(&y)
    }
}

/// Output file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Tiff,
}

impl Format {
    /// Every format, in the order the UI lists them.
    pub const ALL: [Format; 2] = [Format::Png, Format::Tiff];

    /// File name extension, without the dot.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Tiff => "tif",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Png => "PNG",
            Format::Tiff => "TIFF",
        })
    }
}

/// One way the scanner can digitise: a backend scan mode and the pixel
/// format it produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanMode {
    /// Name shown to the user, unique among a source's modes.
    pub label: String,
    /// The backend's name for the mode, e.g. `"48 bits color"`.
    pub backend_mode: String,
    pub pixel: PixelFormat,
}

impl fmt::Display for ScanMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// One file cut from a scan: an area of the bed and where to write it.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub area: Area,
    /// Final output path; the file is written to a `.part` sibling first.
    pub dest: PathBuf,
}

/// Everything needed to perform one full-resolution scan.
///
/// The scanner reads the bounding box of all [`Output`] areas in a single
/// pass and writes each area to its own file.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The backend's source name; `None` for the scanner's default source.
    pub source: Option<String>,
    pub mode: ScanMode,
    pub dpi: u32,
    /// Gamma for the scan; `None` leaves the backend default.
    pub gamma: Option<f64>,
    pub format: Format,
    /// The files to write; never empty.
    pub outputs: Vec<Output>,
}

impl Job {
    /// The area the scanner reads: the bounding box of every output.
    ///
    /// # Panics
    /// If the job has no outputs.
    #[must_use]
    pub fn scan_area(&self) -> Area {
        let mut areas = self.outputs.iter().map(|o| o.area);
        let first = areas.next().expect("a scan job needs at least one output");
        areas.fold(first, Area::union)
    }
}

/// Total uncompressed size of `areas` scanned at `dpi`, saturating.
#[must_use]
pub fn total_raw_bytes(areas: &[Area], dpi: u32, pixel: PixelFormat) -> u64 {
    areas.iter().fold(0, |sum, &a| sum.saturating_add(Estimate::new(a, dpi, pixel).raw_bytes()))
}

/// Converts a length in millimetres to pixels at `dpi`.
///
/// Negative and NaN lengths give 0; lengths beyond `u64` saturate.
#[must_use]
pub fn mm_to_px(mm: f64, dpi: u32) -> u64 {
    let px = (mm / MM_PER_INCH * f64::from(dpi)).round();
    num_traits::ToPrimitive::to_u64(&px).unwrap_or(if px > 0.0 { u64::MAX } else { 0 })
}

/// Expected output dimensions, pixel format and raw (uncompressed) size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub width: u64,
    pub height: u64,
    pub pixel: PixelFormat,
}

impl Estimate {
    /// Estimates the output of scanning `area` at `dpi` in `pixel` format.
    #[must_use]
    pub fn new(area: Area, dpi: u32, pixel: PixelFormat) -> Self {
        Self { width: mm_to_px(area.w, dpi), height: mm_to_px(area.h, dpi), pixel }
    }

    /// Uncompressed size in bytes, saturating at `u64::MAX`.
    #[must_use]
    pub fn raw_bytes(self) -> u64 {
        let row = self.width.saturating_mul(u64::from(self.pixel.bits_per_pixel())).div_ceil(8);
        row.saturating_mul(self.height)
    }
}

impl fmt::Display for Estimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} × {} px · ~{} raw", self.width, self.height, human_bytes(self.raw_bytes()))
    }
}

/// Formats a byte count with a decimal unit and one decimal, e.g. `6.9 GB`.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut scale = 1000_u128;
    let mut unit = 0;
    while u128::from(bytes) >= scale * 1000 && unit < UNITS.len() - 1 {
        scale *= 1000;
        unit += 1;
    }
    // Round to the nearest tenth in integer maths; `u128` can't overflow.
    let tenths = (u128::from(bytes) * 10 + scale / 2) / scale;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_bed_at_4800_matches_scanner_geometry() {
        // LiDE 400 bed as reported by the pixma backend.
        let e = Estimate::new(Area::new(0.0, 0.0, 216.069, 297.011), 4800, PixelFormat::Rgb8);
        assert_eq!((e.width, e.height), (40_832, 56_128));
        assert!(e.raw_bytes() > 6_800_000_000);
    }

    #[test]
    fn raw_bytes_follow_the_pixel_format() {
        let size = |pixel| Estimate { width: 1001, height: 10, pixel }.raw_bytes();
        assert_eq!(size(PixelFormat::Lineart), 126 * 10, "rows are padded to whole bytes");
        assert_eq!(size(PixelFormat::Gray8), 1001 * 10);
        assert_eq!(size(PixelFormat::Rgb8), 3 * size(PixelFormat::Gray8));
        assert_eq!(size(PixelFormat::Rgb16), 2 * size(PixelFormat::Rgb8));
        assert_eq!(size(PixelFormat::Gray16), 2 * size(PixelFormat::Gray8));
        let huge = Estimate { width: u64::MAX, height: u64::MAX, pixel: PixelFormat::Rgb16 };
        assert_eq!(huge.raw_bytes(), u64::MAX);
    }

    #[test]
    fn clamp_keeps_area_on_bed() {
        let a = Area::new(200.0, -5.0, 50.0, 400.0).clamped(216.0, 297.0);
        assert_eq!(a, Area::new(166.0, 0.0, 50.0, 297.0));
    }

    #[test]
    fn union_covers_both_areas() {
        let u = Area::new(10.0, 20.0, 5.0, 5.0).union(Area::new(0.0, 30.0, 4.0, 10.0));
        assert_eq!(u, Area::new(0.0, 20.0, 15.0, 20.0));
    }

    #[test]
    fn total_raw_bytes_sums_frames() {
        let a = Area::new(0.0, 0.0, 25.4, 25.4);
        let one = total_raw_bytes(&[a], 100, PixelFormat::Gray8);
        assert_eq!(one, 100 * 100);
        assert_eq!(total_raw_bytes(&[a, a, a], 100, PixelFormat::Gray8), 3 * one);
    }

    #[test]
    fn mm_to_px_handles_degenerate_lengths() {
        assert_eq!(mm_to_px(-3.0, 300), 0);
        assert_eq!(mm_to_px(f64::NAN, 300), 0);
        assert_eq!(mm_to_px(f64::INFINITY, 300), u64::MAX);
        assert_eq!(mm_to_px(25.4, 300), 300);
    }

    #[test]
    fn human_bytes_picks_unit() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(6_875_000_000), "6.9 GB");
        assert_eq!(human_bytes(1000), "1.0 KB");
        assert_eq!(human_bytes(999_950), "1000.0 KB");
        assert_eq!(human_bytes(u64::MAX), "18446744.1 TB");
    }
}

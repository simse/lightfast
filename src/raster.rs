//! Row-level image plumbing: pixel formats, reassembling scanner byte streams
//! into packed rows, and building small RGB8 views of those rows.
//!
//! Rows are "packed" throughout: no padding between rows, so a row is
//! exactly [`PixelFormat::row_bytes`] long. Samples keep the layout scanner
//! backends deliver them in (SANE's): 16-bit samples in host byte order, and 1-bit lineart
//! packed eight pixels per byte, most significant bit first, with 1 meaning
//! black. Encoders convert to their file format's layout; display code
//! converts to [`RgbImage`] through [`to_rgb8`] or [`Thumbnail`].

use std::fmt;

use anyhow::{Result, ensure};

/// Brightness of the not-yet-scanned area in live thumbnails: a mid-dark
/// gray that reads as "empty" against the dark theme.
const THUMB_UNSCANNED: u8 = 0x60;

/// How the pixels of a frame are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 1 bit per pixel, 1 = black, packed MSB first.
    Lineart,
    Gray8,
    /// One host-endian `u16` per pixel.
    Gray16,
    Rgb8,
    /// Three host-endian `u16`s per pixel.
    Rgb16,
}

impl PixelFormat {
    /// Every format, from smallest to largest pixels.
    pub const ALL: [PixelFormat; 5] =
        [PixelFormat::Lineart, PixelFormat::Gray8, PixelFormat::Gray16, PixelFormat::Rgb8, PixelFormat::Rgb16];

    /// The format of a frame with `color` (RGB) or gray samples of `bits`
    /// each, or `None` if this crate can't handle it.
    #[must_use]
    pub fn new(color: bool, bits: u32) -> Option<Self> {
        match (color, bits) {
            (false, 1) => Some(Self::Lineart),
            (false, 8) => Some(Self::Gray8),
            (false, 16) => Some(Self::Gray16),
            (true, 8) => Some(Self::Rgb8),
            (true, 16) => Some(Self::Rgb16),
            _ => None,
        }
    }

    /// Samples per pixel: 1 for gray and lineart, 3 for colour.
    #[must_use]
    pub fn channels(self) -> usize {
        if self.is_color() { 3 } else { 1 }
    }

    /// Whether pixels have red, green and blue samples.
    #[must_use]
    pub fn is_color(self) -> bool {
        matches!(self, Self::Rgb8 | Self::Rgb16)
    }

    /// Bits per sample.
    #[must_use]
    pub fn depth(self) -> u32 {
        match self {
            Self::Lineart => 1,
            Self::Gray8 | Self::Rgb8 => 8,
            Self::Gray16 | Self::Rgb16 => 16,
        }
    }

    /// Bits per pixel, across all channels.
    #[must_use]
    pub fn bits_per_pixel(self) -> u32 {
        self.depth() * if self.is_color() { 3 } else { 1 }
    }

    /// Bytes in one packed row of `width` pixels, or `None` on overflow.
    #[must_use]
    pub fn row_bytes(self, width: usize) -> Option<usize> {
        Some(width.checked_mul(self.bits_per_pixel() as usize)?.div_ceil(8))
    }

    /// The byte that, repeated, fills a row with white.
    #[must_use]
    pub fn white_byte(self) -> u8 {
        if self == Self::Lineart { 0x00 } else { 0xFF }
    }

    /// Short name for the UI, e.g. "Gray 16-bit".
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lineart => "Black & white",
            Self::Gray8 => "Gray",
            Self::Gray16 => "Gray 16-bit",
            Self::Rgb8 => "Color",
            Self::Rgb16 => "Color 16-bit",
        }
    }

    /// Appends pixels `x0..x0 + width` of the packed `row` to `out` as a packed row.
    ///
    /// Lineart is re-aligned to start at the first bit when `x0` isn't a
    /// multiple of 8, and the unused bits of its last byte are cleared.
    ///
    /// # Panics
    /// If the pixels lie outside `row`.
    pub fn crop_row(self, row: &[u8], x0: usize, width: usize, out: &mut Vec<u8>) {
        if self != Self::Lineart {
            let bytes = self.bits_per_pixel() as usize / 8;
            out.extend_from_slice(&row[x0 * bytes..(x0 + width) * bytes]);
            return;
        }
        let (first, shift) = (x0 / 8, x0 % 8);
        let len = width.div_ceil(8);
        let start = out.len();
        if shift == 0 {
            out.extend_from_slice(&row[first..first + len]);
        } else {
            out.extend((first..first + len).map(|i| {
                let next = row.get(i + 1).copied().unwrap_or(0);
                row[i] << shift | next >> (8 - shift)
            }));
        }
        let spare = len * 8 - width;
        if spare > 0
            && let Some(last) = out.get_mut(start + len - 1)
        {
            *last &= 0xFF << spare;
        }
    }

    /// Pixel `x` of the packed `row`, reduced to RGB8 for display.
    ///
    /// 16-bit samples keep their high byte; lineart becomes black or white.
    fn rgb8_at(self, row: &[u8], x: usize) -> [u8; 3] {
        let hi = |i: usize| (u16::from_ne_bytes([row[i], row[i + 1]]) >> 8).to_le_bytes()[0];
        match self {
            Self::Lineart => {
                let black = row[x / 8] >> (7 - x % 8) & 1 == 1;
                [if black { 0 } else { 0xFF }; 3]
            }
            Self::Gray8 => [row[x]; 3],
            Self::Gray16 => [hi(x * 2); 3],
            Self::Rgb8 => [row[x * 3], row[x * 3 + 1], row[x * 3 + 2]],
            Self::Rgb16 => [hi(x * 6), hi(x * 6 + 2), hi(x * 6 + 4)],
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Appends packed `rows` of `width` pixels in `pixel` format to `out` as RGB8.
///
/// A trailing partial row is ignored.
pub fn to_rgb8(pixel: PixelFormat, width: usize, rows: &[u8], out: &mut Vec<u8>) {
    let Some(stride) = pixel.row_bytes(width).filter(|&s| s > 0) else { return };
    if pixel == PixelFormat::Rgb8 {
        out.extend_from_slice(&rows[..rows.len() / stride * stride]);
        return;
    }
    out.reserve(rows.len() / stride * width * 3);
    for row in rows.chunks_exact(stride) {
        out.extend((0..width).flat_map(|x| pixel.rgb8_at(row, x)));
    }
}

/// An owned packed RGB8 image.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes, row-major.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Bytes a `width` × `height` image occupies, or `None` on overflow.
    #[must_use]
    pub fn byte_len(&self) -> Option<usize> {
        self.width.checked_mul(self.height)?.checked_mul(3)
    }
}

impl fmt::Debug for RgbImage {
    /// Summarises the image instead of dumping its (possibly huge) pixels.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RgbImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .finish_non_exhaustive()
    }
}

/// Splits an arbitrary-sized scanner byte stream into packed rows.
///
/// Device `read` boundaries don't align with lines, and lines may carry
/// padding beyond the pixel data. Samples are passed through unchanged.
#[derive(Debug)]
pub struct RowAssembler {
    in_line: usize,
    out_line: usize,
    partial: Vec<u8>,
}

impl RowAssembler {
    /// Creates an assembler for lines of `pixels` pixels in `pixel` format,
    /// each sent as `bytes_per_line` bytes (the padded input stride).
    ///
    /// # Errors
    /// Fails if a line is empty or too short to hold `pixels` pixels; both
    /// values come from the scanner backend.
    pub fn new(bytes_per_line: usize, pixels: usize, pixel: PixelFormat) -> Result<Self> {
        let used = pixel.row_bytes(pixels);
        ensure!(
            pixels > 0 && used.is_some_and(|used| bytes_per_line >= used),
            "scanner sent {bytes_per_line}-byte lines for {pixels} {pixel} pixels",
        );
        let out_line = used.unwrap_or_default();
        Ok(Self { in_line: bytes_per_line, out_line, partial: Vec::with_capacity(bytes_per_line) })
    }

    /// Length of one output (packed) row.
    #[must_use]
    pub fn out_row_bytes(&self) -> usize {
        self.out_line
    }

    /// Appends every row completed by `data` to `out`; returns the row count.
    pub fn push(&mut self, mut data: &[u8], out: &mut Vec<u8>) -> usize {
        let mut rows = 0;
        while !data.is_empty() {
            let need = self.in_line - self.partial.len();
            if self.partial.is_empty() && data.len() >= self.in_line {
                // Fast path: a whole line is available without copying to `partial`.
                out.extend_from_slice(&data[..self.out_line]);
                data = &data[self.in_line..];
            } else {
                let take = need.min(data.len());
                self.partial.extend_from_slice(&data[..take]);
                data = &data[take..];
                if self.partial.len() < self.in_line {
                    break;
                }
                out.extend_from_slice(&self.partial[..self.out_line]);
                self.partial.clear();
            }
            rows += 1;
        }
        rows
    }
}

/// Builds a downsampled RGB8 copy of an image while its rows stream past.
///
/// Uses nearest-neighbour sampling every `step` pixels; memory is bounded by
/// the thumbnail size regardless of the source size.
#[derive(Debug)]
pub struct Thumbnail {
    step: usize,
    pixel: PixelFormat,
    stride: usize,
    next_row: usize,
    image: RgbImage,
}

impl Thumbnail {
    /// Creates a thumbnail of a `src_width` × `src_height` image whose rows
    /// arrive in `pixel` format, no wider than roughly `max_width` pixels.
    #[must_use]
    pub fn new(src_width: usize, src_height: usize, max_width: usize, pixel: PixelFormat) -> Self {
        let step = src_width.div_ceil(max_width.max(1)).max(1);
        let width = src_width.div_ceil(step).max(1);
        let height = src_height.div_ceil(step).max(1);
        Self {
            step,
            pixel,
            stride: pixel.row_bytes(src_width).unwrap_or(usize::MAX).max(1),
            next_row: 0,
            image: RgbImage { width, height, data: vec![THUMB_UNSCANNED; width * height * 3] },
        }
    }

    /// Feeds packed rows (in source order).
    pub fn push_rows(&mut self, rows: &[u8]) {
        for row in rows.chunks_exact(self.stride) {
            let y = self.next_row;
            self.next_row += 1;
            if !y.is_multiple_of(self.step) {
                continue;
            }
            let ty = y / self.step;
            if ty >= self.image.height {
                continue;
            }
            let dst = &mut self.image.data[ty * self.image.width * 3..][..self.image.width * 3];
            // `tx * step` stays below the source width by construction.
            for (tx, px) in dst.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                *px = self.pixel.rgb8_at(row, tx * self.step);
            }
        }
    }

    /// The thumbnail so far; unscanned rows are gray.
    #[must_use]
    pub fn image(&self) -> &RgbImage {
        &self.image
    }

    /// Consumes the thumbnail, returning its image.
    #[must_use]
    pub fn into_image(self) -> RgbImage {
        self.image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Maps an index to a test byte value; 251 is prime so patterns don't
    /// line up with row strides.
    fn byte(i: usize) -> u8 {
        u8::try_from(i % 251).unwrap()
    }

    /// A stream of `height` lines of `row` bytes each, plus `pad` junk bytes
    /// per line, and the packed rows the assembler should produce from it.
    fn padded_stream(row: usize, height: usize, pad: usize) -> (Vec<u8>, Vec<u8>) {
        let mut stream = Vec::new();
        let mut expected = Vec::new();
        for y in 0..height {
            let line: Vec<u8> = (0..row).map(|x| byte(x * 3 + y)).collect();
            stream.extend_from_slice(&line);
            expected.extend_from_slice(&line);
            stream.extend(std::iter::repeat_n(0xEE, pad));
        }
        (stream, expected)
    }

    #[test]
    fn reassembles_rows_of_every_format_across_odd_read_sizes() {
        let (w, h, pad) = (37, 23, 5);
        for pixel in PixelFormat::ALL {
            let row = pixel.row_bytes(w).unwrap();
            let (stream, expected) = padded_stream(row, h, pad);
            for chunk in [1, 7, 110, 116, 4096] {
                let mut asm = RowAssembler::new(row + pad, w, pixel).unwrap();
                assert_eq!(asm.out_row_bytes(), row);
                let mut out = Vec::new();
                let rows: usize = stream.chunks(chunk).map(|c| asm.push(c, &mut out)).sum();
                assert_eq!(rows, h, "{pixel} chunk size {chunk}");
                assert_eq!(out, expected, "{pixel} chunk size {chunk}");
            }
        }
    }

    #[test]
    fn row_bytes_pack_lineart_and_widen_16_bit() {
        assert_eq!(PixelFormat::Lineart.row_bytes(9), Some(2));
        assert_eq!(PixelFormat::Lineart.row_bytes(16), Some(2));
        assert_eq!(PixelFormat::Gray16.row_bytes(5), Some(10));
        assert_eq!(PixelFormat::Rgb16.row_bytes(5), Some(30));
        assert_eq!(PixelFormat::Rgb16.row_bytes(usize::MAX), None);
    }

    #[test]
    fn short_lines_are_rejected() {
        RowAssembler::new(5, 2, PixelFormat::Rgb8).unwrap_err();
        RowAssembler::new(10, 0, PixelFormat::Gray8).unwrap_err();
        RowAssembler::new(11, 2, PixelFormat::Rgb16).unwrap_err();
        RowAssembler::new(1, 9, PixelFormat::Lineart).unwrap_err();
        RowAssembler::new(6, 2, PixelFormat::Rgb8).unwrap();
        RowAssembler::new(2, 9, PixelFormat::Lineart).unwrap();
    }

    #[test]
    fn to_rgb8_reduces_every_format() {
        let mut out = Vec::new();
        // 1 = black, MSB first.
        to_rgb8(PixelFormat::Lineart, 3, &[0b1010_0000], &mut out);
        assert_eq!(out, [0, 0, 0, 255, 255, 255, 0, 0, 0]);

        out.clear();
        let gray16: Vec<u8> = [0x12ff_u16, 0xab00].iter().flat_map(|v| v.to_ne_bytes()).collect();
        to_rgb8(PixelFormat::Gray16, 2, &gray16, &mut out);
        assert_eq!(out, [0x12, 0x12, 0x12, 0xab, 0xab, 0xab]);

        out.clear();
        let rgb16: Vec<u8> = [0x0100_u16, 0x0200, 0xff01].iter().flat_map(|v| v.to_ne_bytes()).collect();
        to_rgb8(PixelFormat::Rgb16, 1, &rgb16, &mut out);
        assert_eq!(out, [1, 2, 0xff]);

        out.clear();
        to_rgb8(PixelFormat::Gray8, 2, &[7, 9, 1], &mut out);
        assert_eq!(out, [7, 7, 7, 9, 9, 9], "trailing partial row is dropped");
    }

    #[test]
    fn debug_omits_pixel_data() {
        let img = RgbImage { width: 2, height: 1, data: vec![7; 6] };
        assert_eq!(format!("{img:?}"), "RgbImage { width: 2, height: 1, data_len: 6, .. }");
    }

    #[test]
    fn thumbnail_samples_every_step_pixel() {
        let (w, h) = (10, 10);
        let rows: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| [byte(x), byte(y), byte(x + y)])).collect();
        let mut t = Thumbnail::new(w, h, 5, PixelFormat::Rgb8);
        t.push_rows(&rows);
        let img = t.into_image();
        assert_eq!((img.width, img.height), (5, 5));
        // Thumbnail pixel (2, 3) is source pixel (4, 6).
        let i = (3 * 5 + 2) * 3;
        assert_eq!(&img.data[i..i + 3], &[4, 6, 10]);
    }

    #[test]
    fn crop_row_slices_whole_byte_pixels() {
        let row: Vec<u8> = [1_u16, 2, 3, 4, 5, 6, 7, 8, 9].iter().flat_map(|v| v.to_ne_bytes()).collect();
        let mut out = Vec::new();
        PixelFormat::Rgb16.crop_row(&row, 1, 2, &mut out);
        let samples: Vec<u16> = out.as_chunks::<2>().0.iter().map(|&b| u16::from_ne_bytes(b)).collect();
        assert_eq!(samples, [4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn crop_row_realigns_lineart() {
        let row = [0b1011_0011, 0b1000_1111];
        let mut out = Vec::new();
        // Pixels 3..13: 1 0011 1000 11, then cleared spare bits.
        PixelFormat::Lineart.crop_row(&row, 3, 10, &mut out);
        assert_eq!(out, [0b1001_1100, 0b0100_0000]);
        out.clear();
        PixelFormat::Lineart.crop_row(&row, 8, 5, &mut out);
        assert_eq!(out, [0b1000_1000], "aligned crops are masked too");
        out.clear();
        PixelFormat::Lineart.crop_row(&row, 9, 7, &mut out);
        assert_eq!(out, [0b0001_1110], "the last source byte has no successor");
    }

    #[test]
    fn thumbnail_converts_lineart() {
        // 16 px wide, step 2: samples x = 0, 2, 4, ... of 0b1100_1100 1111_0000.
        let mut t = Thumbnail::new(16, 2, 8, PixelFormat::Lineart);
        t.push_rows(&[0b1100_1100, 0b1111_0000, 0, 0]);
        let img = t.into_image();
        let first: Vec<u8> = img.data[..8 * 3].iter().step_by(3).copied().collect();
        assert_eq!(first, [0, 255, 0, 255, 0, 0, 255, 255]);
    }
}

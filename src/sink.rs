//! Streaming image encoders that write packed rows straight to disk.
//!
//! Each writer consumes batches of packed rows (see [`crate::raster`]) as
//! they arrive and never holds more than one batch (plus encoder-internal
//! line buffers) in memory, so a 14 GB scan costs the same RAM as a 14 MB one.
//!
//! How each [`PixelFormat`] is stored:
//!
//! | Pixel   | PNG             | TIFF            |
//! |---------|-----------------|-----------------|
//! | Rgb8    | RGB, 8-bit      | RGB, 8-bit      |
//! | Rgb16   | RGB, 16-bit     | RGB, 16-bit     |
//! | Gray8   | gray, 8-bit     | gray, 8-bit     |
//! | Gray16  | gray, 16-bit    | gray, 16-bit    |
//! | Lineart | gray, 1-bit     | gray, 8-bit     |
//!
//! Lineart TIFFs are 8-bit because the `tiff` encoder sizes strips in whole
//! samples and can't write packed 1-bit data.
//!
//! Batches come in as `anyhow::Result`s: an `Err` aborts the write
//! immediately (used for cancellation) without finalising the file.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use tiff::encoder::colortype::{self, ColorType};
use tiff::encoder::{ImageEncoder, Rational, TiffEncoder, TiffKind, TiffValue};

use crate::job::{BIGTIFF_THRESHOLD, Format};
use crate::raster::PixelFormat;

/// Rows per TIFF strip. ~64 rows keeps strip tables small at 56k rows while
/// strips stay a few MB even at 40k px width.
const TIFF_ROWS_PER_STRIP: u32 = 64;

/// Output buffer size. Rows can be over 100 KB wide, so a large buffer keeps
/// write syscalls rare.
const WRITE_BUFFER: usize = 4 << 20;

/// Rows of white padding generated per batch by [`fit_height`].
const PAD_ROWS: usize = 64;

/// Output geometry, pixel format and resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    /// Layout of the incoming rows.
    pub pixel: PixelFormat,
}

impl Spec {
    /// Bytes in one incoming packed row.
    #[must_use]
    pub fn row_bytes(self) -> usize {
        // `u32` widths fit `usize` on every supported (64-bit) platform.
        self.pixel.row_bytes(self.width as usize).unwrap_or(usize::MAX)
    }

    /// Resolution in pixels per metre, as PNG stores it.
    fn pixels_per_metre(self) -> u32 {
        // 1 inch = 0.0254 m exactly; integer maths with rounding.
        let ppm = (u64::from(self.dpi) * 10_000 + 127) / 254;
        u32::try_from(ppm).unwrap_or(u32::MAX)
    }
}

/// Writes `batches` of packed rows to `path` in `format`.
///
/// The batches must contain exactly `spec.height` whole rows in total; wrap
/// the source in [`fit_height`] when the scanner might deliver a different
/// count.
///
/// # Errors
/// I/O and encoder errors, or the first `Err` yielded by `batches`.
pub fn write(format: Format, path: &Path, spec: Spec, batches: impl Iterator<Item = Result<Vec<u8>>>) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let out = BufWriter::with_capacity(WRITE_BUFFER, file);
    match format {
        Format::Png => write_png(out, spec, batches),
        Format::Tiff => write_tiff(out, spec, batches),
    }
}

/// Host-endian 16-bit samples as `u16`s.
fn ne_u16(bytes: &[u8]) -> Vec<u16> {
    bytes.as_chunks::<2>().0.iter().map(|&s| u16::from_ne_bytes(s)).collect()
}

/// Expands packed lineart rows (1 = black) to one gray byte per pixel.
fn lineart_to_gray8(rows: &[u8], width: usize) -> Vec<u8> {
    let stride = width.div_ceil(8).max(1);
    let mut out = Vec::with_capacity(rows.len() / stride * width);
    for row in rows.chunks_exact(stride) {
        out.extend((0..width).map(|x| if row[x / 8] >> (7 - x % 8) & 1 == 1 { 0 } else { 0xFF }));
    }
    out
}

fn write_png(out: BufWriter<File>, spec: Spec, batches: impl Iterator<Item = Result<Vec<u8>>>) -> Result<()> {
    let mut enc = png::Encoder::new(out, spec.width, spec.height);
    let (color, depth) = match spec.pixel {
        PixelFormat::Lineart => (png::ColorType::Grayscale, png::BitDepth::One),
        PixelFormat::Gray8 => (png::ColorType::Grayscale, png::BitDepth::Eight),
        PixelFormat::Gray16 => (png::ColorType::Grayscale, png::BitDepth::Sixteen),
        PixelFormat::Rgb8 => (png::ColorType::Rgb, png::BitDepth::Eight),
        PixelFormat::Rgb16 => (png::ColorType::Rgb, png::BitDepth::Sixteen),
    };
    enc.set_color(color);
    enc.set_depth(depth);
    // Default/best compression is far too slow for multi-GB images.
    enc.set_compression(png::Compression::Fast);
    let ppm = spec.pixels_per_metre();
    enc.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
    let mut stream = enc.write_header()?.into_stream_writer()?;
    for batch in batches {
        let mut batch = batch?;
        match spec.pixel {
            // PNG gray 1-bit uses 0 for black, scanners use 1.
            PixelFormat::Lineart => batch.iter_mut().for_each(|b| *b = !*b),
            // PNG samples are big-endian.
            PixelFormat::Gray16 | PixelFormat::Rgb16 => {
                for s in batch.as_chunks_mut::<2>().0 {
                    *s = u16::from_ne_bytes(*s).to_be_bytes();
                }
            }
            PixelFormat::Gray8 | PixelFormat::Rgb8 => {}
        }
        stream.write_all(&batch)?;
    }
    // Writes IEND and flushes the BufWriter, surfacing any I/O error.
    stream.finish()?;
    Ok(())
}

fn write_tiff(out: BufWriter<File>, spec: Spec, batches: impl Iterator<Item = Result<Vec<u8>>>) -> Result<()> {
    let width = spec.width as usize;
    match spec.pixel {
        PixelFormat::Rgb8 => write_tiff_as::<colortype::RGB8>(out, spec, batches, <[u8]>::to_vec),
        PixelFormat::Rgb16 => write_tiff_as::<colortype::RGB16>(out, spec, batches, ne_u16),
        PixelFormat::Gray8 => write_tiff_as::<colortype::Gray8>(out, spec, batches, <[u8]>::to_vec),
        PixelFormat::Gray16 => write_tiff_as::<colortype::Gray16>(out, spec, batches, ne_u16),
        PixelFormat::Lineart => {
            write_tiff_as::<colortype::Gray8>(out, spec, batches, |rows| lineart_to_gray8(rows, width))
        }
    }
}

/// Writes a TIFF of colour type `C`, turning each batch into samples with
/// `convert`, and switching to `BigTIFF` when classic offsets won't do.
fn write_tiff_as<C: ColorType>(
    out: BufWriter<File>,
    spec: Spec,
    batches: impl Iterator<Item = Result<Vec<u8>>>,
    convert: impl FnMut(&[u8]) -> Vec<C::Inner>,
) -> Result<()>
where
    C::Inner: Copy,
    [C::Inner]: TiffValue,
{
    let sample_bytes = (size_of::<C::Inner>() * C::BITS_PER_SAMPLE.len()) as u64;
    let raw = u64::from(spec.width) * sample_bytes * u64::from(spec.height);
    if raw > BIGTIFF_THRESHOLD {
        let mut enc = TiffEncoder::new_big(out)?;
        let img = enc.new_image::<C>(spec.width, spec.height)?;
        tiff_strips(img, spec, batches, convert)
    } else {
        let mut enc = TiffEncoder::new(out)?;
        let img = enc.new_image::<C>(spec.width, spec.height)?;
        tiff_strips(img, spec, batches, convert)
    }
}

fn tiff_strips<C: ColorType, K: TiffKind>(
    mut img: ImageEncoder<'_, BufWriter<File>, C, K>,
    spec: Spec,
    batches: impl Iterator<Item = Result<Vec<u8>>>,
    mut convert: impl FnMut(&[u8]) -> Vec<C::Inner>,
) -> Result<()>
where
    C::Inner: Copy,
    [C::Inner]: TiffValue,
{
    img.resolution(tiff::tags::ResolutionUnit::Inch, Rational { n: spec.dpi, d: 1 });
    img.encoder().write_tag(tiff::tags::Tag::Software, "lightfast")?;
    img.rows_per_strip(TIFF_ROWS_PER_STRIP)?;

    // Re-chunk incoming batches into exact strip sizes.
    let strip_len = |img: &ImageEncoder<'_, _, C, K>| {
        usize::try_from(img.next_strip_sample_count()).context("TIFF strip too large")
    };
    let mut strip: Vec<C::Inner> = Vec::with_capacity(strip_len(&img)?);
    for batch in batches {
        let samples = convert(&batch?);
        let mut rest = samples.as_slice();
        while !rest.is_empty() {
            let want = strip_len(&img)?;
            ensure!(want != 0, "scanner delivered more rows than expected");
            let take = (want - strip.len()).min(rest.len());
            strip.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if strip.len() == want {
                img.write_strip(&strip)?;
                strip.clear();
            }
        }
    }
    ensure!(img.next_strip_sample_count() == 0, "image ended before all rows were written");
    img.finish()?;
    Ok(())
}

/// Adapts a batch source so it yields exactly `height` rows.
///
/// Extra rows are dropped; if the source ends early the remainder is padded
/// with `pad` bytes (use [`PixelFormat::white_byte`]). A scanner may deliver
/// slightly more or fewer lines than estimated, but the encoders need the
/// height up front.
pub fn fit_height(
    batches: impl Iterator<Item = Result<Vec<u8>>>,
    row_bytes: usize,
    height: usize,
    pad: u8,
) -> impl Iterator<Item = Result<Vec<u8>>> {
    let mut remaining = height;
    let mut source = batches.fuse();
    std::iter::from_fn(move || {
        if remaining == 0 {
            return None;
        }
        match source.next() {
            Some(Ok(mut batch)) => {
                let rows = batch.len() / row_bytes;
                if rows > remaining {
                    log::warn!(surplus = rows - remaining; "dropping surplus rows");
                    batch.truncate(remaining * row_bytes);
                }
                remaining -= batch.len() / row_bytes;
                Some(Ok(batch))
            }
            Some(Err(e)) => {
                remaining = 0;
                Some(Err(e))
            }
            None => {
                let rows = remaining.min(PAD_ROWS);
                remaining -= rows;
                Some(Ok(vec![pad; rows * row_bytes]))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packed rows of a deterministic pattern in `spec.pixel` format.
    fn pattern(spec: Spec) -> Vec<u8> {
        let (rb, h) = (spec.row_bytes(), spec.height as usize);
        (0..h).flat_map(|y| (0..rb).map(move |i| ((i * 7) ^ (y * 5) ^ (i >> 3)).to_le_bytes()[0])).collect()
    }

    /// The samples of packed `data`, widened to 16 bits the way `image`
    /// widens 8-bit and 1-bit samples, for comparing against a decode.
    fn widened(spec: Spec, data: &[u8]) -> Vec<u16> {
        let w = spec.width as usize;
        match spec.pixel {
            PixelFormat::Lineart => lineart_to_gray8(data, w).into_iter().map(|v| u16::from(v) * 257).collect(),
            PixelFormat::Gray8 | PixelFormat::Rgb8 => data.iter().map(|&v| u16::from(v) * 257).collect(),
            PixelFormat::Gray16 | PixelFormat::Rgb16 => ne_u16(data),
        }
    }

    /// Feeds `data` as uneven batches of whole rows.
    fn batches(data: &[u8], spec: Spec) -> impl Iterator<Item = Result<Vec<u8>>> + '_ {
        let rb = spec.row_bytes();
        let sizes = [1usize, 5, 64, 3].into_iter().cycle();
        let mut off = 0;
        sizes.map_while(move |rows| {
            (off < data.len()).then(|| {
                let end = (off + rows * rb).min(data.len());
                let b = data[off..end].to_vec();
                off = end;
                Ok(b)
            })
        })
    }

    fn roundtrip(format: Format, pixel: PixelFormat) {
        // 123 px is not a multiple of 8, so lineart rows end mid-byte.
        let spec = Spec { width: 123, height: 141, dpi: 600, pixel };
        let data = pattern(spec);
        let dir = std::env::temp_dir().join(format!("lightfast-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t-{pixel:?}.{}", format.extension()));
        write(format, &path, spec, batches(&data, spec)).unwrap();
        let img = image::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((img.width(), img.height()), (spec.width, spec.height), "{format} {pixel}");
        assert_eq!(img.color().has_color(), pixel.is_color(), "{format} {pixel}");
        let decoded = if pixel.is_color() { img.to_rgb16().into_raw() } else { img.to_luma16().into_raw() };
        assert!(decoded == widened(spec, &data), "{format} {pixel} must be lossless");
        let bits = img.color().bits_per_pixel() / u16::from(img.color().channel_count());
        let want_bits = match (format, pixel) {
            // TIFF stores lineart as 8-bit; `image` expands 1-bit PNGs on
            // decode (`png_lineart_is_stored_as_one_bit` checks the file).
            (_, PixelFormat::Lineart) => 8,
            _ => u16::try_from(pixel.depth()).unwrap(),
        };
        assert_eq!(bits, want_bits, "{format} {pixel}");
    }

    #[test]
    fn png_roundtrips_every_pixel_format() {
        for p in PixelFormat::ALL {
            roundtrip(Format::Png, p);
        }
    }

    #[test]
    fn tiff_roundtrips_every_pixel_format() {
        for p in PixelFormat::ALL {
            roundtrip(Format::Tiff, p);
        }
    }

    #[test]
    fn png_lineart_is_stored_as_one_bit() {
        let spec = Spec { width: 20, height: 3, dpi: 300, pixel: PixelFormat::Lineart };
        let path = std::env::temp_dir().join(format!("lightfast-1bit-{}.png", std::process::id()));
        write(Format::Png, &path, spec, std::iter::once(Ok(pattern(spec)))).unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
        let info = decoder.read_info().unwrap().info().clone();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Grayscale, png::BitDepth::One));
    }

    #[test]
    fn fit_height_pads_and_truncates() {
        let rb = 6;
        let short: Vec<_> = fit_height(std::iter::once(Ok(vec![1; rb * 2])), rb, 5, 0xFF).map(Result::unwrap).collect();
        assert_eq!(short.concat().len(), rb * 5);
        assert!(short.concat()[rb * 2..].iter().all(|&b| b == 0xFF));

        let long: Vec<_> = fit_height(std::iter::once(Ok(vec![1; rb * 9])), rb, 4, 0xFF).map(Result::unwrap).collect();
        assert_eq!(long.concat().len(), rb * 4);
    }

    #[test]
    fn lineart_padding_is_white() {
        let padded: Vec<_> =
            fit_height(std::iter::empty(), 2, 3, PixelFormat::Lineart.white_byte()).map(Result::unwrap).collect();
        assert_eq!(lineart_to_gray8(&padded.concat(), 16), vec![0xFF; 48]);
    }

    #[test]
    fn pixels_per_metre_rounds_to_nearest() {
        let ppm = |dpi| Spec { width: 1, height: 1, dpi, pixel: PixelFormat::Rgb8 }.pixels_per_metre();
        assert_eq!(ppm(300), 11_811); // 11 811.02
        assert_eq!(ppm(4800), 188_976); // 188 976.38
    }

    #[test]
    fn abort_stops_write_with_error() {
        let spec = Spec { width: 10, height: 100, dpi: 300, pixel: PixelFormat::Rgb8 };
        let path = std::env::temp_dir().join(format!("lightfast-abort-{}.tif", std::process::id()));
        let src = [Ok(vec![0; 30 * 10]), Err(anyhow::anyhow!("cancelled"))].into_iter();
        let err = write(Format::Tiff, &path, spec, src).unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        std::fs::remove_file(&path).unwrap();
    }
}

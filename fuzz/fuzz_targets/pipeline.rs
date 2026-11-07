//! End-to-end: the scanner's byte stream -> RowAssembler -> batches ->
//! fit_height -> PNG/TIFF encoder, then decode and compare.
//!
//! Mirrors `scanner::read_frame` + `scanner::scan`: the scanner may deliver
//! more or fewer lines than it promised, and in arbitrary read sizes.
#![no_main]

use std::sync::atomic::{AtomicU64, Ordering};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use lightfast::job::Format;
use lightfast::raster::{PixelFormat, RowAssembler};
use lightfast::sink::{self, Spec};

#[derive(Debug, Arbitrary)]
struct Input {
    width: u8,
    height: u8,
    pad: u8,
    pixel: u8,
    format: u8,
    dpi: u32,
    batch_rows: u8,
    /// Lines actually delivered relative to `height`.
    delta: i8,
    reads: Vec<u16>,
    seed: u8,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Samples of one packed row, widened to 16 bits the way `image` widens
/// 8-bit and 1-bit samples on decode (lineart: 1 = black).
fn widened(pixel: PixelFormat, width: usize, row: &[u8]) -> Vec<u16> {
    match pixel {
        PixelFormat::Lineart => {
            (0..width).map(|x| if row[x / 8] >> (7 - x % 8) & 1 == 1 { 0 } else { u16::MAX }).collect()
        }
        PixelFormat::Gray8 | PixelFormat::Rgb8 => row.iter().map(|&v| u16::from(v) * 257).collect(),
        PixelFormat::Gray16 | PixelFormat::Rgb16 => {
            row.chunks_exact(2).map(|s| u16::from_ne_bytes([s[0], s[1]])).collect()
        }
    }
}

fuzz_target!(|inp: Input| {
    let width = usize::from(inp.width) + 1;
    let height = usize::from(inp.height);
    let pad = usize::from(inp.pad % 16);
    let pixel = PixelFormat::ALL[usize::from(inp.pixel) % PixelFormat::ALL.len()];
    let row = pixel.row_bytes(width).unwrap();
    let bpl = row + pad;
    let delivered = (height as isize + isize::from(inp.delta)).max(0) as usize;
    let format = Format::ALL[usize::from(inp.format) % Format::ALL.len()];

    // Synthesise the device stream.
    let sample = |i: usize, y: usize| ((i * 7 + y * 13) as u8) ^ inp.seed;
    let mut stream = Vec::with_capacity(bpl * delivered);
    let mut rows = Vec::with_capacity(row * delivered);
    for y in 0..delivered {
        let line: Vec<u8> = (0..row).map(|i| sample(i, y)).collect();
        stream.extend_from_slice(&line);
        rows.push(line);
        stream.extend(std::iter::repeat_n(0xEE, pad));
    }

    let mut asm = RowAssembler::new(bpl, width, pixel).unwrap();
    let batch_bytes = asm.out_row_bytes() * (usize::from(inp.batch_rows) + 1);
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut rest = stream.as_slice();
    let mut reads = inp.reads.iter();
    while !rest.is_empty() {
        let n = reads.next().map_or(rest.len(), |&r| usize::from(r).max(1)).min(rest.len());
        asm.push(&rest[..n], &mut batch);
        rest = &rest[n..];
        if batch.len() >= batch_bytes {
            batches.push(Ok(std::mem::take(&mut batch)));
        }
    }
    if !batch.is_empty() {
        batches.push(Ok(batch));
    }

    let spec = Spec { width: width as u32, height: height as u32, dpi: inp.dpi, pixel };
    let dir = std::env::temp_dir().join("lightfast-fuzz");
    std::fs::create_dir_all(&dir).unwrap();
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}-{id}.{}", std::process::id(), format.extension()));
    let src = sink::fit_height(batches.into_iter(), row, height, pixel.white_byte());
    let res = sink::write(format, &path, spec, src);

    if height == 0 {
        // Zero-height images may be refused, but must not crash.
        let _ = std::fs::remove_file(&path);
        return;
    }
    res.unwrap_or_else(|e| panic!("{format} write failed for {spec:?}: {e:#}"));

    let img = image::open(&path).unwrap_or_else(|e| panic!("{format} unreadable for {spec:?}: {e}"));
    std::fs::remove_file(&path).unwrap();
    assert_eq!((img.width(), img.height()), (spec.width, spec.height));
    let decoded = if pixel.is_color() { img.to_rgb16().into_raw() } else { img.to_luma16().into_raw() };
    let samples = width * pixel.channels();
    for y in 0..height {
        let want = if y < delivered { widened(pixel, width, &rows[y]) } else { vec![u16::MAX; samples] };
        assert!(decoded[y * samples..][..samples] == want[..], "row {y} {format} {pixel}");
    }
});

//! `Thumbnail` must stay within its buffer and sample nearest-neighbour
//! source pixels, whatever the geometry and batch split.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use lightfast::raster::{PixelFormat, Thumbnail, to_rgb8};

#[derive(Debug, Arbitrary)]
struct Input {
    src_width: u16,
    src_height: u16,
    max_width: u16,
    /// Rows delivered may differ from `src_height` (scanners lie).
    extra_rows: i8,
    split: Vec<u8>,
    seed: u8,
    pixel: u8,
}

fuzz_target!(|inp: Input| {
    // The scanner path only constructs thumbnails for non-empty lines.
    let w = usize::from(inp.src_width % 2048) + 1;
    let h = usize::from(inp.src_height % 2048);
    let mw = usize::from(inp.max_width);
    let delivered = (h as isize + isize::from(inp.extra_rows)).clamp(0, 4096) as usize;
    if w * delivered > 1 << 20 {
        return;
    }
    let pixel = PixelFormat::ALL[usize::from(inp.pixel) % PixelFormat::ALL.len()];
    let stride = pixel.row_bytes(w).unwrap();
    let byte = |i: usize, y: usize| (i as u8) ^ inp.seed ^ (y.wrapping_mul(31) as u8);
    let src: Vec<u8> = (0..delivered).flat_map(|y| (0..stride).map(move |i| byte(i, y))).collect();
    // Reference: the full-size RGB8 conversion of the source, done once.
    let mut full = Vec::with_capacity(w * delivered * 3);
    to_rgb8(pixel, w, &src, &mut full);
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        [full[i], full[i + 1], full[i + 2]]
    };

    let mut t = Thumbnail::new(w, h, mw, pixel);
    let mut rest = src.as_slice();
    let mut split = inp.split.iter();
    while !rest.is_empty() {
        let rows = split.next().map_or(usize::MAX, |&r| usize::from(r) + 1);
        let n = rows.saturating_mul(stride).min(rest.len());
        t.push_rows(&rest[..n]);
        rest = &rest[n..];
        let img = t.image();
        assert_eq!(img.data.len(), img.byte_len().unwrap());
    }
    let img = t.into_image();
    assert!(img.width >= 1 && img.height >= 1);
    assert!(mw == 0 || img.width <= mw.max(1), "thumbnail {} wider than max {mw}", img.width);
    let step = w.div_ceil(mw.max(1)).max(1);
    for ty in 0..img.height {
        for tx in 0..img.width {
            let (sx, sy) = (tx * step, ty * step);
            let got = &img.data[(ty * img.width + tx) * 3..][..3];
            if sy < delivered && sx < w {
                assert_eq!(got, px(sx, sy), "pixel ({tx},{ty}) step {step}");
            }
        }
    }
});

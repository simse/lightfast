//! Geometry and size-estimate helpers used by the UI on every frame.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use lightfast::job::{Area, Estimate, human_bytes, mm_to_px};
use lightfast::raster::PixelFormat;

#[derive(Debug, Arbitrary)]
struct Input {
    area: [f64; 4],
    bed: [f64; 2],
    dpi: u32,
    pixel: u8,
    bytes: u64,
}

fuzz_target!(|inp: Input| {
    // Bed sizes come from the scanner's br-x/br-y ranges: finite and at
    // least a few millimetres. Selections come from mouse maths: finite.
    let [bw, bh] = inp.bed;
    if bw.is_finite() && bh.is_finite() && (1.0..1e6).contains(&bw) && (1.0..1e6).contains(&bh)
        && inp.area.iter().all(|v| v.is_finite())
    {
        let [x, y, w, h] = inp.area;
        let a = Area::new(x, y, w, h).clamped(bw, bh);
        let eps = 1e-9 * bw.max(bh);
        assert!(a.w >= 1.0 && a.w <= bw && a.h >= 1.0 && a.h <= bh, "{a:?}");
        assert!(a.x >= 0.0 && a.y >= 0.0, "{a:?}");
        assert!(a.x + a.w <= bw + eps && a.y + a.h <= bh + eps, "{a:?} off bed {bw}x{bh}");
        // Clamping is idempotent.
        assert_eq!(a.clamped(bw, bh), a);

        let pixel = PixelFormat::ALL[usize::from(inp.pixel) % PixelFormat::ALL.len()];
        let est = Estimate::new(a, inp.dpi, pixel);
        let _ = est.to_string();
        // Raw size is monotonic in bits per pixel.
        let rgb16 = Estimate { pixel: PixelFormat::Rgb16, ..est };
        assert!(est.raw_bytes() <= rgb16.raw_bytes());
    }

    for v in inp.area {
        let px = mm_to_px(v, inp.dpi);
        if v <= 0.0 || v.is_nan() || inp.dpi == 0 {
            assert!(px <= 1, "mm_to_px({v}, {}) = {px}", inp.dpi);
        }
    }
    // Monotonic in length.
    let [a, b, ..] = inp.area;
    if a.is_finite() && b.is_finite() && a <= b {
        assert!(mm_to_px(a, inp.dpi) <= mm_to_px(b, inp.dpi));
    }

    // human_bytes: well-formed and within 5% of the true value.
    let s = human_bytes(inp.bytes);
    let (num, unit) = s.split_once(' ').expect("number and unit");
    let mul: f64 = match unit {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => panic!("unit {unit}"),
    };
    let val: f64 = num.parse().expect("numeric");
    let real = inp.bytes as f64;
    assert!((val * mul - real).abs() <= real * 0.05 + 0.5, "{} -> {s}", inp.bytes);
});

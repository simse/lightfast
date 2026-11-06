//! Host-side image processing (`pixma_common.c`, `pixma_mp150.c`).
//!
//! The firmware calibrates and shades; the host only crops, shrinks below
//! the hardware's minimum resolution, converts 48-bit colour to 16-bit gray
//! and binarises 8-bit gray to lineart. Each function reproduces the C
//! arithmetic exactly, since the replay tests compare images byte for byte.
//! Names follow the C code: `c` bytes per pixel, `w` pixels, `wx` raw width.
#![allow(clippy::many_single_char_names, reason = "names match pixma_common.c")]

/// Fills `table` with a 1024-entry gamma table of little-endian `u16`s
/// (`pixma_fill_gamma_table` for tables other than 4096 entries).
pub(crate) fn fill_gamma_table(gamma: f64, table: &mut [u8]) {
    let n = table.len() / 2;
    let r_gamma = 1.0 / gamma;
    let in_scale = 1.0 / f64::from(u32::try_from(n - 1).unwrap_or(u32::MAX));
    for (i, out) in table.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let x = f64::from(u32::try_from(i).unwrap_or(u32::MAX)) * in_scale;
        // `as` saturates where C's conversion would be undefined; in range
        // the two agree (truncation toward zero).
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "matches C's (uint16_t) cast")]
        let value = (65535.0 * x.powf(r_gamma) + 0.5) as u16;
        *out = value.to_le_bytes();
    }
}

/// Converts `w` pixels of 24- or 48-bit RGB to 8- or 16-bit gray with the
/// Rec. 709 weights, appending to `dst` (`pixma_rgb_to_gray`). `c` is the
/// bytes per source pixel: 3 or 6.
pub(crate) fn rgb_to_gray(dst: &mut Vec<u8>, src: &[u8], w: usize, c: usize) {
    for px in src.chunks_exact(c).take(w) {
        let g = if c == 6 {
            let r = u32::from(u16::from_le_bytes([px[0], px[1]]));
            let g = u32::from(u16::from_le_bytes([px[2], px[3]]));
            let b = u32::from(u16::from_le_bytes([px[4], px[5]]));
            (r * 2126 + g * 7152 + b * 722) / 10000
        } else {
            (u32::from(px[0]) * 2126 + u32::from(px[1]) * 7152 + u32::from(px[2]) * 722) / 10000
        };
        let [lo, hi, ..] = g.to_le_bytes();
        dst.push(lo);
        if c == 6 {
            dst.push(hi);
        }
    }
}

/// Averages `scale` × `scale` blocks of a raw line group into `w` pixels of
/// `c` bytes, skipping `xs` pixels on the left (`shrink_image`). `src` holds
/// `scale` raw lines of `wx` pixels each. Like the C code this averages each
/// byte separately, so 16-bit samples are averaged per byte.
pub(crate) fn shrink(dst: &mut Vec<u8>, src: &[u8], xs: usize, w: usize, wx: usize, scale: usize, c: usize) {
    let divisor = u32::try_from(scale * scale).unwrap_or(u32::MAX);
    let mut base = c * xs;
    for _ in 0..w {
        for ic in 0..c {
            let mut sum = 0u32;
            for m in 0..scale {
                for n in 0..scale {
                    sum += u32::from(src[base + ic + c * n + wx * c * m]);
                }
            }
            dst.push(u8::try_from(sum / divisor).unwrap_or(u8::MAX));
        }
        base += c * scale;
    }
}

/// Builds the lineart threshold lookup table (`load_lut` from genesys).
/// `slope` and `offset` run from -127 to 127.
#[expect(clippy::cast_possible_truncation, reason = "matches C's double-to-int conversion")]
pub(crate) fn load_lut(lut: &mut [u8; 256], out_min: i32, out_max: i32, slope: i32, offset: i32) {
    let max_in = 255.0;
    let max_out = 255.0;
    let rise = (f64::from(slope) / 127.0 * std::f64::consts::FRAC_PI_2).tan() * max_out / max_in;
    let mut shift = max_out / 2.0 - rise * max_in / 2.0;
    shift += f64::from(offset) / 127.0 * max_out / 2.0;
    for (i, out) in lut.iter_mut().enumerate() {
        let j = (rise * f64::from(u8::try_from(i).unwrap_or(u8::MAX)) + shift) as i32;
        *out = u8::try_from(j.clamp(out_min, out_max)).unwrap_or(u8::MAX);
    }
}

/// Lineart settings for [`binarize_line`].
#[derive(Debug, Clone)]
pub(crate) struct Lineart {
    pub(crate) threshold: u32,
    pub(crate) threshold_curve: u32,
    pub(crate) lut: [u8; 256],
    pub(crate) xdpi: u32,
}

/// Binarises one line of `width` 8-bit gray pixels to 1 bit per pixel
/// (1 = black, MSB first), appending `width / 8` bytes to `dst`
/// (`pixma_binarize_line` for single-byte gray). `src` is normalised in
/// place first, as in C.
pub(crate) fn binarize_line(dst: &mut Vec<u8>, src: &mut [u8], width: usize, la: &Lineart) {
    let src = &mut src[..width];
    let (mut min, mut max) = (255u8, 0u8);
    for &v in src.iter() {
        max = max.max(v);
        min = min.min(v);
    }
    if min > 80 {
        min = 0;
    }
    if max < 80 {
        max = 255;
    }
    // C divides by zero (and crashes) when every pixel is exactly 80.
    if max > min {
        let span = u32::from(max - min);
        for v in src.iter_mut() {
            *v = u8::try_from((u32::from(*v - min) * 255) / span).unwrap_or(u8::MAX);
        }
    }

    let mut window = usize::try_from(6 * la.xdpi / 150).unwrap_or(usize::MAX);
    if window % 2 == 0 {
        window += 1;
    }
    let offset_x = 1 + (window / 2) / 8;
    let mut sum: u32 = (offset_x..=window).filter_map(|j| src.get(j)).map(|&v| u32::from(v)).sum();

    let mut byte = 0u8;
    for j in 0..width {
        let bit = j % 8;
        let mask = 0x80u8 >> bit;
        let mut threshold = la.threshold;
        if la.threshold_curve != 0 {
            let add = j + window / 2;
            if let Some(drop) = add.checked_sub(window)
                && drop >= offset_x
                && add < width
            {
                sum += u32::from(src[add]);
                sum -= sum.min(u32::from(src[drop]));
            }
            let idx = usize::try_from(sum).unwrap_or(usize::MAX) / window;
            threshold = u32::from(la.lut[idx.min(255)]);
        }
        if u32::from(src[j]) > threshold {
            byte &= !mask;
        } else {
            byte |= mask;
        }
        if bit == 7 {
            dst.push(byte);
            byte = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_table_endpoints() {
        let mut t = vec![0u8; 2048];
        fill_gamma_table(2.2, &mut t);
        assert_eq!(&t[..2], [0, 0]);
        assert_eq!(&t[2046..], [0xff, 0xff]);
        fill_gamma_table(1.0, &mut t);
        // 65535 * 1/1023 + 0.5 = 64.56 -> 64
        assert_eq!(u16::from_le_bytes([t[2], t[3]]), 64);
    }

    #[test]
    fn gray_uses_rec709_weights() {
        let mut out = Vec::new();
        rgb_to_gray(&mut out, &[255, 255, 255, 255, 0, 0], 2, 3);
        assert_eq!(out, [255, 54]);
        out.clear();
        rgb_to_gray(&mut out, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff], 1, 6);
        assert_eq!(out, [0xff, 0xff]);
    }

    #[test]
    fn shrink_averages_blocks() {
        // Two raw lines of 4 gray pixels, scale 2, no offset.
        let src = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut out = Vec::new();
        shrink(&mut out, &src, 0, 2, 4, 2, 1);
        assert_eq!(out, [35, 55]);
    }

    #[test]
    fn binarize_thresholds_normalised_pixels() {
        let la = Lineart { threshold: 127, threshold_curve: 0, lut: [0; 256], xdpi: 300 };
        let mut src = [0u8, 255, 0, 255, 0, 255, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0];
        let mut out = Vec::new();
        binarize_line(&mut out, &mut src, 16, &la);
        assert_eq!(out, [0b1010_1010, 0b0000_1111]);
    }
}

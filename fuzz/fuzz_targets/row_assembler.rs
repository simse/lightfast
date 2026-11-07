//! Differential test: `RowAssembler` fed in arbitrary read sizes must match a
//! straightforward reference that strips each line's padding.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use lightfast::raster::{PixelFormat, RowAssembler};

#[derive(Debug, Arbitrary)]
struct Input {
    bytes_per_line: u16,
    pixels: u16,
    pixel: u8,
    chunks: Vec<u16>,
    data: Vec<u8>,
}

fuzz_target!(|inp: Input| {
    let (bpl, px) = (usize::from(inp.bytes_per_line), usize::from(inp.pixels));
    let pixel = PixelFormat::ALL[usize::from(inp.pixel) % PixelFormat::ALL.len()];
    let used = pixel.row_bytes(px).unwrap();
    let Ok(mut asm) = RowAssembler::new(bpl, px, pixel) else {
        assert!(px == 0 || bpl < used, "valid geometry rejected: bpl={bpl} px={px} {pixel}");
        return;
    };
    assert_eq!(asm.out_row_bytes(), used);

    let mut out = Vec::new();
    let mut rows = 0;
    let mut rest = inp.data.as_slice();
    let mut sizes = inp.chunks.iter().map(|&c| usize::from(c));
    while !rest.is_empty() {
        let n = sizes.next().unwrap_or(rest.len()).min(rest.len());
        let before = out.len();
        let got = asm.push(&rest[..n], &mut out);
        assert_eq!(out.len() - before, got * used, "row count disagrees with output");
        rows += got;
        rest = &rest[n..];
    }

    let mut expected = Vec::new();
    for line in inp.data.chunks_exact(bpl) {
        expected.extend_from_slice(&line[..used]);
    }
    assert_eq!(rows, inp.data.len() / bpl);
    assert!(out == expected, "output differs from reference");
});

//! Flatbed photo/art scanner, driving scanners through the pure-Rust
//! `scan` crates.
//!
//! - [`scanner`] runs all device I/O on a dedicated worker thread.
//! - [`sink`] streams rows to PNG/TIFF without buffering whole images.
//! - [`job`] describes scan areas and estimates output sizes.

pub mod job;
pub mod raster;
pub mod scanner;
pub mod sink;

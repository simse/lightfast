//! Headless scan for testing the pipeline against real hardware.
//!
//! `cargo run --release --example cli_scan -- <dpi> <x> <y> <w> <h> <out.{png,tif}>`
//! (area in mm), or with no arguments to just run a preview.
//!
//! Set `CANCEL_AFTER=<secs>` to cancel the scan after that long, then run a
//! preview on the same handle to check the scanner recovers.
//!
//! Set `PREVIEW_FIRST=1` to run a preview before the scan, on the same
//! handle, as the app does.
//!
//! Optional settings, matched against what the scanner offers (the
//! available values are printed on startup):
//!
//! - `MODE=<label>`, e.g. `MODE="Gray 16-bit"`; defaults to 8-bit colour.
//! - `SOURCE=<name>`, e.g. `SOURCE="Transparency Unit"`.
//! - `GAMMA=<value>`, e.g. `GAMMA=1.8`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use scanapp::job::{Area, Format, Job, Output, ScanMode};
use scanapp::scanner::{Caps, Command, Event, Scanner};

/// The scan requested on the command line, before mode and source are
/// resolved against the scanner.
struct Request {
    area: Area,
    dpi: u32,
    format: Format,
    dest: PathBuf,
}

/// Device settings from the environment.
struct Settings {
    mode: Option<String>,
    source: Option<String>,
    gamma: Option<f64>,
}

impl Settings {
    fn from_env() -> Result<Self> {
        Ok(Self {
            mode: std::env::var("MODE").ok(),
            source: std::env::var("SOURCE").ok(),
            gamma: std::env::var("GAMMA").ok().map(|g| g.parse()).transpose().context("GAMMA must be a number")?,
        })
    }

    /// Resolves the source and mode names against `caps`.
    fn resolve(&self, caps: &Caps) -> Result<(Option<String>, ScanMode)> {
        let src = caps.source(self.source.as_deref()).with_context(|| {
            let names: Vec<_> = caps.sources.iter().map(|s| s.name.as_str()).collect();
            format!("no source {:?}; choose one of {names:?}", self.source)
        })?;
        let mode = match &self.mode {
            None => src.default_mode().clone(),
            Some(label) => src.modes.iter().find(|m| m.label == *label).cloned().with_context(|| {
                let labels: Vec<_> = src.modes.iter().map(|m| m.label.as_str()).collect();
                format!("no mode {label:?}; choose one of {labels:?}")
            })?,
        };
        Ok((Some(src.name.clone()), mode))
    }
}

/// Parses the scan arguments, or returns `None` for a preview-only run.
fn parse_request(args: &[String]) -> Result<Option<Request>> {
    let [dpi, x, y, w, h, out] = args else {
        if args.is_empty() {
            return Ok(None);
        }
        bail!("usage: cli_scan <dpi> <x> <y> <w> <h> <out.{{png,tif}}>");
    };
    let mm = |s: &String| s.parse::<f64>().with_context(|| format!("'{s}' is not a length in mm"));
    let dest: PathBuf = std::path::absolute(out)?;
    let format = match dest.extension().and_then(|e| e.to_str()) {
        Some("png") => Format::Png,
        Some("tif" | "tiff") => Format::Tiff,
        _ => bail!("output must end in .png or .tif"),
    };
    Ok(Some(Request {
        area: Area::new(mm(x)?, mm(y)?, mm(w)?, mm(h)?),
        dpi: dpi.parse().with_context(|| format!("'{dpi}' is not a whole DPI value"))?,
        format,
        dest,
    }))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut request = parse_request(&args)?;
    let settings = Settings::from_env()?;
    let cancel_after = std::env::var("CANCEL_AFTER")
        .ok()
        .map(|s| s.parse().map(Duration::from_secs))
        .transpose()
        .context("CANCEL_AFTER must be a whole number of seconds")?;

    let preview_first = std::env::var_os("PREVIEW_FIRST").is_some();
    let (scanner, events) = Scanner::spawn()?;
    scanner.send(Command::Discover);
    let mut sent = false;
    let mut job = None;
    let mut last_decile = None;
    while let Ok(ev) = events.recv_blocking() {
        match &ev {
            Event::Thumb(_) => continue,
            Event::Progress { rows, total } => {
                let pct = rows * 100 / (*total).max(1);
                if last_decile != Some(pct / 10) {
                    eprintln!("{rows}/{total} rows ({pct}%)");
                    last_decile = Some(pct / 10);
                }
                continue;
            }
            Event::PreviewDone(img) => println!("\npreview {}x{}", img.width, img.height),
            _ => println!("{ev:?}"),
        }
        match ev {
            Event::Ready(caps) if !sent => {
                sent = true;
                for src in &caps.sources {
                    let modes: Vec<_> = src.modes.iter().map(|m| m.label.as_str()).collect();
                    println!("source {:?}: modes {modes:?}, gamma {:?}", src.name, src.gamma);
                }
                let (source, mode) = settings.resolve(&caps)?;
                let Some(req) = request.take() else {
                    scanner.send(Command::Preview { source, gamma: settings.gamma });
                    continue;
                };
                println!("scanning {mode} ({:?}) at {} DPI", mode.pixel, req.dpi);
                let scan = Job {
                    source,
                    mode,
                    dpi: req.dpi,
                    gamma: settings.gamma,
                    format: req.format,
                    outputs: vec![Output { area: req.area, dest: req.dest }],
                };
                if preview_first {
                    scanner.send(Command::Preview { source: scan.source.clone(), gamma: settings.gamma });
                    job = Some(scan);
                    continue;
                }
                scanner.send(Command::Scan(scan));
                if let Some(delay) = cancel_after {
                    let s = scanner.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        println!("requesting cancel");
                        s.cancel();
                    });
                }
            }
            Event::PreviewDone(_) if job.is_some() => scanner.send(Command::Scan(job.take().expect("checked"))),
            Event::Cancelled if cancel_after.is_some() => {
                scanner.send(Command::Preview { source: settings.source.clone(), gamma: settings.gamma });
            }
            Event::NoScanner | Event::Saved(_) | Event::PreviewDone(_) | Event::Cancelled | Event::Failed(_) => break,
            _ => {}
        }
    }
    Ok(())
}

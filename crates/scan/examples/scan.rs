//! A small `scanimage` stand-in for testing backends on real hardware.
//!
//! ```text
//! cargo run -p scan --release --example scan -- --list
//! cargo run -p scan --release --example scan -- [-d NAME] [--mode Color] [--resolution 300]
//!     [-l MM] [-t MM] [-x MM] [-y MM] [--gamma G] [--calibrate Once|Always|Never]
//!     [--cancel-after SECS] [--repeat N] -o out.pnm
//! ```
//!
//! Writes PNM like `scanimage --format=pnm` (16-bit samples big-endian), so
//! outputs can be compared with C SANE's. `--repeat` scans N times on one
//! open device, writing `out-1.pnm`, `out-2.pnm`, ...

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use scan::{Area, CancelToken, Channels, OptionValue, Parameters, ScanSettings};

#[derive(Debug, Default)]
struct Args {
    list: bool,
    device: Option<String>,
    mode: Option<String>,
    resolution: Option<u32>,
    l: f64,
    t: f64,
    x: Option<f64>,
    y: Option<f64>,
    gamma: Option<f64>,
    calibrate: Option<String>,
    cancel_after: Option<f64>,
    repeat: usize,
    out: Option<PathBuf>,
}

fn parse() -> Result<Args> {
    let mut a = Args { repeat: 1, ..Args::default() };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--list" | "-L" => a.list = true,
            "-d" => a.device = Some(val()?),
            "--mode" => a.mode = Some(val()?),
            "--resolution" => a.resolution = Some(val()?.parse()?),
            "-l" => a.l = val()?.parse()?,
            "-t" => a.t = val()?.parse()?,
            "-x" => a.x = Some(val()?.parse()?),
            "-y" => a.y = Some(val()?.parse()?),
            "--gamma" => a.gamma = Some(val()?.parse()?),
            "--calibrate" => a.calibrate = Some(val()?),
            "--cancel-after" => a.cancel_after = Some(val()?.parse()?),
            "--repeat" => a.repeat = val()?.parse()?,
            "-o" => a.out = Some(val()?.into()),
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

fn write_pnm(path: &Path, p: &Parameters, data: &[u8]) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let magic = match (p.layout.channels, p.layout.depth) {
        (Channels::Gray, 1) => "P4",
        (Channels::Gray, _) => "P5",
        (Channels::Rgb, _) => "P6",
    };
    write!(f, "{magic}\n# SANE data follows\n{} {}\n", p.pixels_per_line, p.lines)?;
    match p.layout.depth {
        1 => f.write_all(data)?,
        8 => {
            f.write_all(b"255\n")?;
            f.write_all(data)?;
        }
        _ => {
            f.write_all(b"65535\n")?;
            for px in data.as_chunks::<2>().0 {
                f.write_all(&[px[1], px[0]])?;
            }
        }
    }
    f.flush()?;
    Ok(())
}

fn numbered(path: &Path, i: usize, n: usize) -> PathBuf {
    if n == 1 {
        return path.to_owned();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    path.with_file_name(format!("{stem}-{i}{ext}"))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse()?;
    let devices = scan::list_devices()?;
    if args.list {
        for d in &devices {
            println!("device `{}' is a {} {}", d.name, d.vendor, d.model);
        }
        return Ok(());
    }
    let name = match args.device {
        Some(n) => n,
        None => devices.first().context("no scanners found")?.name.clone(),
    };
    let mut dev = scan::open(&name)?;
    let src = dev.capabilities().sources[0].clone();
    let settings = ScanSettings {
        source: src.name.clone(),
        mode: args.mode.unwrap_or_else(|| src.modes[0].name.clone()),
        resolution: args.resolution.unwrap_or(75),
        area: Area {
            x: args.l,
            y: args.t,
            width: args.x.unwrap_or(src.width_mm - args.l),
            height: args.y.unwrap_or(src.height_mm - args.t),
        },
        gamma: args.gamma,
        extras: args.calibrate.map(|c| ("calibrate".to_owned(), OptionValue::Choice(c))).into_iter().collect(),
    };
    let out = args.out.context("-o is required")?;
    for i in 1..=args.repeat {
        let cancel = CancelToken::new();
        if let Some(secs) = args.cancel_after {
            let c = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs_f64(secs));
                c.cancel();
            });
        }
        let started = Instant::now();
        let params = dev.start(&settings, &cancel)?;
        eprintln!("scan {i}: {params:?}");
        let mut data = Vec::with_capacity(params.bytes_per_line * params.lines);
        let mut buf = vec![0u8; 1 << 20];
        let result = loop {
            match dev.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => data.extend_from_slice(&buf[..n]),
                Err(e) => break Err(e),
            }
        };
        dev.finish();
        match result {
            Ok(()) => {
                let path = numbered(&out, i, args.repeat);
                write_pnm(&path, &params, &data)?;
                eprintln!("scan {i}: {} bytes in {:.1?} -> {}", data.len(), started.elapsed(), path.display());
            }
            Err(e) if e.is_cancelled() => eprintln!("scan {i}: cancelled after {:.1?}", started.elapsed()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

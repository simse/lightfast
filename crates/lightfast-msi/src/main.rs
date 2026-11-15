//! Builds Lightfast's Windows installer: one MSI per architecture, holding
//! `Lightfast.exe` and `WinSparkle.dll`.
//!
//! ```text
//! lightfast-msi --exe Lightfast.exe --winsparkle WinSparkle.dll \
//!     --icon windows/lightfast.ico --version 0.1.0 --arch x64 --out Lightfast.msi
//! ```
//!
//! It writes the installer database directly with the `msi` crate and its
//! files into an embedded MSZIP cabinet with the `cab` crate, so it runs on
//! any platform and needs no `WiX` toolset. See `docs/windows-release.md`.

mod package;
mod ui;

use std::path::PathBuf;

use anyhow::{Context as _, bail};

/// The build's target architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "x64" | "x86_64" => Ok(Self::X64),
            "arm64" | "aarch64" => Ok(Self::Arm64),
            _ => bail!("unknown architecture {s:?}: expected x64 or arm64"),
        }
    }

    /// The Summary Information stream's platform, in its Template property.
    #[must_use]
    pub fn platform(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "Arm64",
        }
    }

    /// The minimum Windows Installer schema the platform needs, which goes
    /// in the Summary Information's page count.
    #[must_use]
    pub fn schema(self) -> i32 {
        match self {
            Self::X64 => 200,
            Self::Arm64 => 500,
        }
    }

    /// The architecture's name in GUID seeds and file names.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

/// What goes in the installer.
#[derive(Debug)]
pub struct Input {
    pub exe: PathBuf,
    pub winsparkle: PathBuf,
    pub icon: PathBuf,
    /// The release version, `major.minor.patch`: Windows Installer compares
    /// only those three fields.
    pub version: String,
    pub arch: Arch,
}

fn main() -> anyhow::Result<()> {
    let mut exe = None;
    let mut winsparkle = None;
    let mut icon = None;
    let mut version = None;
    let mut arch = None;
    let mut out = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--exe" => exe = Some(PathBuf::from(value)),
            "--winsparkle" => winsparkle = Some(PathBuf::from(value)),
            "--icon" => icon = Some(PathBuf::from(value)),
            "--version" => version = Some(value),
            "--arch" => arch = Some(Arch::parse(&value)?),
            "--out" => out = Some(PathBuf::from(value)),
            _ => bail!("unknown option {flag}"),
        }
    }
    let input = Input {
        exe: exe.context("--exe is required")?,
        winsparkle: winsparkle.context("--winsparkle is required")?,
        icon: icon.context("--icon is required")?,
        version: version.context("--version is required")?,
        arch: arch.context("--arch is required")?,
    };
    let out = out.context("--out is required")?;
    package::check_version(&input.version)?;
    package::build(&input, &out)?;
    println!("built {}", out.display());
    Ok(())
}

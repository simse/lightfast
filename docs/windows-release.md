# Windows release

Windows builds ship as MSIs, one per architecture (`x64` and `arm64`), built by the `windows` job
in `.github/workflows/build.yml` and published with each tagged release next to the DMG and the
AppImages. They update themselves with [WinSparkle](https://winsparkle.org) from the same appcast
as the macOS and Linux apps.

## Layout

- `crates/lightfast-msi`: writes the MSI. It builds the installer database with the
  [`msi`](https://crates.io/crates/msi) crate and packs the files into an MSZIP cabinet, embedded
  in the MSI, with [`cab`](https://crates.io/crates/cab). No WiX or Windows SDK is needed, so it
  runs anywhere; only the executable must be built on Windows.
- `windows/build-msi.sh`: builds `Lightfast-<arch>.msi`. It downloads a pinned, checksummed
  WinSparkle release, runs `cargo build --release` for the architecture and then
  `lightfast-msi`.
- `windows/test-msi.ps1`: installs an MSI through its dialogs (driven with UI Automation), starts
  the app, installs it again silently and removes it. CI runs it on every build. It changes
  Program Files, so only run it on a throwaway machine.
- `windows/lightfast.ico` and `windows/lightfast.rc`: the icon, rendered from
  `linux/io.simse.lightfast.png`, which `build.rs` embeds in the executable.

## What the installer does

- Installs per machine into `C:\Program Files\Lightfast` (`Lightfast.exe` and `WinSparkle.dll`),
  with a Start menu shortcut. Windows asks for administrator approval once, when the install starts.
- Shows Welcome → Progress → Finish, with a "Start Lightfast" check box. Opening the MSI again
  when the same build is installed offers to remove it. Settings → Apps removes it too.
- Is a major upgrade of any installed Lightfast: every build has a new product code, and the
  shared upgrade code (`UPGRADE_CODE` in `crates/lightfast-msi/src/package.rs`, never to change)
  finds the installed copy, which is removed before the new files go in. Downgrades work the
  same way.
- Component GUIDs come from the upgrade code, the file and the architecture, so they stay the same
  across versions as Windows Installer requires.

Windows Installer compares only `major.minor.patch`, so the tag version must be three numbers
under 256.256.65536; `lightfast-msi` refuses anything else.

## Differences from macOS

As on Linux: no Liquid Glass, menus drawn in the title bar, Ctrl rather than Cmd. GPUI draws
with DirectX 11. Release builds are GUI programs (no console window), so logs only show in a
debug build (`cargo run`).

## Scanner access

The drivers talk to USB through [nusb](https://crates.io/crates/nusb), which on Windows needs the
scanner bound to Microsoft's generic WinUSB driver rather than the manufacturer's. Use
[Zadig](https://zadig.akeo.ie) to switch it (Options → List All Devices, pick the scanner, choose
WinUSB, Replace Driver). That stops the manufacturer's own software from seeing the scanner until
the driver is changed back in Device Manager.

## Updates

WinSparkle reads the same appcast as Sparkle (`SUFeedURL`) and checks the same Ed25519 key
(`SUPublicEDKey`); `src/updater.rs` has copies of both for Linux and Windows. The MSI installs
`WinSparkle.dll` next to the executable, which loads it at runtime: a build run without it, or
without `LIGHTFAST_BUILD`, has no updater and no Check for Updates… item. WinSparkle checks
daily and from Lightfast → Check for Updates…, shows its own windows, downloads the MSI and asks
Lightfast to quit before it runs it. It keeps its settings in
`HKEY_CURRENT_USER\Software\Lightfast\WinSparkle`.

**One feed, many platforms.** WinSparkle only takes items whose enclosure is marked with its
architecture (`windows-x64` or `windows-arm64`), or with no `sparkle:os` at all, so
`scripts/add-to-appcast.py` marks the DMG's enclosure `macos` and adds one item per platform.

**Versions.** Like Sparkle, WinSparkle compares the build number, the `version` job's UTC
timestamp baked in from `LIGHTFAST_BUILD`, and shows the release version.

**Installing.** The items carry `sparkle:installerArguments="/passive"`, so WinSparkle runs the
MSI with Windows Installer's progress bar and no dialogs (and the administrator prompt). The app
doesn't restart by itself afterwards; start it from the Start menu.

**Signing.** The release job signs each MSI with `SPARKLE_ED_PRIVATE_KEY`, as it does the Linux
tarball. The MSIs aren't Authenticode-signed, so SmartScreen warns about an unknown publisher on
first install (More info → Run anyway). Signing them needs a code-signing certificate and a
`signtool` step before the Ed25519 signature.

## Building locally

On Windows, with Git Bash, Rust and the Visual Studio C++ build tools (with the Windows SDK):

```sh
windows/build-msi.sh x64      # or arm64; writes build/windows/Lightfast-x64.msi
```

From macOS or Linux, [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) cross-checks and
cross-builds debug executables (it needs LLVM's `clang-cl`, `lld-link` and `llvm-rc`):

```sh
cargo xwin clippy --target x86_64-pc-windows-msvc --workspace --all-targets
cargo xwin build --target x86_64-pc-windows-msvc --bin lightfast
cargo run -p lightfast-msi -- --exe target/x86_64-pc-windows-msvc/debug/lightfast.exe \
  --winsparkle WinSparkle.dll --icon windows/lightfast.ico --version 0.1.0 --arch x64 \
  --out Lightfast.msi
```

Release builds can't be cross-compiled: GPUI only precompiles its shaders when built on Windows.
`msiinfo` and `msiextract` from [msitools](https://wiki.gnome.org/msitools) inspect an MSI anywhere.

### Testing an update

As on Linux, `LIGHTFAST_FEED_URL` and `LIGHTFAST_PUBLIC_KEY` at build time replace the feed and
key. Build an old and a new MSI with `LIGHTFAST_BUILD=1` and `=2`, sign the new one with
`openssl pkeyutl -sign -inkey test.pem -rawin -in Lightfast-x64.msi | base64 -w0`, add it to an
appcast with `scripts/add-to-appcast.py ... --os windows-x64 --type application/x-msi
--installer-arguments /passive`, serve the directory, install the old MSI and check for updates.

## Publishing a release

Tagging publishes every platform (see `docs/macos-release.md`). The release gets
`Lightfast-x64.msi` and `Lightfast-arm64.msi`, for first installs (stable links:
`https://github.com/simse/lightfast/releases/latest/download/Lightfast-x64.msi`) and updates
alike. A Windows build or install-test failure fails the release, as the others do.

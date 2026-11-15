#!/usr/bin/env bash
# Builds Lightfast-<arch>.msi.
#
#   windows/build-msi.sh x64|arm64 [output-dir]      (default: build/windows)
#
# Run on Windows (Git Bash) with the MSVC build tools: release builds compile
# GPUI's shaders with the Windows SDK's fxc.exe. Set LIGHTFAST_BUILD to the
# appcast's `sparkle:version` for this build (CI uses the release's UTC
# timestamp); without it the app has no updater. Needs curl and unzip.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
arch="${1:?usage: build-msi.sh x64|arm64 [output-dir]}"
out="$(mkdir -p "${2:-$repo/build/windows}" && cd "${2:-$repo/build/windows}" && pwd)"
case "$arch" in
  x64) target=x86_64-pc-windows-msvc; winsparkle_dir=x64 ;;
  arm64) target=aarch64-pc-windows-msvc; winsparkle_dir=ARM64 ;;
  *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
esac

# Pinned, and checked against this hash before it's used.
winsparkle_version=0.9.4
winsparkle_sha256=6037df37fc263bd1650a1c4949681a9d40ffe991d01f35892a406cb5d103c976

tools="$out/tools"
mkdir -p "$tools"
zip="$tools/WinSparkle-$winsparkle_version.zip"
if [[ ! -f "$zip" ]] || ! echo "$winsparkle_sha256  $zip" | sha256sum -c --quiet - 2>/dev/null; then
  curl -fsSL --retry 4 --retry-all-errors -o "$zip.part" \
    "https://github.com/vslavik/winsparkle/releases/download/v$winsparkle_version/WinSparkle-$winsparkle_version.zip"
  echo "$winsparkle_sha256  $zip.part" | sha256sum -c --quiet -
  mv "$zip.part" "$zip"
fi
rm -rf "$tools/WinSparkle-$winsparkle_version"
unzip -q "$zip" "WinSparkle-$winsparkle_version/$winsparkle_dir/Release/WinSparkle.dll" -d "$tools"
dll="$tools/WinSparkle-$winsparkle_version/$winsparkle_dir/Release/WinSparkle.dll"

cargo build --release --locked --manifest-path "$repo/Cargo.toml" --bin lightfast --target "$target"
target_dir="$(cargo metadata --no-deps --format-version 1 --manifest-path "$repo/Cargo.toml" \
  | sed -E 's/.*"target_directory":"([^"]*)".*/\1/' | sed 's/\\\\/\//g')"
version="$(sed -n '/^\[package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml")"

# The MSI builder runs on the build machine, so it's built for the host.
cargo run --locked --manifest-path "$repo/Cargo.toml" -p lightfast-msi -- \
  --exe "$target_dir/$target/release/lightfast.exe" --winsparkle "$dll" \
  --icon "$repo/windows/lightfast.ico" --version "$version" --arch "$arch" \
  --out "$out/Lightfast-$arch.msi"

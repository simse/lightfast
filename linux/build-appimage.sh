#!/usr/bin/env bash
# Builds Lightfast-<arch>.AppImage for this machine's architecture.
#
#   linux/build-appimage.sh [output-dir]      (default: build/linux)
#
# Set LIGHTFAST_BUILD to the appcast's `sparkle:version` for this build (CI
# uses the release's UTC timestamp); without it the AppImage has no updater.
# Needs GPUI's build dependencies (see docs/linux-release.md) and curl.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$(mkdir -p "${1:-$repo/build/linux}" && cd "${1:-$repo/build/linux}" && pwd)"
arch="$(uname -m)"
app_id=io.simse.lightfast

# Pinned tools, checked against these hashes before they run. The runtime is
# passed explicitly so appimagetool doesn't download the latest one itself.
appimagetool_version=1.9.1
runtime_version=20251108
case "$arch" in
  x86_64)
    appimagetool_sha256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    appimagetool_sha256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *)
    echo "unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

tools="$out/tools"
mkdir -p "$tools"
fetch() { # url sha256 dest
  if [[ ! -f "$3" ]] || ! echo "$2  $3" | sha256sum -c --quiet - 2>/dev/null; then
    curl -fsSL --retry 4 --retry-all-errors -o "$3.part" "$1"
    echo "$2  $3.part" | sha256sum -c --quiet -
    mv "$3.part" "$3"
  fi
}
fetch "https://github.com/AppImage/appimagetool/releases/download/$appimagetool_version/appimagetool-$arch.AppImage" \
  "$appimagetool_sha256" "$tools/appimagetool-$arch.AppImage"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$runtime_version/runtime-$arch" \
  "$runtime_sha256" "$tools/runtime-$arch"
chmod +x "$tools/appimagetool-$arch.AppImage"

cargo build --release --locked --manifest-path "$repo/Cargo.toml" --bin lightfast
target_dir="$(cargo metadata --no-deps --format-version 1 --manifest-path "$repo/Cargo.toml" \
  | sed -E 's/.*"target_directory":"([^"]*)".*/\1/')"

appdir="$out/Lightfast.AppDir"
rm -rf "$appdir"
install -Dm755 "$target_dir/release/lightfast" "$appdir/usr/bin/lightfast"
install -Dm644 "$repo/linux/$app_id.desktop" "$appdir/usr/share/applications/$app_id.desktop"
install -Dm644 "$repo/linux/$app_id.png" "$appdir/usr/share/icons/hicolor/512x512/apps/$app_id.png"
install -Dm644 "$repo/linux/$app_id.metainfo.xml" "$appdir/usr/share/metainfo/$app_id.metainfo.xml"
# The AppImage spec wants the desktop entry and icon at the root too, and
# .DirIcon is what LinSparkle's update window and file managers show.
ln -s "usr/share/applications/$app_id.desktop" "$appdir/$app_id.desktop"
ln -s "usr/share/icons/hicolor/512x512/apps/$app_id.png" "$appdir/$app_id.png"
ln -s "$app_id.png" "$appdir/.DirIcon"
ln -s usr/bin/lightfast "$appdir/AppRun"

# The tool is an AppImage itself; extracting it avoids needing FUSE (CI
# runners and containers don't have it).
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$arch" "$tools/appimagetool-$arch.AppImage" --no-appstream \
  --runtime-file "$tools/runtime-$arch" "$appdir" "$out/Lightfast-$arch.AppImage"
echo "built $out/Lightfast-$arch.AppImage"

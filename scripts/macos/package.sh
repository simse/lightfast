#!/usr/bin/env bash
# Build a signed, notarized, stapled Lightfast.dmg. One-time setup and
# troubleshooting: docs/macos-release.md.
#
# Env:
#   LIGHTFAST_P12_PASSWORD   password of signing/developer-id.p12 (required)
#   LIGHTFAST_SIGN_IDENTITY  keychain identity for nested code; defaults to the
#                          first "Developer ID Application" identity found
#   LIGHTFAST_NOTARY_PROFILE notarytool keychain profile (default: lightfast-notary)
#   LIGHTFAST_VERSION        release version such as 0.1.0; rewrites the version
#                          in Cargo.toml (default: leave it as committed)
#   SKIP_NOTARIZE=1        stop after signing (quicker iteration)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

profile="${LIGHTFAST_NOTARY_PROFILE:-lightfast-notary}"
p12="signing/developer-id.p12"
app="target/release/bundle/dmg/Lightfast.app"
dmg="target/release/bundle/dmg/Lightfast.dmg"
sparkle_version="2.10.1"
sparkle_sha256="1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27"
sparkle_dir="target/sparkle"

die() { echo "package: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

mounted=""
detach() {
    [[ -z "$mounted" ]] && return 0
    hdiutil detach "$mounted" -quiet
    rmdir "$mounted" 2>/dev/null || true
    mounted=""
}
trap detach EXIT

# Attaches a disk image at a private mount point, stored in $mounted.
attach() {
    mounted="$(mktemp -d)"
    hdiutil attach "$1" -nobrowse -noverify -noautoopen -mountpoint "$mounted" -quiet
}

# Fails unless the bundle's marketing version is the requested release version.
check_version() {
    [[ -z "${LIGHTFAST_VERSION:-}" ]] && return 0
    local actual
    actual="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$1/Contents/Info.plist")"
    [[ "$actual" == "$LIGHTFAST_VERSION" ]] || die "$1 has version $actual, expected $LIGHTFAST_VERSION"
}

# Hardened runtime on each piece, as cargo-bundle only sets it on the app.
sign() { codesign --force --timestamp -o runtime -s "$LIGHTFAST_SIGN_IDENTITY" "$@"; }

# The app isn't sandboxed, so Sparkle's XPC services are never used, and
# removing them leaves less to sign and ship.
fetch_sparkle() {
    [[ -f "$sparkle_dir/.version-$sparkle_version" ]] && return 0
    rm -rf "$sparkle_dir"
    mkdir -p "$sparkle_dir"
    local archive="$sparkle_dir/Sparkle.tar.xz"
    curl -fsSL -o "$archive" \
        "https://github.com/sparkle-project/Sparkle/releases/download/$sparkle_version/Sparkle-$sparkle_version.tar.xz"
    echo "$sparkle_sha256  $archive" | shasum -a 256 -c - >/dev/null || die "Sparkle checksum mismatch"
    tar -xJf "$archive" -C "$sparkle_dir"
    rm -f "$archive" "$sparkle_dir/Sparkle.framework/XPCServices"
    rm -rf "$sparkle_dir/Sparkle.framework/Versions/B/XPCServices" "$sparkle_dir/Symbols" \
        "$sparkle_dir/Sparkle Test App.app"
    touch "$sparkle_dir/.version-$sparkle_version"
}

step "Preflight"
cargo bundle --version >/dev/null 2>&1 || die "cargo-bundle missing: cargo install cargo-bundle --version 0.12.0 --locked"
[[ -f "$p12" ]] || die "$p12 not found (export your Developer ID cert, see docs/macos-release.md)"
[[ -n "${LIGHTFAST_P12_PASSWORD:-}" ]] || die "LIGHTFAST_P12_PASSWORD is not set"
if [[ -z "${LIGHTFAST_SIGN_IDENTITY:-}" ]]; then
    LIGHTFAST_SIGN_IDENTITY="$(security find-identity -v -p codesigning \
        | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1)"
fi
[[ -n "$LIGHTFAST_SIGN_IDENTITY" ]] || die "no 'Developer ID Application' identity in the keychain"
export LIGHTFAST_SIGN_IDENTITY
echo "identity: $LIGHTFAST_SIGN_IDENTITY"
if [[ -z "${SKIP_NOTARIZE:-}" ]]; then
    xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 \
        || die "notarytool profile '$profile' missing: xcrun notarytool store-credentials $profile ..."
fi

if [[ -n "${LIGHTFAST_VERSION:-}" ]]; then
    step "Stamping version $LIGHTFAST_VERSION"
    [[ "$LIGHTFAST_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "LIGHTFAST_VERSION must be X.Y.Z, got '$LIGHTFAST_VERSION'"
    sed -i '' -E "/^\[package\]/,/^\[/ s/^version = \".*\"/version = \"$LIGHTFAST_VERSION\"/" Cargo.toml
fi

step "Fetching Sparkle $sparkle_version"
fetch_sparkle

step "Bundling"
MACOSX_DEPLOYMENT_TARGET=27.0 cargo bundle --release --format dmg
check_version "$app"

# The layered Icon Composer icon goes in as a compiled asset catalog, not a
# flat .icns, so macOS can render its dark, clear and tinted variants.
# actool also writes a Lightfast.icns fallback and the CFBundleIcon* keys.
step "Compiling app icon"
icon_plist="$(mktemp -t lightfast-icon).plist"
xcrun actool assets/Lightfast.icon --compile "$app/Contents/Resources" \
    --app-icon Lightfast --platform macosx --target-device mac \
    --minimum-deployment-target 27.0 --output-partial-info-plist "$icon_plist" \
    --output-format human-readable-text --errors --warnings >/dev/null
/usr/libexec/PlistBuddy -c "Merge $icon_plist" "$app/Contents/Info.plist" >/dev/null
rm -f "$icon_plist"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconName' "$app/Contents/Info.plist")" == Lightfast ]] \
    || die "CFBundleIconName missing from Info.plist"

# Inside out and without --deep, as Sparkle's documentation requires.
step "Signing Sparkle and the app"
framework="$app/Contents/Frameworks/Sparkle.framework"
sign "$framework/Versions/B/Autoupdate"
sign "$framework/Versions/B/Updater.app"
sign "$framework"
sign --entitlements scripts/macos/entitlements.plist "$app"

# cargo-bundle copied the app into its image before the signing above. Swap in
# the signed copy, keeping the image's Finder layout.
step "Replacing the app in the DMG"
rm -f "$dmg.rw.dmg"
hdiutil convert "$dmg" -format UDRW -o "$dmg.rw.dmg" -quiet
attach "$dmg.rw.dmg"
rm -rf "$mounted/Lightfast.app"
ditto "$app" "$mounted/Lightfast.app"
detach

# cargo-bundle writes a zlib (UDZO) image. LZMA (ULMO, macOS 10.15+) is ~30%
# smaller. Converting drops the signature, so sign it again.
step "Recompressing DMG with LZMA"
rm -f "$dmg" "$dmg.ulmo.dmg"
hdiutil convert "$dmg.rw.dmg" -format ULMO -o "$dmg.ulmo.dmg" -quiet
rm -f "$dmg.rw.dmg"
mv "$dmg.ulmo.dmg" "$dmg"
codesign --force --timestamp -s "$LIGHTFAST_SIGN_IDENTITY" "$dmg"
ls -lh "$dmg" | awk '{print "dmg: " $5}'

step "Verifying signatures"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv --verbose=4 "$app" 2>&1 | grep -E "^(Authority|Timestamp|TeamIdentifier|CodeDirectory)" || true
codesign --verify --verbose=2 "$dmg"
attach "$dmg"
codesign --verify --deep --strict --verbose=2 "$mounted/Lightfast.app"
check_version "$mounted/Lightfast.app"
detach

if [[ -n "${SKIP_NOTARIZE:-}" ]]; then
    echo; echo "SKIP_NOTARIZE set: signed but not notarized: $dmg"
    exit 0
fi

step "Notarizing (usually a few minutes)"
out="$(xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait --output-format json)"
echo "$out"
id="$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' <<<"$out" | head -1)"
if ! grep -q '"status" *: *"Accepted"' <<<"$out"; then
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$profile"
    die "notarization failed"
fi

step "Stapling"
xcrun stapler staple "$dmg"
spctl -a -t open --context context:primary-signature -vv "$dmg"

echo; echo "Done: $dmg"

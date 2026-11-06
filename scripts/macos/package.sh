#!/usr/bin/env bash
# Build a signed, notarized, stapled ScanApp.dmg. One-time setup and
# troubleshooting: docs/macos-release.md.
#
# Env:
#   SCANAPP_P12_PASSWORD   password of signing/developer-id.p12 (required)
#   SCANAPP_SIGN_IDENTITY  keychain identity for nested code; defaults to the
#                          first "Developer ID Application" identity found
#   SCANAPP_NOTARY_PROFILE notarytool keychain profile (default: scanapp-notary)
#   SKIP_NOTARIZE=1        stop after signing (quicker iteration)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

profile="${SCANAPP_NOTARY_PROFILE:-scanapp-notary}"
p12="signing/developer-id.p12"
app="target/release/bundle/dmg/ScanApp.app"
dmg="target/release/bundle/dmg/ScanApp.dmg"

die() { echo "package: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

step "Preflight"
cargo bundle --version >/dev/null 2>&1 || die "cargo-bundle missing: cargo install cargo-bundle --version 0.12.0 --locked"
[[ -f "$p12" ]] || die "$p12 not found (export your Developer ID cert, see docs/macos-release.md)"
[[ -n "${SCANAPP_P12_PASSWORD:-}" ]] || die "SCANAPP_P12_PASSWORD is not set"
if [[ -z "${SCANAPP_SIGN_IDENTITY:-}" ]]; then
    SCANAPP_SIGN_IDENTITY="$(security find-identity -v -p codesigning \
        | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1)"
fi
[[ -n "$SCANAPP_SIGN_IDENTITY" ]] || die "no 'Developer ID Application' identity in the keychain"
export SCANAPP_SIGN_IDENTITY
echo "identity: $SCANAPP_SIGN_IDENTITY"
if [[ -z "${SKIP_NOTARIZE:-}" ]]; then
    xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 \
        || die "notarytool profile '$profile' missing: xcrun notarytool store-credentials $profile ..."
fi

step "Bundling"
MACOSX_DEPLOYMENT_TARGET=27.0 cargo bundle --release --format dmg

# cargo-bundle writes a zlib (UDZO) image. LZMA (ULMO, macOS 10.15+) is ~30%
# smaller. Converting drops the signature, so sign it again.
step "Recompressing DMG with LZMA"
rm -f "$dmg.ulmo.dmg"
hdiutil convert "$dmg" -format ULMO -o "$dmg.ulmo.dmg" -quiet
mv "$dmg.ulmo.dmg" "$dmg"
codesign --force --timestamp -s "$SCANAPP_SIGN_IDENTITY" "$dmg"
ls -lh "$dmg" | awk '{print "dmg: " $5}'

step "Verifying signatures"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv --verbose=4 "$app" 2>&1 | grep -E "^(Authority|Timestamp|TeamIdentifier|CodeDirectory)" || true
codesign --verify --verbose=2 "$dmg"

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

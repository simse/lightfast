#!/usr/bin/env bash
# Record one SANE USB session to XML with the record/replay build in
# target/sane-record, then replay it and check the replayed image matches.
#
# Usage: record-sane.sh <out-dir> <name> <device> [scanimage options...]
#   e.g. record-sane.sh tests/recordings/pixma-lide400 gray-150 \
#          pixma:04A91912_508624 --mode Gray --resolution 150 -x 50 -y 50
#
# Writes <out-dir>/<name>.xml (the USB transfers) and <out-dir>/<name>.pnm
# (the image C SANE produced). The device must be a full backend device name
# including any serial suffix, and its backend must be preloaded into libsane
# (PRELOADABLE_BACKENDS) or nothing is recorded on macOS.
#
# Env:
#   CANCEL_AFTER  seconds; send SIGINT to scanimage after this long to record
#                 a cancelled scan. The replay check is skipped.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
prefix="$root/target/sane-record"
[[ $# -ge 3 ]] || { sed -n '2,9p' "$0" >&2; exit 2; }
out_dir="$1" name="$2" device="$3"; shift 3

die() { echo "record-sane: $*" >&2; exit 1; }
[[ -x "$prefix/bin/scanimage" ]] || die "no record/replay build at $prefix"

export DYLD_LIBRARY_PATH="$prefix/lib" SANE_CONFIG_DIR="$prefix/etc/sane.d"
mkdir -p "$out_dir"
xml="$(cd "$out_dir" && pwd)/$name.xml"
pnm="${xml%.xml}.pnm"
rm -f "$xml" "$pnm"

if [[ -n "${CANCEL_AFTER:-}" ]]; then
    "$prefix/bin/scanimage" -d "fakeusbout:$xml:$device" --format=pnm -o "$pnm" "$@" &
    pid=$!
    sleep "$CANCEL_AFTER"
    kill -INT "$pid"
    wait "$pid" || true
    rm -f "$pnm"
    [[ -s "$xml" ]] || die "$name: no recording written"
    echo "$name: cancelled after ${CANCEL_AFTER}s, $(du -h "$xml" | cut -f1)"
    exit 0
fi

"$prefix/bin/scanimage" -d "fakeusbout:$xml:$device" --format=pnm -o "$pnm" "$@"
[[ -s "$xml" ]] || die "$name: no recording written"

replay="$(mktemp -t "$name").pnm"
trap 'rm -f "$replay"' EXIT
"$prefix/bin/scanimage" -d "fakeusb:$xml" --format=pnm -o "$replay" "$@"
cmp -s "$pnm" "$replay" || die "$name: replayed image differs from the recorded scan"
echo "$name: ok, $(du -h "$xml" | cut -f1) xml, $(head -c 64 "$pnm" | sed -n 3p) px"

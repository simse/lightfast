# CanoScan LiDE 400 USB recordings

USB sessions recorded from C SANE (`vendor/backends` master, `1.4.0-414-gf4bd07353`,
pixma 0.28.6) driving a CanoScan LiDE 400 (`04a9:1912`, device
`pixma:04A91912_508624`) on macOS, recorded on 2026-11-03 with a printed page
on the glass.

Each `<name>.xml` is SANE's record/replay format: every control, bulk and
interrupt transfer of one `scanimage` run. `<name>.pnm` is the image C SANE
produced in that run. Replaying the XML through C SANE (`-d fakeusb:<name>.xml`
with the same options) reproduces the `.pnm` byte for byte; this was checked
for every recording except the cancelled one.

Recorded with `scripts/macos/record-sane.sh`. All area options are in mm.

| Recording | scanimage options | Output px |
|---|---|---|
| `color-75-fullpage` | `--mode Color --resolution 75` (full glass) | 638×877 |
| `gray-150` | `--mode Gray --resolution 150 -x 50 -y 50` | 295×295 |
| `lineart-300` | `--mode Lineart --resolution 300 -x 50 -y 50` | 592×591 |
| `color-300` | `--mode Color --resolution 300 -x 50 -y 50` | 591×591 |
| `gray-300` | `--mode Gray --resolution 300 -x 50 -y 50` | 591×591 |
| `color48-300` | `--mode "48 bits color" --resolution 300 -x 50 -y 50` | 591×591 |
| `gray16-300` | `--mode "16 bits gray" --resolution 300 -x 50 -y 50` | 591×591 |
| `color-300-offset` | `--mode Color --resolution 300 -l 100 -t 150 -x 30 -y 30` | 354×354 |
| `color-600` | `--mode Color --resolution 600 -l 60 -t 60 -x 25 -y 25` | 591×591 |
| `color-1200` | `--mode Color --resolution 1200 -l 60 -t 60 -x 12.5 -y 12.5` | 590×590 |
| `color-2400` | `--mode Color --resolution 2400 -l 60 -t 60 -x 6.25 -y 6.25` | 591×591 |
| `color-4800` | `--mode Color --resolution 4800 -l 60 -t 60 -x 3.125 -y 3.125` | 590×590 |
| `color48-4800` | `--mode "48 bits color" --resolution 4800 -l 60 -t 60 -x 3.125 -y 3.125` | 590×590 |
| `gray-4800` | `--mode Gray --resolution 4800 -l 60 -t 60 -x 3.125 -y 3.125` | 590×590 |
| `color-150-gamma1` | `--mode Color --resolution 150 -x 50 -y 50 --gamma 1.0` | 295×295 |
| `color-150-calibrate-always` | `... -x 50 -y 50 --calibrate Always` | 295×295 |
| `color-150-calibrate-never` | `... -x 50 -y 50 --calibrate Never` | 295×295 |
| `cancel-color-300-fullpage` | `--mode Color --resolution 300`, SIGINT after 8 s | none |
| `batch2-gray-150-page2-fails` | `--mode Gray --resolution 150 -x 50 -y 50 --batch-count=2` | 295×295 (page 1) |

## Notes

- **Back-to-back scans fail.** In `batch2-gray-150-page2-fails` the second
  scan in one session gets `PCScanning - Previous scan attempt was not
  completed` from the device. Stock SANE 1.4.0 (Homebrew) does the same, so it
  is existing pixma behaviour, not this build. Replay reproduces the error, and
  page 1 matches.
- **Not recorded:** a scanner button press (needs someone at the scanner).
- **Size.** Bulk data is stored as hex text, and the driver scans at a higher
  hardware resolution than requested at low dpi, so the full-page 75 dpi
  recording is 77 MB. libxml2 2.15 cannot replay compressed XML, but zstd
  shrinks it about 5x if the Rust replay transport decompresses on load.

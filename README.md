# ScanApp

A flatbed photo and art scanner for macOS, built on GPUI, with scanner drivers written in pure safe Rust instead of C SANE.

## Scanner drivers

Each driver is a crate under `crates/`, ported from the matching backend in [sane-backends](https://gitlab.com/sane-project/backends) and registered in `crates/scan/src/drivers.rs`. The goal is to cover every USB and network backend; SCSI-only, parallel-port and camera backends are out of scope.

7 of the 47 other in-scope backends are ported so far, and `pixma` is partial. The table below is the current state.

### Hardware testing

**Only the CanoScan LiDE 400 has been tested on a real scanner.** We don't own any other supported scanner, so every other driver has never talked to real hardware and may not work.

| Driver | Evidence |
| --- | --- |
| `pixma` (LiDE 400) | Tested on hardware, plus replay tests against USB sessions recorded from C SANE |
| `pixma` (LiDE 300) | Same protocol as the LiDE 400, not tested on the device itself |
| Everything else | Unit tests against a simulated device, written from reading the C source |

A simulated device only shows that a driver matches our reading of the C code. It can't catch a misread protocol or a device quirk the source doesn't mention.

If you own one of these scanners, a USB recording from stock SANE would let us replay that session as a regression test: see `tests/recordings/pixma-lide400/README.md` and `scripts/macos/record-sane.sh`. Reports of what works and what doesn't are just as welcome.

### Driver status

| Backend | Status | Licence of the C source |
| --- | --- | --- |
| `artec_eplus48u` | Not ported yet | GPL + SANE exception |
| `avision` | Not ported yet | GPL + SANE exception |
| `canon630u` | Not ported yet | GPL + SANE exception |
| `canon_dr` | Not ported yet | GPL + SANE exception |
| `canon_lide70` | Not ported yet | GPL, no exception |
| `cardscan` | Ported, untested | GPL + SANE exception |
| `coolscan2` | Not ported yet | GPL + SANE exception |
| `coolscan3` | Not ported yet | GPL, no exception |
| `dell1600n_net` | Ported, untested | GPL + SANE exception |
| `epjitsu` | Not ported yet | GPL + SANE exception |
| `epson` | Not ported yet | GPL + SANE exception |
| `epson2` | Not ported yet | GPL, no exception |
| `epsonds` | Not ported yet | GPL, no exception |
| `escl` | Not ported yet | GPL-3.0+, no exception |
| `fujitsu` | Not ported yet | GPL + SANE exception |
| `genesys` | Not ported yet | GPL, no exception |
| `gt68xx` | Not ported yet | GPL + SANE exception |
| `hp` | Not ported yet | GPL + SANE exception |
| `hp3500` | Not ported yet | GPL + SANE exception |
| `hp3900` | Not ported yet | GPL + SANE exception |
| `hp4200` | Not ported yet | GPL, exception on only some files |
| `hp5400` | Not ported yet | GPL + SANE exception |
| `hp5590` | Not ported yet | GPL + SANE exception |
| `hpljm1005` | Ported, untested | GPL + SANE exception |
| `kodakaio` | Not ported yet | GPL, no exception |
| `kvs1025` | Not ported yet | no licence text in source |
| `kvs20xx` | Not ported yet | no licence text in source |
| `kvs40xx` | Not ported yet | no licence text in source |
| `lexmark` | Not ported yet | GPL + SANE exception |
| `lexmark_x2600` | Ported, untested | GPL + SANE exception |
| `ma1509` | Ported, untested | GPL + SANE exception |
| `magicolor` | Not ported yet | GPL, no exception |
| `mustek_usb` | Not ported yet | GPL + SANE exception |
| `mustek_usb2` | Not ported yet | GPL + SANE exception |
| `niash` | Not ported yet | GPL, no exception |
| `pieusb` | Not ported yet | GPL + SANE exception |
| `pixma` | Partial: mp150 generation 5 only (LiDE 300, LiDE 400) | GPL + SANE exception |
| `plustek` | Not ported yet | GPL + SANE exception |
| `ricoh2` | Ported, untested | GPL + SANE exception |
| `rts8891` | Not ported yet | GPL + SANE exception |
| `sm3600` | Not ported yet | GPL + SANE exception |
| `sm3840` | Not ported yet | GPL + SANE exception |
| `snapscan` | Not ported yet | GPL + SANE exception |
| `stv680` | Not ported yet | GPL, no exception |
| `u12` | Not ported yet | GPL + SANE exception |
| `umax` | Not ported yet | GPL + SANE exception |
| `umax1220u` | Not ported yet | GPL + SANE exception |
| `xerox_mfp` | Ported, untested | GPL + SANE exception |

### Licensing

The ported crates are derivative works of their SANE backends and keep their licences. Most backends carry the SANE linking exception, which lets ScanApp link them without becoming GPL.

Backends marked as having no exception would make the whole program GPL if linked, so when ported they will build only with the `gpl-backends` feature of the `scan` crate, which is off by default. None of them are ported yet. The licence column comes from each backend's source headers and isn't legal advice.

## Testing

```sh
cargo test --workspace
```

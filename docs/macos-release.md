# macOS release

`scripts/macos/package.sh` produces `target/release/bundle/dmg/ScanApp.dmg`. The DMG is
self-contained, Developer ID signed, notarized and stapled.

## What the bundle contains

Just the `scanapp` executable. Scanner drivers are the pure-Rust crates in `crates/`
(`scan-pixma` over `nusb`, which talks to IOKit directly), so there are no dylibs,
backend plugins or config files to bundle, and users need nothing installed.

**Platform:** Apple Silicon, macOS 27+ (`minimum_system_version` in `Cargo.toml`).

## One-time setup

1. **Tools**
   ```sh
   cargo install cargo-bundle --version 0.12.0 --locked
   ```
2. **Developer ID Application certificate.** In Xcode: Settings → Accounts → (your team) →
   Manage Certificates → **+** → *Developer ID Application*. This needs the Account Holder
   role. Check it with `security find-identity -v -p codesigning`.
3. **Export it as .p12.** cargo-bundle signs from a file, not the keychain. In Keychain
   Access → My Certificates, right-click *Developer ID Application: …* → Export, then save
   it as `signing/developer-id.p12` (gitignored) with a password.
4. **Notary credentials.** Create an app-specific password at <https://account.apple.com> →
   Sign-In and Security, then run:
   ```sh
   xcrun notarytool store-credentials scanapp-notary \
     --apple-id <your apple id> --team-id <TEAMID> --password <app-specific password>
   ```
   Your Team ID is at <https://developer.apple.com/account> → Membership details.

## Building a release

```sh
export SCANAPP_P12_PASSWORD=...           # e.g. from your password manager
scripts/macos/package.sh                  # SKIP_NOTARIZE=1 to stop after signing
```

## CI

`.github/workflows/build.yml` builds a signed DMG on every push to `main` and every pull
request, and uploads it as a workflow artifact. CI builds are not notarized.

It needs two repository secrets:

- `SCANAPP_P12_BASE64`: `base64 -i signing/developer-id.p12 | tr -d '\n' | gh secret set SCANAPP_P12_BASE64`
- `SCANAPP_P12_PASSWORD`: the password of the `.p12`

## Notes

- `cargo build` / `cargo run` need nothing beyond Rust: no SANE, libusb or pkg-config.
- No icon is configured yet. Add `icon = ["assets/icon/…png"]` under
  `[package.metadata.bundle]`.

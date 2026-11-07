# macOS release

`scripts/macos/package.sh` produces `target/release/bundle/dmg/Lightfast.dmg`. The DMG is
self-contained, Developer ID signed, notarized and stapled.

## What the bundle contains

The `lightfast` executable and `Sparkle.framework`. Scanner drivers are the pure-Rust crates
in `crates/` (`scan-pixma` over `nusb`, which talks to IOKit directly), so there are no
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
   xcrun notarytool store-credentials lightfast-notary \
     --apple-id <your apple id> --team-id <TEAMID> --password <app-specific password>
   ```
   Your Team ID is at <https://developer.apple.com/account> → Membership details.

## Publishing a release

Tag a commit on `main` and push the tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

CI builds the DMG with that version, then publishes a GitHub release with
`Lightfast-0.1.0.dmg` and `appcast.xml` attached. Installed apps read the appcast from the
latest release, so publishing is what offers them the update. Only plain `vX.Y.Z` tags
release.

The tag is the source of truth for the version: CI rewrites `version` in `Cargo.toml` before
building, and the committed value only applies to untagged builds.

## Building locally

```sh
export LIGHTFAST_P12_PASSWORD=...           # e.g. from your password manager
scripts/macos/package.sh                  # SKIP_NOTARIZE=1 to stop after signing
```

`LIGHTFAST_VERSION=0.1.0` sets the version, and leaves `Cargo.toml` modified.

## Updates

Sparkle fetches its feed from `SUFeedURL` in `scripts/macos/Info.plist.ext`, and only accepts
updates signed by the private key matching `SUPublicEDKey`.

**Back up the private key.** It lives in the login Keychain of the Mac that created it, under
the Sparkle account `lightfast`, and in the `SPARKLE_ED_PRIVATE_KEY` secret, which can't be read
back. If it is lost, installed apps can't accept any further update. To export it, using
`generate_keys` from `target/sparkle/bin` after a build:

```sh
generate_keys --account lightfast -x lightfast-sparkle.key   # store it somewhere safe, then delete it
```

**Versions.** cargo-bundle sets `CFBundleVersion` to the build's timestamp, and Sparkle compares
that value, not the marketing version, to decide what is newer. `generate_appcast` copies it
from the built app, so the build most recently tagged wins, whatever its version number says.

**Feed URL.** The URL depends on this repository's name and on it being public, since Sparkle
can't authenticate to GitHub. Renaming the repository leaves installed apps relying on GitHub's
redirect.

## CI

`.github/workflows/build.yml` builds a signed, notarized and stapled DMG on every push to
`main`, every pull request and every version tag, and uploads it as a workflow artifact. Version
tags also publish the release.

It needs these repository secrets:

- `LIGHTFAST_P12_BASE64`: `base64 -i signing/developer-id.p12 | tr -d '\n' | gh secret set LIGHTFAST_P12_BASE64`
- `LIGHTFAST_P12_PASSWORD`: the password of the `.p12`
- `LIGHTFAST_NOTARY_KEY_BASE64`: an App Store Connect API key, `base64 -i AuthKey_XXXXXXXXXX.p8 | tr -d '\n' | gh secret set LIGHTFAST_NOTARY_KEY_BASE64`
- `LIGHTFAST_NOTARY_KEY_ID`: that key's ID
- `LIGHTFAST_NOTARY_ISSUER_ID`: the issuer ID shown above the list of keys
- `SPARKLE_ED_PRIVATE_KEY`: the exported Sparkle key, `gh secret set SPARKLE_ED_PRIVATE_KEY < lightfast-sparkle.key`

## Notes

- `cargo build` / `cargo run` need nothing beyond Rust: no SANE, libusb or pkg-config.
- The app icon is `assets/Lightfast.icon`, a layered Icon Composer bundle. Edit it in
  Icon Composer. `package.sh` compiles it with `actool` into `Contents/Resources/Assets.car`
  (plus a `Lightfast.icns` fallback) and sets `CFBundleIconName`, so macOS renders the
  dark, clear and tinted variants. Don't add a flat `icon = [...]` to
  `[package.metadata.bundle]`: it would replace the layered icon. `cargo run` shows no
  icon, because only the bundled app has one.

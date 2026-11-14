# Linux release

Linux builds ship as AppImages, one per architecture (`x86_64` and `aarch64`), built by the
`linux` job in `.github/workflows/build.yml` and published with each tagged release next to the
macOS DMG. They update themselves with [LinSparkle](https://github.com/medcelerate/LinSparkle)
from the same appcast as the macOS app.

## Layout

- `linux/build-appimage.sh`: builds `Lightfast-<arch>.AppImage` for the machine it runs on. It
  runs `cargo build --release`, lays out an AppDir and packs it with a pinned, checksummed
  `appimagetool` and AppImage runtime.
- `linux/io.simse.lightfast.desktop`, `.png` and `.metainfo.xml`: the desktop entry, icon and
  AppStream metadata. The id is the macOS bundle identifier; the app passes it to Wayland as its
  `app_id` so the window gets this icon and name. The icon is rendered from `assets/Lightfast.icon`.
- `linux/add-to-appcast.py`: adds the Linux update to the appcast `generate_appcast` wrote for
  the DMG.

## Differences from macOS

The UI is the same GPUI view. What macOS draws natively, Linux draws with GPUI:

- **No Liquid Glass.** `ui::glass` is inert, so the window is opaque and the sidebar, toolbar and
  controls use the gpui-component versions, as on macOS before 26.
- **Menus in the window.** GPUI has no global menu bar on Linux, so the same menus (Lightfast,
  Scan, View) are drawn in the title bar by gpui-component's `AppMenuBar`.
- **Ctrl, not Cmd.** Shortcuts use Ctrl: Ctrl+Q, Ctrl+= and so on.
- **Window controls.** The title bar draws its own close, maximise and minimise buttons when
  the compositor wants client-side decorations, as GNOME does.
- **Icons.** The component library's icons (check marks, dropdown chevrons) come from
  `gpui-kit-assets`, registered in `main`. macOS's glass UI mostly uses native controls instead.

`Cargo.toml` depends on `gpui-pre-linux` directly only to turn on its `wayland` and `x11`
features: `gpui-pre-platform` depends on it with none, and without them GPUI runs headless,
opening windows that never reach the screen and logging nothing about it.

## Building locally

Install GPUI's build dependencies. On Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config libxkbcommon-devel libxkbcommon-x11-devel \
  wayland-devel libxcb-devel vulkan-loader-devel fontconfig-devel freetype-devel
```

On Debian and Ubuntu:

```sh
sudo apt install build-essential pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libx11-xcb-dev libvulkan-dev libfontconfig-dev libfreetype-dev
```

Then `cargo run`, or `linux/build-appimage.sh` for an AppImage in `build/linux`. Builds made
without `LIGHTFAST_BUILD` have no updater.

The binary links only glibc, fontconfig and freetype; it loads Wayland, X11, xkbcommon and
Vulkan when it starts. CI builds on Ubuntu 24.04, so the AppImages need glibc 2.39 or newer
(Fedora 40, Ubuntu 24.04, Debian 13). GPUI draws with Vulkan, so a machine without a Vulkan
driver needs Mesa's `lavapipe` (`mesa-vulkan-drivers`).

## Scanner access

The drivers talk to USB directly, so the user needs write access to the scanner's device node.
Distributions that ship SANE's udev rules (`sane-backends` on Fedora, `libsane1` on Debian and
Ubuntu) grant it to the logged-in user for every scanner SANE knows. Without them, add a rule
such as this to `/etc/udev/rules.d/70-lightfast.rules` (here for the CanoScan LiDE 400):

```
SUBSYSTEM=="usb", ATTRS{idVendor}=="04a9", ATTRS{idProduct}=="1912", TAG+="uaccess"
```

## Updates

LinSparkle reads the same appcast as Sparkle (`SUFeedURL`) and checks the same Ed25519 key
(`SUPublicEDKey`). `src/updater.rs` holds copies of both: change them together with
`macos/Info.plist`. It checks daily and from Lightfast → Check for Updates…, shows its own
window in a helper copy of the executable, replaces the running AppImage in place and relaunches
it. Run outside an AppImage, it offers the download instead.

**One feed, two platforms.** Each updater ignores items whose enclosure has another
`sparkle:os`. A missing `sparkle:os` means macOS to Sparkle but Linux to LinSparkle, so the
release job marks the DMG's enclosure `macos` and adds an item marked `linux`.

**One update, two architectures.** LinSparkle doesn't know about architectures, so the Linux
enclosure is `Lightfast-<version>-linux.tar.gz`, holding both AppImages and a
`linsparkle-bundle.json` manifest that gives each the id of its architecture. The app runs in
bundle mode with its architecture as its id, so it installs its own AppImage, even if renamed,
and skips the other, which isn't installed next to it. The tarball is signed as a whole.

**Versions.** Like Sparkle, LinSparkle compares the build number: the `version` job's UTC
timestamp, which every platform's build of a release shares. The Linux build bakes it in from
`LIGHTFAST_BUILD`. LinSparkle shows the running app's version as that build number ("you have
20261114.103000"), since it uses one string for both; the offered update shows the release
version from `sparkle:shortVersionString`.

**Signing.** The release job signs the tarball with `SPARKLE_ED_PRIVATE_KEY`, the secret
`generate_appcast` uses, with openssl, and refuses to publish if the key doesn't match
`SUPublicEDKey`.

### Testing an update

`LIGHTFAST_FEED_URL` and `LIGHTFAST_PUBLIC_KEY` at build time replace the feed and key, so an
update can be tried against a local feed signed with a throwaway key:

```sh
openssl genpkey -algorithm ed25519 -out test.pem
pub="$(openssl pkey -in test.pem -pubout -outform DER | tail -c 32 | base64)"
export LIGHTFAST_FEED_URL=http://127.0.0.1:8765/appcast.xml LIGHTFAST_PUBLIC_KEY="$pub"
LIGHTFAST_BUILD=1 linux/build-appimage.sh build/old
LIGHTFAST_BUILD=2 linux/build-appimage.sh build/new
# Put build/new's AppImage in a tarball as the release job does, sign it with
#   openssl pkeyutl -sign -inkey test.pem -rawin -in <tarball> | base64 -w0
# add it to an appcast with linux/add-to-appcast.py, serve the directory on
# port 8765 and run build/old's AppImage.
```

## Publishing a release

Tagging publishes both platforms (see `docs/macos-release.md`). The release gets
`Lightfast-<arch>.AppImage` for first installs (stable links:
`https://github.com/simse/lightfast/releases/latest/download/Lightfast-x86_64.AppImage`) and
`Lightfast-<version>-linux.tar.gz` for updates. A Linux build failure fails the release, as a
macOS one does.

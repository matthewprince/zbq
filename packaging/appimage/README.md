# ZBQ AppImage

ZBQ's Linux release: one AppImage that runs on Ubuntu 22.04, Debian 12 and anything
newer (glibc 2.35 floor). It bundles Qt 6.9 with Qt WebEngine and every library the
app links that a desktop system does not already have.

## Running it

- From the host it uses only what a desktop install has: OpenGL/EGL, fontconfig, the X11
  or Wayland client libraries, GLib, D-Bus, the ALSA library and GTK 3.
- PipeWire for bit-perfect Qobuz playback (ZBQ sets it to each track's rate). Local files
  can also play through ALSA directly.
- Firefox, required for Qobuz playback: the installed one, else ZBQ asks to download
  Firefox ESR from Mozilla.
- Without FUSE (in a container, say): `--appimage-extract-and-run`.

## Building

```bash
packaging/appimage/build.sh [out-dir]          # default: dist/
```

Needs Docker and a Qt 6.9.3 SDK with Qt WebEngine on the host (aqtinstall layout:
modules qtwebengine qtwebchannel qtpositioning qtshadertools qtimageformats
qtwaylandcompositor; `ZBQ_QT` points at its `gcc_64`). The build:

1. Exports the committed tree (`git archive HEAD`), so uncommitted changes never ship.
2. Builds in an Ubuntu 22.04 container (`Dockerfile`), without network, against the
   mounted Qt.
3. Checks the binary's glibc floor (2.35); deploys Qt, Qt WebEngine and the QML modules
   with linuxdeploy and its Qt plugin; adds the icons, AppStream metadata, licences and
   Qt's SPDX bills of materials; packs with appimagetool and the static type2 runtime.
4. Writes `ZBQ-<version>-x86_64.AppImage` and its `.sha256`.

## Testing

```bash
packaging/appimage/smoke-distros.sh dist/ZBQ-<version>-x86_64.AppImage
```

Starts the AppImage offscreen on clean Ubuntu 22.04, Ubuntu 24.04, Debian 12, Fedora 41
and Arch containers (desktop libraries only, no Qt or build tools). It must initialize,
ask about Firefox (none is installed there; `ZBQ_FIREFOX_AUTO_INSTALL=1` downloads
Firefox ESR instead) and stay up for the observation window. No account, no audio.
`DISTROS="debian:12 fedora:41"` picks a subset.

## Releasing

```bash
ZBQ_SIGNING_KEY=<minisign secret key> packaging/appimage/sign-release.sh dist/ZBQ-<version>-x86_64.AppImage
```

Signs the AppImage for the in-app updater and writes `latest.json`. Upload the AppImage,
its `.sha256` and `latest.json` to the GitHub release tagged `v<version>`. The updater
reads only that tag's files and accepts only signatures from the key in
`crates/qbz-updater/src/install.rs`.

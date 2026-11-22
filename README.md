<p align="center">
  <img src="crates/qbz-qt/qml/assets/zbq-logo.svg" alt="ZBQ" width="140" />
</p>

# ZBQ

> **ZBQ is in beta.** It's new, so expect bugs. If something goes wrong, please report it:
> **[Report a bug](https://qobuzify.app/zbq/report)** (a short form, no account needed) or
> [open a GitHub issue](https://github.com/matthewprince/zbq/issues/new?template=bug_report.yml).

ZBQ is a hi-fi music player for Linux: your Qobuz subscription, your local
library and your media servers (Plex, Jellyfin, Subsonic/Navidrome) through one
interface and one audio pipeline. Bit-perfect output (ALSA Direct, or PipeWire
switched to each track's rate), gapless playback, a DAC setup wizard, DSD and
SACD playback, lyrics, scrobbling and more.

ZBQ is a fork of [QBZ](NOTICE.md). It keeps what QBZ could legally offer and
removes what it could not; [COMPLIANCE.md](COMPLIANCE.md) has the details.

## Install

Download `ZBQ-<version>-x86_64.AppImage` from the
[latest release](https://github.com/matthewprince/zbq/releases/latest), then:

```bash
chmod +x ZBQ-*-x86_64.AppImage
./ZBQ-*-x86_64.AppImage
```

* Runs on 64-bit x86 Linux with glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora,
  Arch and anything newer. Tested on clean installs of each.
* Needs FUSE to mount itself, which desktop distributions have. Where it is missing (a
  container, say), run it with `--appimage-extract-and-run`.
* Uses your installed Firefox for Qobuz playback, or downloads Firefox ESR from Mozilla
  on first use (checked against the SHA-512 Mozilla publishes).
* Check the download with `sha256sum -c ZBQ-<version>-x86_64.AppImage.sha256`.
* ZBQ checks this repository's releases for updates: menu > Check for updates now.

## Qobuz in ZBQ

* You sign in on Qobuz's own sign-in page.
* Qobuz's official web player runs hidden and does everything Qobuz protects:
  fetching, signing and decrypting. ZBQ never holds a Qobuz secret, key or stream
  link.
* With Firefox installed, Qobuz plays bit-perfect and gapless: Qobuz's player runs in a
  hidden Firefox, which keeps each file's own sample rate, and ZBQ sets PipeWire to it.
  Signing in opens Qobuz's own page in a normal Firefox window. Without Firefox (or with
  `ZBQ_QOBUZ_AUDIO=web`) Qobuz's player runs inside ZBQ and its audio is not
  bit-perfect.
* Not available: offline Qobuz downloads, Qobuz Connect, casting Qobuz tracks.
  Purchases are listed; download them from qobuz.com and add the folder to your
  library.

ZBQ requires a Qobuz account for Qobuz playback. It is not affiliated with,
endorsed by or certified by Qobuz, or by the QBZ authors.

## Lyrics

Lyrics come from the Qobuzify lyrics server first (word-synced for most tracks), then
Qobuz, LRCLIB and lyrics.ovh. A Qobuzify answer that arrives after another source was
shown replaces it. The lyrics panel and Immersive's split view draw them with
[Lyra](crates/qbz-qt/lyra/README.md), animated word by word; "Animated lyrics (Lyra)" in
the lyrics display settings switches back to the classic view, which also shows
translations.

## Building (Linux)

* Firefox for bit-perfect Qobuz playback at run time: the installed one, or Firefox
  ESR downloaded from Mozilla on first use (`ZBQ_FIREFOX` points at a specific binary).
* Qt 6.9.3 with Qt WebEngine, WebChannel and Positioning (for example through
  aqtinstall into `~/local/qt/6.9.3/gcc_64`), a recent Rust toolchain, and the
  usual audio development headers (ALSA, PipeWire).
* Build: point `QMAKE`, `CMAKE_PREFIX_PATH`, `LD_LIBRARY_PATH` and
  `QBZ_QT_QML_DIR` at that Qt, then run `NORUN=1 ./scripts/qt-run.sh`. It runs
  the QML audits first, then builds `crates/target/release/zbq`.
* Run: `./scripts/zbq-local.sh` (sets the Qt library path; `ZBQ_QT` overrides
  where Qt lives).
* Release AppImage: `packaging/appimage/build.sh` (Docker); see
  [packaging/appimage](packaging/appimage/README.md).

For bit-perfect output of local files and media servers, choose the ALSA backend with
your DAC's hardware device in Settings > Audio, keep ZBQ's volume at 100% (or enable
hardware volume), and limit the quality to what the device supports. Qobuz through
Firefox plays to the device chosen in Settings > Audio (or the system default) at each
track's own rate, and never asks Qobuz for more than that device plays. ZBQ's volume
control sets the device's own volume, so the stream stays bit-perfect wherever the
device has a hardware volume control. PipeWire is never set to a rate the device lacks:
such a track plays at the nearest rate the device has, resampled, and is shown as not
bit-perfect.

## Reporting bugs

ZBQ is in beta, and bug reports are the most useful thing you can send.

* **[Report a bug](https://qobuzify.app/zbq/report)**: a short form, no account needed.
* [GitHub issues](https://github.com/matthewprince/zbq/issues/new?template=bug_report.yml), if you have an account.
* Logs help most: in ZBQ, Settings > Developer > View logs > Upload (public), and include the link.

## Licence

MIT, like QBZ. See [LICENSE](LICENSE), [NOTICE.md](NOTICE.md) and
[licenses/](licenses/README.md) for the third-party components.

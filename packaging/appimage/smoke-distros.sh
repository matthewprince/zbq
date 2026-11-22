#!/usr/bin/env bash
# Start the AppImage offscreen on clean distro containers (desktop libraries
# only: no Qt, no build tools): it must initialize, start its Qobuz engine
# (Firefox ESR is downloaded inside each container) and survive the
# observation window. No account, no audio.
#   packaging/appimage/smoke-distros.sh dist/ZBQ-<version>-x86_64.AppImage
#   DISTROS="debian:12 fedora:41" ... to pick
set -uo pipefail
appimage="$(readlink -f "${1:?usage: smoke-distros.sh <AppImage>}")"
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
distros="${DISTROS:-ubuntu:22.04 ubuntu:24.04 debian:12 fedora:41 archlinux:latest}"
apt_common="python3 dbus ca-certificates libgl1 libegl1 libfontconfig1 fonts-dejavu-core libx11-6 libxcb1 libxkbcommon0 libx11-xcb1 libxtst6 libdbus-glib-1-2 tar xz-utils procps"
failed=0
for image in $distros; do
  case "$image" in
    ubuntu:24.04) setup="apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $apt_common libglib2.0-0t64 libasound2t64 libgtk-3-0t64" ;;
    ubuntu:*|debian:*) setup="apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $apt_common libglib2.0-0 libasound2 libgtk-3-0" ;;
    fedora:*) setup="dnf -y -q install python3 dbus-daemon dbus-tools mesa-libGL mesa-libEGL fontconfig dejavu-sans-fonts libX11 libxcb libxkbcommon libX11-xcb libXtst glib2 alsa-lib gtk3 dbus-glib tar xz procps-ng shadow-utils util-linux" ;;
    archlinux:*) setup="pacman -Syu --noconfirm --needed python dbus mesa fontconfig ttf-dejavu libx11 libxcb libxkbcommon libxtst glib2 alsa-lib gtk3 dbus-glib tar xz procps-ng" ;;
    *) echo "no setup recipe for $image" >&2; failed=1; continue ;;
  esac
  echo "== $image"
  if docker run --rm -v "$appimage:/app/ZBQ.AppImage:ro" -v "$root/scripts:/scripts:ro" "$image" bash -c "
      $setup >/tmp/setup.log 2>&1 || { tail -5 /tmp/setup.log; exit 3; }
      useradd -m tester
      su tester -c 'cd /tmp && APPIMAGE_EXTRACT_AND_RUN=1 python3 /scripts/packaging/smoke_release.py /app/ZBQ.AppImage --seconds 75 --log /tmp/smoke.log; rc=\$?; grep -E \"firefox-engine|qobuz-engine. ready|QbzCore initialized|smoke\" /tmp/smoke.log | cut -c1-170 | head -8; exit \$rc'
    "; then
    echo "PASS $image"
  else
    echo "FAIL $image"
    failed=1
  fi
done
exit "$failed"

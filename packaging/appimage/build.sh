#!/usr/bin/env bash
# Build ZBQ's Linux AppImage from the committed tree, in an Ubuntu 22.04
# container (glibc 2.35 floor), against a Qt 6.9 SDK with Qt WebEngine on the
# host (aqtinstall layout: modules qtwebengine qtwebchannel qtpositioning
# qtshadertools qtimageformats qtwaylandcompositor).
#   packaging/appimage/build.sh [out-dir]          (default: dist/)
#   ZBQ_QT=/path/to/6.9.3/gcc_64   ZBQ_RELEASE_CACHE=~/.cache/zbq-release
set -euo pipefail
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
qt="$(readlink -f "${ZBQ_QT:-$HOME/local/qt/6.9.3/gcc_64}")"
cache="${ZBQ_RELEASE_CACHE:-$HOME/.cache/zbq-release}"
out="$(mkdir -p "${1:-$root/dist}" && cd "${1:-$root/dist}" && pwd)"
[ -x "$qt/bin/qmake" ] || { echo "no Qt SDK at $qt (set ZBQ_QT)" >&2; exit 1; }
[ -d "$qt/lib" ] && ls "$qt"/lib/libQt6WebEngineCore.so* >/dev/null 2>&1 \
  || { echo "the Qt SDK at $qt has no Qt WebEngine" >&2; exit 1; }
if [ -n "$(git -C "$root" status --porcelain)" ]; then
  echo "note: uncommitted changes are NOT in the build (it uses the committed tree)" >&2
fi
mkdir -p "$cache/tools" "$cache/target" "$cache/src"

# The packaging tools, fetched once (the inherited CI used the same ones).
fetch() { [ -s "$cache/tools/$1" ] || curl -fsSL -o "$cache/tools/$1" "$2"; chmod +x "$cache/tools/$1"; }
fetch linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
fetch linuxdeploy-plugin-qt https://github.com/linuxdeploy/linuxdeploy-plugin-qt/releases/download/continuous/linuxdeploy-plugin-qt-x86_64.AppImage
fetch appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
fetch runtime-x86_64 https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
(cd "$cache/tools" && sha256sum linuxdeploy linuxdeploy-plugin-qt appimagetool runtime-x86_64)

rm -rf "$cache/src" && mkdir -p "$cache/src"
git -C "$root" archive HEAD | tar -x -C "$cache/src"
docker build -q -t zbq-build:22.04 "$root/packaging/appimage" >/dev/null
docker run --rm --user "$(id -u):$(id -g)" --memory 14g --memory-swap 14g --network none \
  -e JOBS="${JOBS:-6}" -e APPIMAGE_EXTRACT_AND_RUN=1 \
  -v "$cache/src:/src" -v "$qt:/qt:ro" -v "$cache/target:/target" -v "$out:/out" \
  -v "$cache/tools:/tools:ro" \
  -v "$HOME/.cargo/registry:/opt/cargo/registry" -v "$HOME/.cargo/git:/opt/cargo/git" \
  zbq-build:22.04 bash /src/packaging/appimage/build-in-container.sh

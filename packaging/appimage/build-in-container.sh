#!/usr/bin/env bash
# Runs inside the build image (see build.sh): builds zbq against the mounted Qt,
# deploys it with Qt and Qt WebEngine into an AppDir (linuxdeploy + its Qt
# plugin, the same tools the inherited CI used) and packs the AppImage.
#   /src     the committed source tree        /qt     Qt 6.9 SDK (read-only)
#   /target  Cargo target dir                 /tools  linuxdeploy, appimagetool, runtime
#   /out     where the AppImage goes
set -euo pipefail
export HOME=/tmp/home
mkdir -p "$HOME"
export QMAKE=/qt/bin/qmake CMAKE_PREFIX_PATH=/qt PATH="/qt/bin:$PATH" LD_LIBRARY_PATH=/qt/lib
export QBZ_QT_QML_DIR=/qt/qml CARGO_TARGET_DIR=/target CARGO_NET_OFFLINE=true CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${JOBS:-6}"
cd /src
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/Cargo.toml | head -1)
echo "[zbq-release] building ZBQ $version"
python3 scripts/qt-cargo.py build --release --manifest-path crates/Cargo.toml -p qbz-qt

work=/tmp/appimage
rm -rf "$work"
mkdir -p "$work"
strip -o "$work/zbq" /target/release/zbq
# The glibc this binary needs is the floor for every system it runs on.
floor=$(objdump -T "$work/zbq" | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -1)
echo "[zbq-release] glibc floor: $floor"
[ "$(printf '%s\nGLIBC_2.35\n' "$floor" | sort -V | tail -1)" = "GLIBC_2.35" ] \
  || { echo "glibc floor above 2.35: $floor" >&2; exit 1; }

# Deploy from a scratch Qt without the NMEA positioning plugin: it needs Qt
# SerialPort, which this SDK lacks, and a music player has no use for GPS. The
# libraries are only read, so they stay linked to the mounted SDK.
qt=/tmp/qt-deploy
rm -rf "$qt"
mkdir -p "$qt"
for d in /qt/*; do
  if [ "$(basename "$d")" = lib ]; then ln -s /qt/lib "$qt/lib"; else cp -a "$d" "$qt/"; fi
done
rm -f "$qt/plugins/position/libqtposition_nmea.so"
export QMAKE="$qt/bin/qmake" LD_LIBRARY_PATH="$qt/lib"

cd "$work"
cp /src/packaging/linux/lol.zbq.ZBQ.desktop .
cp /src/packaging/icons/zbq/256x256.png lol.zbq.ZBQ.png
# The Qt plugin copies the QML modules the app imports (QML_SOURCES_PATHS) and,
# for Qt WebEngine, QtWebEngineProcess with its resources and locales. Wayland
# and offscreen need naming; xcb comes by default.
QML_SOURCES_PATHS=/src/crates/qbz-qt/qml \
EXTRA_PLATFORM_PLUGINS="libqoffscreen.so;libqwayland-generic.so;libqwayland-egl.so" \
EXTRA_QT_MODULES="waylandcompositor" \
  /tools/linuxdeploy --appimage-extract-and-run --appdir AppDir \
    -e zbq -d lol.zbq.ZBQ.desktop -i lol.zbq.ZBQ.png --plugin qt
# Nothing in the app uses Qt SQL or network-information plugins.
rm -rf AppDir/usr/plugins/sqldrivers AppDir/usr/plugins/networkinformation
for size in 16 24 32 48 64 128 256 512; do
  install -Dm644 "/src/packaging/icons/zbq/${size}x${size}.png" \
    "AppDir/usr/share/icons/hicolor/${size}x${size}/apps/lol.zbq.ZBQ.png"
done
install -Dm644 /src/packaging/linux/lol.zbq.ZBQ.metainfo.xml AppDir/usr/share/metainfo/lol.zbq.ZBQ.metainfo.xml
lic=AppDir/usr/share/licenses/zbq
mkdir -p "$lic"
cp /src/LICENSE /src/NOTICE.md /src/COMPLIANCE.md "$lic/"
cp -r /src/licenses "$lic/third-party"
# Qt (LGPL v3; text in third-party/LGPL-3.0.txt) and Qt WebEngine's Chromium
# components: the SPDX bills of materials the Qt SDK ships, license texts included.
mkdir -p "$lic/qt-sbom"
find /qt/sbom -maxdepth 1 -name '*.spdx' ! -name '*.source.spdx' -exec cp {} "$lic/qt-sbom/" \;
test -f "$lic/qt-sbom/qtwebengine-chromium-webengine-6.9.3.spdx"

test -x AppDir/usr/bin/zbq
test -f AppDir/usr/bin/qt.conf
test -f AppDir/usr/plugins/platforms/libqxcb.so
test -f AppDir/usr/plugins/platforms/libqwayland-generic.so
test -f AppDir/usr/plugins/platforms/libqoffscreen.so
test -x AppDir/usr/libexec/QtWebEngineProcess
test -f AppDir/usr/resources/qtwebengine_resources.pak
test -f AppDir/usr/qml/QtWebEngine/qmldir
du -sh AppDir

out="/out/ZBQ-${version}-x86_64.AppImage"
ARCH=x86_64 /tools/appimagetool --appimage-extract-and-run --no-appstream \
  --runtime-file /tools/runtime-x86_64 AppDir "$out"
(cd /out && sha256sum "$(basename "$out")" > "$(basename "$out").sha256")
ls -la "$out"

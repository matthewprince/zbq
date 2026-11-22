#!/usr/bin/env bash
# LyraLyricsView in a real Qt WebEngine view (offscreen), driven by
# scripts/qml-tests/lyra_view_check.qml. The page is assembled exactly as
# lyrics_qt::lyra_page does: lyra-host.html with lyra.js and lyra-zbq.js inlined.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/.."
if [[ -n "${QT_ROOT_DIR:-}" ]]; then
  qt_lyra_bins="$QT_ROOT_DIR/bin"
else
  qt_lyra_qmake="${QMAKE:-}"
  if [[ -z "$qt_lyra_qmake" ]]; then
    qt_lyra_qmake="$(command -v qmake6 || command -v qmake)"
  fi
  qt_lyra_bins="$("$qt_lyra_qmake" -query QT_INSTALL_BINS)"
fi
work="$(mktemp -d "${TMPDIR:-/tmp}/zbq-lyra-test-XXXXXX")"
trap 'rm -rf "$work"' EXIT
python3 - "$work/page.html" <<'EOF'
import pathlib, sys
lyra = pathlib.Path("crates/qbz-qt/lyra")
page = (lyra / "lyra-host.html").read_text()
page = page.replace("/*LYRA_JS*/", (lyra / "lyra.js").read_text(), 1)
page = page.replace("/*ZBQ_JS*/", (lyra / "lyra-zbq.js").read_text(), 1)
pathlib.Path(sys.argv[1]).write_text(page)
EOF
QT_QPA_PLATFORM=offscreen QML_XHR_ALLOW_FILE_READ=1 QTWEBENGINE_DISABLE_SANDBOX=1 \
  XDG_CACHE_HOME="$work/cache" XDG_DATA_HOME="$work/data" XDG_CONFIG_HOME="$work/config" \
  timeout 60 "$qt_lyra_bins/qml" -I scripts/qml-tests/imports scripts/qml-tests/lyra_view_check.qml -- \
  "$work/page.html" "$PWD/scripts/qml-tests/fixtures/lyra-sample.json" 2>&1 \
  | grep -E '^qml: (ok |FAIL|Lyra view)|ReferenceError|TypeError|is not a type|failed to load' \
  | sed 's/^qml: //' | tee "$work/out.txt"
grep -q '^Lyra view: all checks passed' "$work/out.txt"

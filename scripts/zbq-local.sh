#!/usr/bin/env bash
# Run a locally built ZBQ (crates/target/release/zbq) against the Qt SDK it was
# built with (aqtinstall layout). ZBQ_QT overrides the Qt location.
set -euo pipefail
here="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
qt="${ZBQ_QT:-$HOME/local/qt/6.9.3/gcc_64}"
bin="$here/crates/target/release/zbq"
[[ -x "$bin" ]] || { echo "zbq is not built yet: $bin" >&2; exit 1; }
export LD_LIBRARY_PATH="$qt/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$bin" "$@"

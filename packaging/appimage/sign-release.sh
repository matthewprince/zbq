#!/usr/bin/env bash
# Signs a release AppImage for the in-app updater and writes the latest.json
# manifest it reads. Upload the AppImage, its .sha256 and latest.json to the
# GitHub release tagged v<version>.
#   ZBQ_SIGNING_KEY=<minisign secret key> packaging/appimage/sign-release.sh dist/ZBQ-<version>-x86_64.AppImage
# The public half is PUBLIC_KEY in crates/qbz-updater/src/install.rs; the
# signature is checked against it before the manifest is written.
set -euo pipefail
appimage="$(readlink -f "${1:?usage: sign-release.sh <ZBQ-version-x86_64.AppImage>}")"
key="${ZBQ_SIGNING_KEY:?set ZBQ_SIGNING_KEY to the minisign secret key}"
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
name="$(basename "$appimage")"
version="$(sed -nE 's/^ZBQ-([0-9]+\.[0-9]+\.[0-9]+)-x86_64\.AppImage$/\1/p' <<<"$name")"
[ -n "$version" ] || { echo "not a ZBQ release AppImage: $name" >&2; exit 1; }
built="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/crates/Cargo.toml" | head -1)"
[ "$version" = "$built" ] || { echo "$name does not match the source version $built" >&2; exit 1; }
public="$(sed -nE 's/^const PUBLIC_KEY: &str = "([^"]+)";/\1/p' "$root/crates/qbz-updater/src/install.rs")"
[ -n "$public" ] || { echo "PUBLIC_KEY not found in qbz-updater" >&2; exit 1; }

minisign -S -s "$key" -m "$appimage" -x "$appimage.minisig" -t "file:$name" >/dev/null
minisign -V -q -P "$public" -m "$appimage" -x "$appimage.minisig"
url="https://github.com/matthewprince/zbq/releases/download/v$version/$name"
printf '{"version":"%s","platforms":{"linux-x86_64":{"url":"%s","signature":"%s"}}}\n' \
  "$version" "$url" "$(base64 -w0 "$appimage.minisig")" > "$(dirname "$appimage")/latest.json"
rm -f "$appimage.minisig"
echo "signed $name; wrote latest.json for v$version"

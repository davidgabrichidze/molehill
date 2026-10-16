#!/bin/sh
# Builds Molehill.app from a release build and signs it ad hoc.
#
#   packaging/macos/make-app.sh <target-triple> <out-dir>
#   e.g. packaging/macos/make-app.sh aarch64-apple-darwin dist
#
# Needs: cargo build --release --target <target-triple> done first; macOS
# (iconutil, codesign).
set -eu

target=$1
out=$2
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
bin="$root/target/$target/release/molehill"
[ -x "$bin" ] || { echo "missing $bin; build it first" >&2; exit 1; }

app="$out/Molehill.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/molehill"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

iconset=$(mktemp -d)/Molehill.iconset
(cd "$root" && cargo run --quiet --release -p molehill --example appicon -- "$iconset" >/dev/null)
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Molehill.icns"

# Ad hoc: no Developer ID, so Gatekeeper asks once ("Open Anyway").
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"
echo "$app ($version, $target)"

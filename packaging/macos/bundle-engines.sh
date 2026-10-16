#!/bin/sh
# Bundles OpenVPN and OpenConnect from Homebrew into one relocatable folder
# for the .pkg (installed at /opt/molehill/engines). Run on a Mac of the
# target architecture (Homebrew bottles are per architecture).
#
#   packaging/macos/bundle-engines.sh <out-dir>
#
# Layout is flat: the binaries, libopenconnect and every dylib they need sit
# side by side and find each other through @loader_path. Each Mach-O is then
# signed ad hoc again (rewriting install names breaks the signature, and
# Apple Silicon won't run unsigned code).
set -eu

out=$1
brew install --quiet openvpn openconnect dylibbundler >/dev/null
rm -rf "$out"
mkdir -p "$out/licenses"

cp "$(brew --prefix openvpn)/sbin/openvpn" "$out/"
cp "$(brew --prefix openconnect)/bin/openconnect" "$out/"
chmod u+w "$out/openvpn" "$out/openconnect"

# Copies every non-system dylib next to the binaries (libopenconnect too,
# as a dependency of openconnect) and points the references at it.
dylibbundler -of -b -x "$out/openvpn" -x "$out/openconnect" -d "$out" -p @loader_path/ >/dev/null
[ -f "$out/libopenconnect.5.dylib" ] || { echo "libopenconnect.5.dylib wasn't bundled" >&2; exit 1; }
for lib in "$out"/*.dylib; do
    chmod u+w "$lib"
    install_name_tool -id "@loader_path/$(basename "$lib")" "$lib" 2>/dev/null
done

# Nothing may still point into Homebrew (or anywhere else on the build Mac).
bad=0
for f in "$out"/*; do
    [ -f "$f" ] || continue
    file "$f" | grep -q Mach-O || continue
    codesign --force --sign - --timestamp=none "$f" 2>/dev/null
    otool -L "$f" | tail -n +2 | awk '{print $1}' | while read -r dep; do
        case "$dep" in
            @loader_path/*|/usr/lib/*|/System/*) ;;
            *) echo "$f still links $dep" >&2; echo bad ;;
        esac
    done | grep -q bad && bad=1
done
[ "$bad" = 0 ] || { echo "engines are not relocatable" >&2; exit 1; }

# Licenses of everything bundled, and where its source comes from.
formulae=$(brew deps --installed --union openvpn openconnect | tr '\n' ' ')
for f in openvpn openconnect $formulae; do
    keg=$(brew --prefix "$f")
    for lic in "$keg"/LICENSE* "$keg"/COPYING* "$keg"/COPYRIGHT*; do
        [ -f "$lic" ] && cp "$lic" "$out/licenses/$f-$(basename "$lic")"
    done
done
brew info --json=v2 openvpn openconnect $formulae |
    python3 -c 'import json,sys; [print(f["name"], f["versions"]["stable"], f["urls"]["stable"]["url"]) for f in json.load(sys.stdin)["formulae"]]' \
    > "$out/licenses/SOURCES.txt"

"$out/openvpn" --version | head -n 1
"$out/openconnect" --version 2>&1 | head -n 1

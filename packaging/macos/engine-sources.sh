#!/bin/sh
# Collects the source archives of every bundled engine and library (OpenVPN
# is GPLv2, OpenConnect and GnuTLS LGPL), as Homebrew built them, plus the
# list in SOURCES.txt, into one tar for the release.
#
#   packaging/macos/engine-sources.sh <out.tar>
set -eu
out=$1
work=$(mktemp -d)
for f in openvpn openconnect $(brew deps --installed --union openvpn openconnect); do
    brew fetch --quiet --build-from-source "$f" >/dev/null
    cp "$(brew --cache --build-from-source "$f")" "$work/"
done
brew info --json=v2 openvpn openconnect $(brew deps --installed --union openvpn openconnect) |
    python3 -c 'import json,sys; [print(f["name"], f["versions"]["stable"], f["urls"]["stable"]["url"]) for f in json.load(sys.stdin)["formulae"]]' \
    > "$work/SOURCES.txt"
tar -cf "$out" -C "$work" .
echo "$out"

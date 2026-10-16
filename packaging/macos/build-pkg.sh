#!/bin/sh
# Builds Molehill-<version>-<arch>.pkg.
#
#   packaging/macos/build-pkg.sh <target-triple> <engines-dir> <out-dir>
#
# Needs a release build for <target-triple>, Molehill.app from make-app.sh
# in <out-dir>, and the engines from bundle-engines.sh.
set -eu

target=$1
engines=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
root_dir=$(cd "$here/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root_dir/Cargo.toml" | head -n 1)
case "$target" in
    aarch64-apple-darwin) arch=arm64 ;;
    x86_64-apple-darwin) arch=x86_64 ;;
    *) echo "unknown target $target" >&2; exit 1 ;;
esac

work=$(mktemp -d)
root="$work/root"
mkdir -p "$root/Applications" "$root/opt/molehill/bin" "$root/opt/molehill/engines" \
    "$root/Library/LaunchDaemons" "$root/Library/LaunchAgents"
ditto "$out/Molehill.app" "$root/Applications/Molehill.app"
cp "$root_dir/target/$target/release/molehill-service" "$root/opt/molehill/bin/"
codesign --force --sign - --timestamp=none "$root/opt/molehill/bin/molehill-service"
ditto "$engines" "$root/opt/molehill/engines"
cp "$here/uninstall.sh" "$root/opt/molehill/"
cp "$here/io.github.giorgobiani.molehill.service.plist" "$root/Library/LaunchDaemons/"
cp "$here/io.github.giorgobiani.molehill.plist" "$root/Library/LaunchAgents/"
chmod 755 "$root/opt/molehill/uninstall.sh" "$here/scripts/preinstall" "$here/scripts/postinstall"

# Install the app where the package puts it, even if a copy was moved.
pkgbuild --analyze --root "$root" "$work/component.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$work/component.plist"

pkgbuild --root "$root" --component-plist "$work/component.plist" --scripts "$here/scripts" \
    --identifier io.github.giorgobiani.molehill --version "$version" --install-location / \
    "$work/molehill-component.pkg" >/dev/null
sed -e "s/@VERSION@/$version/g" -e "s/@ARCH@/$arch/g" "$here/distribution.xml" > "$work/distribution.xml"
pkg="$out/Molehill-$version-$arch.pkg"
productbuild --distribution "$work/distribution.xml" --package-path "$work" "$pkg" >/dev/null
shasum -a 256 "$pkg" | awk '{print $1}' > "$pkg.sha256"
echo "$pkg"

#!/usr/bin/env bash
# Build Trove.app and the DMG from already-built binaries — compiles nothing.
#
#   ./make-bundle.sh <version> <bin-dir>
#
# <bin-dir> holds the release binaries (trove-app, trove) built for the
# *macOS target matching this machine* (release.yml runs it on macos-14, once
# per architecture). The DMG lands in the current directory as
# Trove-<version>-<arch>.dmg, arch derived from uname (arm64 -> aarch64).
# Needs the macOS toolchain only: iconutil, codesign, hdiutil are all
# preinstalled.
#
# Signing, read this before "fixing" it: `codesign --sign -` is an *ad-hoc*
# signature, not Developer ID signing. It ships no identity and costs no
# certificate, and it is not optional — Apple Silicon refuses to execute a
# completely unsigned binary at all. Gatekeeper still stands in front of a
# downloaded DMG either way (README documents the two ways past it: right-
# click -> Open, or xattr -cr). Real signing + notarization is a deliberate
# non-goal until there are certificates to sign with (docs/GAP-TO-SERPENT.md
# §F). Inner binaries are signed before the bundle: the bundle's seal covers
# them, and an unsigned nested executable would break that seal on arm64.
set -euo pipefail

version=${1:?usage: make-bundle.sh <version> <bin-dir>}
bindir=${2:?usage: make-bundle.sh <version> <bin-dir>}
[[ -x $bindir/trove-app && -x $bindir/trove ]] || {
    echo "bin-dir must contain the built trove-app and trove binaries" >&2
    exit 1
}
for tool in iconutil codesign hdiutil; do
    command -v "$tool" >/dev/null || {
        echo "$tool not found — this script needs macOS (release.yml runs it on macos-14)" >&2
        exit 1
    }
done

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

app=Trove.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$bindir/trove-app" "$app/Contents/MacOS/trove-app"
install -m 755 "$bindir/trove" "$app/Contents/MacOS/trove"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

# The iconset layout is dictated by iconutil: fixed names, one file per slot
# the design set provides (16..256). Slots beyond that are simply absent —
# iconutil accepts a subset, and a soft upscale would buy nothing on a Retina
# dock at sizes no launcher renders.
iconset=$stage/trove.iconset
mkdir -p "$iconset"
cp "$root/design/icon/trove-16.png"  "$iconset/icon_16x16.png"
cp "$root/design/icon/trove-32.png"  "$iconset/icon_16x16@2x.png"
cp "$root/design/icon/trove-32.png"  "$iconset/icon_32x32.png"
cp "$root/design/icon/trove-64.png"  "$iconset/icon_32x32@2x.png"
cp "$root/design/icon/trove-128.png" "$iconset/icon_128x128.png"
cp "$root/design/icon/trove-256.png" "$iconset/icon_128x128@2x.png"
cp "$root/design/icon/trove-256.png" "$iconset/icon_256x256.png"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/trove.icns"

# One pass with --deep: the bundle holds a second executable beside its main
# one (the CLI), and a manual sign-inner-then-outer order — which is what the
# first two release runs tried — still had the x86_64 job's seal refuse the
# freshly signed sibling with "code object is not signed at all / In
# subcomponent" while the arm64 job signed the identical script fine. --deep
# signs nested code bottom-up in the same pass as the outer seal, which is
# the shape that survives a cross-arch signing host. --force because a
# rebuild can land a binary that already carries an ad-hoc signature.
codesign --force --deep --sign - "$app"

# UDZO = compressed read-only image, the shape a download wants.
#
# The image is named for the *binaries'* architecture, read from the bin-dir
# the workflow passes — never from `uname -m`: a cross-compiled x86_64 job
# runs on an arm64 runner, and host-derived naming shipped two different
# builds under one name on the release page (the v0.5.2 fourth run's one
# surviving aarch64.dmg was whoever uploaded last).
case "$bindir" in
    *aarch64*) arch=aarch64 ;;
    *x86_64*) arch=x86_64 ;;
    *) arch=$(uname -m | sed 's/^arm64$/aarch64/') ;;
esac
hdiutil create -volname "Trove $version" -srcfolder "$app" \
    -ov -format UDZO "Trove-$version-$arch.dmg"

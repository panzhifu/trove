#!/usr/bin/env bash
# Build the .deb from already-built binaries — this script compiles nothing.
#
#   ./build-deb.sh <version> <bin-dir>
#
# <bin-dir> holds the release binaries (trove-app, trove) built for
# x86_64-unknown-linux-gnu. The package lands in the current directory as
# trove_<version>_amd64.deb. Needs dpkg-deb (preinstalled on Debian/Ubuntu;
# release.yml runs this on ubuntu-24.04).
#
# Two facts here are load-bearing, not conventional:
#
# - /usr/bin/trove-app is a contract, not a habit: packaging/linux/trove.desktop
#   declares Exec=/usr/bin/trove-app and KWin authorizes the screenshot D-Bus
#   interface by matching /proc/<pid>/exe against that value. The binary name
#   itself is part of it — do not rename either side alone.
#
# - The Depends list mirrors release.yml's build-time apt list, where every
#   library is annotated with the crate that links it. If the link graph
#   changes, re-verify with `ldd target/<target>/release/trove-app` and move
#   both lists together. libc6 (>= 2.39) is the glibc floor of the
#   ubuntu-24.04 build (xcap -> libspa needs pipewire headers >= 0.3.65, which
#   is also why the binary cannot be built on 22.04) — Ubuntu 24.04+ /
#   Debian 13+.
set -euo pipefail

version=${1:?usage: build-deb.sh <version> <bin-dir>}
bindir=${2:?usage: build-deb.sh <version> <bin-dir>}
[[ -x $bindir/trove-app && -x $bindir/trove ]] || {
    echo "bin-dir must contain the built trove-app and trove binaries" >&2
    exit 1
}
command -v dpkg-deb >/dev/null || {
    echo "dpkg-deb not found — this path is built on Debian/Ubuntu (release.yml)" >&2
    exit 1
}

root=$(cd "$(dirname "$0")/../.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

install -Dm755 "$bindir/trove-app" "$stage/usr/bin/trove-app"
install -Dm755 "$bindir/trove" "$stage/usr/bin/trove"
install -Dm644 "$root/packaging/linux/trove.desktop" \
    "$stage/usr/share/applications/trove.desktop"
# Wayland takes the window icon from the .desktop entry matched by app_id
# ("trove"); the hicolor sizes are what launchers and the tray actually ask
# for. The X11 window icon is embedded in the binary and needs none of this.
for size in 16 32 48 64 128 256; do
    install -Dm644 "$root/design/icon/trove-$size.png" \
        "$stage/usr/share/icons/hicolor/${size}x${size}/apps/trove.png"
done
# A package without a copyright file makes no claim about redistribution.
install -Dm644 "$root/LICENSE" "$stage/usr/share/doc/trove/copyright"

mkdir -p "$stage/DEBIAN"
# Recommends, not Depends: every external tool degrades gracefully — without
# ffmpeg video keeps its kind icon and loses playback, without heif-dec the
# HEIC/AVIF formats do, without a PDF rasterizer PDFs do. The alternatives
# chain is deliberate: any one of the three rasterizers serves PDFs.
cat > "$stage/DEBIAN/control" <<EOF
Package: trove
Version: $version
Section: graphics
Priority: optional
Architecture: amd64
Maintainer: panzhifu <noke601508@outlook.com>
Depends: libc6 (>= 2.39), libasound2t64, libdrm2, libegl1, libfontconfig1, libgbm1, libgl1, libpipewire-0.3-0, libwayland-client0, libwayland-cursor0, libwayland-egl1, libxcb1, libxkbcommon-x11-0, libxkbcommon0, libzstd1
Recommends: ffmpeg, libheif-examples, poppler-utils | mupdf-tools | ghostscript
Homepage: https://github.com/panzhifu/trove
Description: local-first asset library for images, video, fonts, 3D models
 Trove indexes a folder of files into a local library with previews,
 justified-grid browsing, full-text and visual search, tags, collections
 and an inspector — entirely on the user's machine.
 .
 This package installs the desktop application (trove-app) and the
 command-line companion (trove), which share the same library.
EOF

dpkg-deb --build --root-owner-group "$stage" "trove_${version}_amd64.deb"

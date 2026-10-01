#!/usr/bin/env bash
# Build the .rpm from already-built binaries — this script compiles nothing.
#
#   ./build-rpm.sh <version> <bin-dir>
#
# <bin-dir> holds the release binaries (trove-app, trove) built for
# x86_64-unknown-linux-gnu. The package lands in the current directory as
# trove-<version>-1.x86_64.rpm. Needs rpmbuild: preinstalled on rpm distros,
# `apt install rpm` on the ubuntu runner (see release.yml), `pacman -S
# rpm-tools` on Arch.
#
# The spec's @VERSION@ placeholder is filled into a temp copy rather than
# passed through --define: header fields are expanded by the rpmbuild of the
# day, and Version: is the one field every downstream tool (dnf, yum,
# copr) parses literally.
set -euo pipefail

version=${1:?usage: build-rpm.sh <version> <bin-dir>}
bindir=${2:?usage: build-rpm.sh <version> <bin-dir>}
[[ -x $bindir/trove-app && -x $bindir/trove ]] || {
    echo "bin-dir must contain the built trove-app and trove binaries" >&2
    exit 1
}
command -v rpmbuild >/dev/null || {
    echo "rpmbuild not found — apt install rpm / pacman -S rpm-tools / dnf install rpm-build" >&2
    exit 1
}

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

sed "s/@VERSION@/$version/g" "$here/trove.spec" > "$stage/trove.spec"
mkdir -p "$stage"/{BUILD,RPMS,SOURCES,SRPMS,SPECS} "$stage/rpmdb"

# _dbpath keeps rpm's package database inside the stage: rpmbuild opens it at
# startup even for a pure file-repack (-bb of prebuilt binaries), and the
# system database must not be a precondition — it does not exist on Arch, and
# a bare `apt install rpm` on the ubuntu image may leave it uninitialized.
rpmbuild --quiet -bb \
    --define "_topdir $stage" \
    --define "_dbpath $stage/rpmdb" \
    --define "bindir $(cd "$bindir" && pwd)" \
    --define "pkgdir $here" \
    --define "icondir $root/design/icon" \
    --define "licfile $root/LICENSE" \
    "$stage/trove.spec"

# The filename between "-1" and ".x86_64" carries %{?dist} only on distros
# that define it (fc40 …); on a bare rpmbuild — Arch, the ubuntu CI image —
# it is empty, so the glob tolerates both shapes.
mv "$stage"/RPMS/x86_64/trove-"$version"-1*.x86_64.rpm .

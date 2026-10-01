# RPM spec for the prebuilt Trove binaries — rpmbuild compiles nothing here.
#
# Built through ./build-rpm.sh, which seds @VERSION@ into a temp copy of this
# file (header fields do not take --define macros cleanly across rpm
# versions) and points %install at the directory holding the already-built
# binaries.
#
# The binary lands at /usr/bin/trove-app on purpose, same contract as the
# .deb: packaging/linux/trove.desktop declares Exec=/usr/bin/trove-app and
# KWin authorizes the screenshot D-Bus interface by matching
# /proc/<pid>/exe against it.
Name:           trove
Version:        @VERSION@
Release:        1%{?dist}
Summary:        Local-first asset library for images, video, fonts, 3D models

License:        BUSL-1.1
URL:            https://github.com/panzhifu/trove
BuildArch:      x86_64

# The glibc floor of the ubuntu-24.04 build (xcap -> libspa pins the
# toolchain there): Ubuntu 24.04+ / Debian 13+ / Fedora 40+ all clear it.
# Everything else the binary links is near-universal, and the exact sonames
# are resolved automatically by rpm's find-requires on the packaged files —
# distro-specific *package* names for the optional tools go to Suggests, so
# one spec serves Fedora and openSUSE alike.
Requires:       glibc >= 2.39
Suggests:       ffmpeg
Suggests:       libheif-tools
Suggests:       poppler-utils

%description
Trove indexes a folder of files into a local library with previews,
justified-grid browsing, full-text and visual search, tags, collections
and an inspector — entirely on the user's machine. This package installs
the desktop application (trove-app) and the command-line companion
(trove), which share the same library.

%prep
%build

%install
mkdir -p %{buildroot}/usr/bin \
         %{buildroot}/usr/share/applications \
         %{buildroot}/usr/share/licenses/trove
for size in 16 32 48 64 128 256; do
    dir=%{buildroot}/usr/share/icons/hicolor/${size}x${size}/apps
    mkdir -p "$dir"
    install -m 644 %{icondir}/trove-$size.png "$dir"/trove.png
done
install -m 755 %{bindir}/trove-app %{buildroot}/usr/bin/trove-app
install -m 755 %{bindir}/trove     %{buildroot}/usr/bin/trove
install -m 644 %{pkgdir}/trove.desktop %{buildroot}/usr/share/applications/trove.desktop
install -m 644 %{licfile} %{buildroot}/usr/share/licenses/trove/LICENSE

%files
/usr/bin/trove-app
/usr/bin/trove
/usr/share/applications/trove.desktop
/usr/share/licenses/trove/LICENSE
/usr/share/icons/hicolor/16x16/apps/trove.png
/usr/share/icons/hicolor/32x32/apps/trove.png
/usr/share/icons/hicolor/48x48/apps/trove.png
/usr/share/icons/hicolor/64x64/apps/trove.png
/usr/share/icons/hicolor/128x128/apps/trove.png
/usr/share/icons/hicolor/256x256/apps/trove.png

%changelog
* Thu Oct 01 2026 panzhifu <noke601508@outlook.com> - @VERSION@-1
- Prebuilt-binary packaging (see packaging/README.md).

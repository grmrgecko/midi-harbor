#!/bin/sh
# Wraps a built Linux binary into an AppImage.
#
#   packaging/linux/appimage.sh <binary> <amd64|arm64> <version> <output directory>
#
# The AppImage is Midi-Harbor-<version>-<x86_64|aarch64>.AppImage in the output directory. It
# needs patchelf, appimagetool, the AppImage runtime for the target in
# /usr/local/share/appimage/runtime-<x86_64|aarch64>, and the Debian 12 sysroot the binary was
# linked against in /sysroot/linux_<arch>; the image packaging/cross/Dockerfile makes has the
# tools, and GoReleaser runs it there with the sysroots mounted. MIDI_HARBOR_SYSROOT names another
# directory holding the sysroots.
#
# Started with no arguments the AppImage opens the graphical interface, and `service install` run
# from it registers the AppImage file itself, so systemd mounts it again at every start
# (specs/017-appimage).
#
# It bundles only the Avahi client libraries, which a desktop may lack. glibc, ALSA, D-Bus and
# libxkbcommon come from the host: a bundled libasound finds no sound cards, and the host's
# libxkbcommon-x11, which the interface loads while it runs, is built against the host's
# libxkbcommon (R-104).
set -eu

binary=$1
arch=$2
version=$3
out=$4
cd "$(dirname "$0")/../.."

case "$arch" in
    amd64) machine=x86_64 ;;
    arm64) machine=aarch64 ;;
    *)
        echo "appimage.sh: unsupported architecture $arch" >&2
        exit 1
        ;;
esac
sysroot="${MIDI_HARBOR_SYSROOT:-/sysroot}/linux_$arch"
libraries="$sysroot/usr/lib/$machine-linux-gnu"
runtime="/usr/local/share/appimage/runtime-$machine"
appdir="$out/appimage-$arch/AppDir"
image="$out/Midi-Harbor-$version-$machine.AppImage"
id=com.mrgeckosmedia.MidiHarbor

# Check the sysroot is new enough to carry the bundled libraries' license.
if [ ! -f "$sysroot/usr/share/doc/libavahi-client3/copyright" ]; then
    echo "appimage.sh: $sysroot has no Avahi license; run make build-sysroot" >&2
    exit 1
fi

# Assemble the tree. AppRun is what the runtime starts; it runs the binary under the name the
# desktop entry expects.
rm -rf "$appdir" "$image"
mkdir -p "$appdir/usr/bin" "$appdir/usr/lib" "$appdir/usr/share/applications" \
    "$appdir/usr/share/icons/hicolor/scalable/apps" "$appdir/usr/share/doc/midi-harbor" \
    "$appdir/usr/share/doc/libavahi-client3"
cp "$binary" "$appdir/usr/bin/midi-harbor"
chmod 0755 "$appdir/usr/bin/midi-harbor"
cp packaging/linux/AppRun "$appdir/AppRun"
chmod 0755 "$appdir/AppRun"

# Bundle the Avahi client libraries, found through the binary's RUNPATH rather than
# LD_LIBRARY_PATH, which would reach every command the daemon runs.
cp -L "$libraries/libavahi-client.so.3" "$libraries/libavahi-common.so.3" "$appdir/usr/lib/"
patchelf --set-rpath '$ORIGIN/../lib' "$appdir/usr/bin/midi-harbor"

# Desktop entry and icon, at the root where appimagetool looks and under usr/share where desktop
# integration tools do.
cp packaging/linux/$id.desktop "$appdir/$id.desktop"
cp packaging/linux/$id.desktop "$appdir/usr/share/applications/$id.desktop"
cp Icon.svg "$appdir/$id.svg"
cp Icon.svg "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg"

# Licenses and documentation.
cp LICENSE.txt "$appdir/usr/share/doc/midi-harbor/copyright"
cp docs/*.md "$appdir/usr/share/doc/midi-harbor/"
cp "$sysroot/usr/share/doc/libavahi-client3/copyright" "$appdir/usr/share/doc/libavahi-client3/"
cp "$sysroot/usr/share/common-licenses/LGPL-2.1" "$appdir/usr/share/doc/libavahi-client3/"

# Pack it behind the runtime for the target.
ARCH=$machine appimagetool --no-appstream --runtime-file "$runtime" "$appdir" "$image"
rm -rf "$out/appimage-$arch"

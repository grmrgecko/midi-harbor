#!/bin/sh
# Renders Icon.svg into the icon files each platform's packaging uses, which are checked in so that
# building needs none of these tools. Run it on a Mac after changing Icon.svg.
#
# Needs rsvg-convert and ImageMagick (brew install librsvg imagemagick), and iconutil, which ships
# with macOS. Linux installs Icon.svg itself. MenuBarIcon.svg is the anchor from Icon.svg alone,
# kept beside it by hand.
set -eu

cd "$(dirname "$0")/.."
work=target/icons
rm -rf "$work"
mkdir -p "$work/AppIcon.iconset"

# macOS app icon. Apple's grid draws the tile at 824 of 1024 pixels with the rest left clear, so
# the icon sits the same size as every other app's in the Dock and Finder.
for size in 16 32 128 256 512; do
    for scale in 1 2; do
        pixels=$((size * scale))
        tile=$((pixels * 824 / 1024))
        name=icon_${size}x${size}
        [ "$scale" = 2 ] && name=${name}@2x
        rsvg-convert -w "$tile" -h "$tile" Icon.svg -o "$work/tile.png"
        magick "$work/tile.png" -background none -gravity center -extent "${pixels}x${pixels}" \
            "$work/AppIcon.iconset/$name.png"
    done
done
iconutil --convert icns --output packaging/macos/AppIcon.icns "$work/AppIcon.iconset"

# Windows icon, embedded in the executable, which the taskbar shows too. The tile fills the whole
# square, as Windows icons do.
for pixels in 16 24 32 48 64 128 256; do
    rsvg-convert -w "$pixels" -h "$pixels" Icon.svg -o "$work/$pixels.png"
done
magick "$work/16.png" "$work/24.png" "$work/32.png" "$work/48.png" "$work/64.png" \
    "$work/128.png" "$work/256.png" packaging/windows/midi-harbor.ico

# The App Store build's menu bar item: the anchor alone, which macOS draws as a template image in
# the menu bar's own colour. Rendered at twice its 18-point size for Retina displays.
rsvg-convert -w 36 -h 36 packaging/macos/MenuBarIcon.svg -o packaging/macos/MenuBarIcon.png

echo packaging/macos/AppIcon.icns
echo packaging/windows/midi-harbor.ico
echo packaging/macos/MenuBarIcon.png

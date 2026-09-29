#!/bin/sh
# Wraps a built macOS binary into "Midi Harbor.app" and a disk image holding it.
#
#   packaging/macos/bundle.sh [--helper <headless binary>] <binary> <version> <output directory>
#                             [<disk image directory>]
#
# The app is built in the output directory, and the disk image goes beside it unless a directory
# is given for it; GoReleaser puts the image in dist/ with the other artifacts.
#
# The binary inside the bundle is the whole program: opened from Finder it shows the graphical
# interface, and `service install` run from it registers the bundled executable, so the daemon
# launchd starts carries the bundle's identity and its Bluetooth permission.
#
# On a Mac it signs with codesign and makes the image with hdiutil. It signs with the identity
# MIDI_HARBOR_SIGNING_IDENTITY names, or else the first Developer ID Application identity in the
# keychain. Elsewhere, which is how GoReleaser runs it, it signs with rcodesign and makes the image
# with xorriso and libdmg-hfsplus's dmg, signing with .signing/developer-id.p12 when it exists.
# Without a Developer ID certificate it signs ad hoc. With one, and an App Store Connect API key in
# .signing/notary-api-key.json, it notarizes the app and the disk image and staples both.
#
# --helper makes the App Store variant instead, on a Mac only: the sandboxed app, with the helper
# as the daemon it starts (Contents/MacOS/midi-harbor-daemon), requiring macOS 13 and starting
# without a Dock icon (specs/014-mac-app-store-mode/contracts/bundle.md). With a Mac App Store
# Connect provisioning profile in .signing/app-store.provisionprofile it is built for submission:
# the profile embedded, signed with the keychain's Apple Distribution identity, and wrapped in an
# installer package signed with its Mac Installer Distribution identity, the only form App Store
# Connect takes (specs/016-app-store-submission). Without one it is signed ad hoc, or with
# MIDI_HARBOR_SIGNING_IDENTITY, to run on this Mac.
set -eu

helper=
if [ "${1:-}" = --helper ]; then
    helper=$2
    shift 2
fi
binary=$1
version=$2
out=$3
images=${4:-$out}
cd "$(dirname "$0")/../.."
app="$out/Midi Harbor.app"
dmg="$images/Midi-Harbor-$version.dmg"
staging="$out/dmg-staging"
p12=.signing/developer-id.p12
p12_password=.signing/developer-id.p12.password
api_key=.signing/notary-api-key.json

# Assemble the bundle. The bundle's version must be plain numbers, so a snapshot's suffix is
# dropped there and kept in the image's name.
rm -rf "$app" "$dmg" "$staging"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$images"
cp "$binary" "$app/Contents/MacOS/midi-harbor"
chmod 0755 "$app/Contents/MacOS/midi-harbor"
cp packaging/macos/AppIcon.icns "$app/Contents/Resources/"
sed "s/@VERSION@/${version%%-*}/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"

# The App Store variant: the helper signed first, since signing the bundle seals it, then the
# app with the sandbox.
if [ -n "$helper" ]; then
    profile=.signing/app-store.provisionprofile
    entitlements=packaging/macos/app-store.entitlements
    cp "$helper" "$app/Contents/MacOS/midi-harbor-daemon"
    chmod 0755 "$app/Contents/MacOS/midi-harbor-daemon"
    plutil -replace LSMinimumSystemVersion -string 13.0 "$app/Contents/Info.plist"
    plutil -replace LSUIElement -bool true "$app/Contents/Info.plist"
    # Hashing and random numbers are all the encryption it has, which is exempt, so App Store
    # Connect need not ask at every upload.
    plutil -replace ITSAppUsesNonExemptEncryption -bool false "$app/Contents/Info.plist"

    # For submission: the identities, the profile, and the identifiers it grants.
    if [ -f "$profile" ]; then
        identity=${MIDI_HARBOR_SIGNING_IDENTITY:-$(security find-identity -v -p codesigning \
            | sed -n 's/^.*"\(Apple Distribution: .*\)"$/\1/p' | head -n 1)}
        installer=$(security find-identity -v \
            | sed -n 's/^.*"\(3rd Party Mac Developer Installer: .*\)"$/\1/p' | head -n 1)
        if [ -z "$identity" ] || [ -z "$installer" ]; then
            echo "$profile needs an Apple Distribution and a Mac Installer Distribution" \
                "certificate in the keychain; see packaging/README.md" >&2
            exit 1
        fi
        security cms -D -i "$profile" -o "$out/profile.plist"
        field() { /usr/libexec/PlistBuddy -c "Print :$1" "$out/profile.plist"; }
        team=$(field Entitlements:com.apple.developer.team-identifier)
        app_id=$(field Entitlements:com.apple.application-identifier)
        bundle_id=$(plutil -extract CFBundleIdentifier raw "$app/Contents/Info.plist")
        if [ "$app_id" != "$team.$bundle_id" ]; then
            echo "$profile is for $app_id, not $team.$bundle_id" >&2
            exit 1
        fi
        cp "$profile" "$app/Contents/embedded.provisionprofile"
        entitlements="$out/app-store.entitlements"
        cp packaging/macos/app-store.entitlements "$entitlements"
        /usr/libexec/PlistBuddy \
            -c "Add :com.apple.application-identifier string $app_id" \
            -c "Add :com.apple.developer.team-identifier string $team" "$entitlements"
        rm "$out/profile.plist"
        # Every upload needs a build number above the last, whatever the version, so it is the
        # time of the build.
        plutil -replace CFBundleVersion -string "$(date -u +%Y%m%d%H%M)" \
            "$app/Contents/Info.plist"
    else
        identity=${MIDI_HARBOR_SIGNING_IDENTITY:--}
    fi

    # Sign, with no extended attributes: App Store Connect refuses a package holding a file
    # marked with com.apple.quarantine, as a downloaded profile is (research R-103).
    xattr -cr "$app"
    plutil -lint "$app/Contents/Info.plist" >/dev/null
    codesign --force --options runtime --sign "$identity" \
        --entitlements packaging/macos/helper.entitlements \
        "$app/Contents/MacOS/midi-harbor-daemon"
    codesign --force --options runtime --sign "$identity" --entitlements "$entitlements" "$app"
    codesign --verify --strict "$app"
    echo "$app"

    # Package for App Store Connect.
    if [ -f "$profile" ]; then
        pkg="$out/Midi-Harbor-$version.pkg"
        productbuild --quiet --component "$app" /Applications --sign "$installer" "$pkg"
        echo "$pkg"
    fi
    exit 0
fi

# Find the Developer ID certificate. The App Store variant above takes its identity only from
# MIDI_HARBOR_SIGNING_IDENTITY, since a Developer ID one is the wrong kind for it.
developer_id=
if [ "$(uname -s)" = Darwin ]; then
    identity=${MIDI_HARBOR_SIGNING_IDENTITY:-}
    if [ -z "$identity" ]; then
        identity=$(security find-identity -v -p codesigning \
            | sed -n 's/^.*"\(Developer ID Application: .*\)"$/\1/p' | head -n 1)
    fi
    if [ -n "$identity" ] && [ "$identity" != - ]; then
        developer_id=yes
    fi
elif [ -f "$p12" ]; then
    if [ ! -f "$p12_password" ]; then
        echo "$p12 needs its password in $p12_password" >&2
        exit 1
    fi
    developer_id=yes
fi
notarize=
if [ -n "$developer_id" ] && [ -f "$api_key" ]; then
    notarize=yes
elif [ -f "$api_key" ]; then
    echo "not notarizing: $api_key needs a Developer ID certificate to sign with" >&2
fi
if [ -n "$developer_id" ]; then
    echo "signing with a Developer ID certificate" >&2
else
    echo "no Developer ID certificate found; signing ad hoc" >&2
fi

# sign <path> signs the app bundle, with the hardened runtime notarization requires, or the disk
# image, which carries no runtime flag. Without a Developer ID certificate the image is left
# unsigned, since an ad hoc signature on it proves nothing.
sign() {
    case "$1" in
        *.app) runtime=yes ;;
        *) runtime= ;;
    esac
    if [ -z "$developer_id" ]; then
        if [ -z "$runtime" ]; then
            return
        fi
        if [ "$(uname -s)" = Darwin ]; then
            codesign --force --options runtime --sign - "$1"
            codesign --verify --strict "$1"
        else
            rcodesign sign --code-signature-flags runtime "$1" >/dev/null
        fi
    elif [ "$(uname -s)" = Darwin ]; then
        codesign --force --timestamp ${runtime:+--options runtime} --sign "$identity" "$1"
        codesign --verify --strict "$1"
    else
        rcodesign sign --p12-file "$p12" --p12-password-file "$p12_password" \
            ${runtime:+--code-signature-flags runtime --for-notarization} "$1" >/dev/null
    fi
}

# notarize <path> submits the app bundle or disk image to Apple's notary service, waits for its
# verdict, and staples the ticket to it so it opens without a network connection.
notarize() {
    if [ "$(uname -s)" = Darwin ]; then
        # notarytool takes the key as a .p8 file and an app bundle only inside a zip.
        work=$(mktemp -d)
        {
            echo "-----BEGIN PRIVATE KEY-----"
            plutil -extract private_key raw "$api_key" | fold -w 64
            echo "-----END PRIVATE KEY-----"
        } > "$work/key.p8"
        submission=$1
        case "$1" in
            *.app)
                submission="$work/$(basename "$1").zip"
                ditto -c -k --keepParent "$1" "$submission"
                ;;
        esac
        status=0
        xcrun notarytool submit "$submission" --wait \
            --key "$work/key.p8" \
            --key-id "$(plutil -extract key_id raw "$api_key")" \
            --issuer "$(plutil -extract issuer_id raw "$api_key")" >&2 || status=$?
        rm -rf "$work"
        if [ "$status" -ne 0 ]; then
            return "$status"
        fi
        xcrun stapler staple "$1" >&2
    else
        # An hour rather than rcodesign's ten minutes: Apple holds a team's first submissions for
        # longer, and a release that stops waiting is left unnotarized.
        rcodesign notary-submit --api-key-file "$api_key" --max-wait-seconds 3600 --staple "$1" >&2
    fi
}

# Sign the app, and notarize it so the copy dragged out of the disk image carries its own ticket.
# Bluetooth permission is granted to the signed bundle, not to a loose binary.
if [ "$(uname -s)" = Darwin ]; then
    plutil -lint "$app/Contents/Info.plist" >/dev/null
fi
sign "$app"
if [ -n "$notarize" ]; then
    notarize "$app"
fi

# Put it on a disk image beside a link to Applications, the usual way to install one, with the
# license and documentation the archives carry.
mkdir -p "$staging/docs"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
cp LICENSE.txt "$staging/"
cp docs/*.md "$staging/docs/"
if [ "$(uname -s)" = Darwin ]; then
    hdiutil create -quiet -volname "Midi Harbor" -srcfolder "$staging" -ov -format UDZO "$dmg"
else
    xorriso -as mkisofs -quiet -V "Midi Harbor" -r -D -no-pad -o "$out/uncompressed.iso" "$staging"
    dmg dmg "$out/uncompressed.iso" "$dmg" >/dev/null
    rm -f "$out/uncompressed.iso"
fi
rm -rf "$staging"

# Sign and notarize the disk image itself, which is what Gatekeeper checks first on download.
sign "$dmg"
if [ -n "$notarize" ]; then
    notarize "$dmg"
fi

echo "$app"
echo "$dmg"

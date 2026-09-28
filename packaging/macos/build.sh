#!/bin/sh
# Builds "Midi Harbor.app" and a disk image holding it on a Mac, under target/package/macos.
# `make snapshot` and `make release` make the same through GoReleaser; this is for building one
# without Docker, or signed with a Developer ID certificate from the keychain.
#
#   packaging/macos/build.sh [--app-store]
#
# --app-store builds the sandboxed App Store variant instead, under target/package/macos-app-store:
# the full program as the app and the headless build as the daemon it starts
# (specs/014-mac-app-store-mode/contracts/bundle.md).
#
# Signs and notarizes as packaging/macos/bundle.sh describes. Builds universal binaries
# when the x86_64 target is installed (rustup target add x86_64-apple-darwin), and binaries for
# this Mac otherwise.
set -eu

cd "$(dirname "$0")/../.."
version=$(cat VERSION)
build=${CARGO_TARGET_DIR:-target}
universal=
if rustup target list --installed | grep -qx x86_64-apple-darwin \
    && rustup target list --installed | grep -qx aarch64-apple-darwin; then
    universal=yes
else
    echo "x86_64-apple-darwin is not installed; building for $(uname -m) only" >&2
fi

# build_binary <target directory> [cargo flags] builds midi-harbor into the target directory and
# prints the path of the binary to bundle, universal when both targets are installed.
build_binary() {
    dir=$1
    shift
    if [ -n "$universal" ]; then
        CARGO_TARGET_DIR="$dir" cargo build --release --target aarch64-apple-darwin "$@" >&2
        CARGO_TARGET_DIR="$dir" cargo build --release --target x86_64-apple-darwin "$@" >&2
        lipo -create -output "$dir/release/midi-harbor-universal" \
            "$dir/aarch64-apple-darwin/release/midi-harbor" \
            "$dir/x86_64-apple-darwin/release/midi-harbor"
        echo "$dir/release/midi-harbor-universal"
    else
        CARGO_TARGET_DIR="$dir" cargo build --release "$@" >&2
        echo "$dir/release/midi-harbor"
    fi
}

# Build.
binary=$(build_binary "$build")
if [ "${1:-}" = --app-store ]; then
    # The headless build goes to a target directory of its own, since it is the same package
    # built with other features and would otherwise overwrite the full binary.
    helper=$(build_binary "$build/headless" --no-default-features)
    out=target/package/macos-app-store
    rm -rf "$out"
    mkdir -p "$out"
    packaging/macos/bundle.sh --helper "$helper" "$binary" "$version" "$out"
    exit 0
fi

# Bundle.
rm -rf target/package/macos
mkdir -p target/package/macos
packaging/macos/bundle.sh "$binary" "$version" target/package/macos

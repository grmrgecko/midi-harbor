#!/usr/bin/env bash
# Builds the Linux sysroots the release is cross-compiled against, into target/sysroot/linux_amd64
# and target/sysroot/linux_arm64. See README.md.
set -euo pipefail

cd -P -- "$(dirname -- "$0")"
out=../../target/sysroot
rm -rf "$out"
mkdir -p "$out"

# One architecture at a time, which Docker's default builder can do.
for arch in amd64 arm64; do
    if ! docker buildx build --platform="linux/$arch" --output "type=local,dest=$out/linux_$arch" .; then
        echo "failed to build the $arch sysroot; see packaging/sysroot/README.md" >&2
        exit 1
    fi
    # Debian merged /lib into /usr/lib, and the linker looks in both.
    ln -sfn usr/lib "$out/linux_$arch/lib"
done

echo "sysroots built in target/sysroot"

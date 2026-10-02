# Tests, and release builds. Release builds run inside the image packaging/cross/Dockerfile makes,
# against the Linux sysroots packaging/sysroot/build.sh makes; see packaging/README.md.

CROSS_IMAGE  := midi-harbor-cross:latest
SYSROOT      := $(CURDIR)/target/sysroot
# The last file packaging/sysroot/build.sh makes, so a build it did not finish counts as missing.
SYSROOT_DONE := $(SYSROOT)/linux_arm64/lib
# Downloaded crates, kept between runs inside target/ so nothing else in the tree changes.
CARGO_CACHE  := $(CURDIR)/target/release-cargo-home
# The release build's own target directory, a Docker volume: building through a bind mount of the
# working tree's target/ stalled for good on Docker Desktop. Only dist/ comes back to the tree.
TARGET_VOLUME := midi-harbor-release-target
MIDI_HARBOR_MAINTAINER ?= $(shell git config user.name) <$(shell git config user.email)>
# The version every build step uses; a release is the commit tagged with it.
VERSION := $(shell cat VERSION)
# Identifies everything one release run builds, apart from every other build of the version.
BUILD_ID := $(shell uuidgen | tr '[:upper:]' '[:lower:]')

# GoReleaser runs one build at a time: its Rust builder reads each binary from target/, which
# the full and headless builds of one target share. Cargo still builds each in parallel.
# Docker creates a missing volume owned by root, so the volume is handed to the build's user
# first; otherwise a release after make clean cannot write to target/.
GORELEASER = mkdir -p $(CARGO_CACHE) && docker run --rm \
	--user 0:0 \
	--entrypoint chown \
	-v $(TARGET_VOLUME):/target \
	$(CROSS_IMAGE) \
	$(shell id -u):$(shell id -g) /target && docker run --rm \
	--user $(shell id -u):$(shell id -g) \
	-e CARGO_HOME=/cargo-home \
	-e HOME=/tmp \
	-e MIDI_HARBOR_MAINTAINER="$(MIDI_HARBOR_MAINTAINER)" \
	-e MIDI_HARBOR_VERSION="$(VERSION)" \
	-e MIDI_HARBOR_BUILD_ID="$(BUILD_ID)" \
	-v $(CURDIR):/src \
	-v $(SYSROOT):/sysroot:ro \
	-v $(CARGO_CACHE):/cargo-home \
	-v $(TARGET_VOLUME):/src/target \
	-w /src \
	$(1) \
	$(CROSS_IMAGE)

.PHONY: default
default: test

# Unit tests, beside the code.
.PHONY: test
test:
	cargo test --workspace --lib --bins

# Integration tests: the daemon, CLI and protocols as a whole, over real files and sockets.
.PHONY: test-integration
test-integration:
	cargo test --workspace --test '*'

# Tests that need CoreMIDI, ALSA, WinMM, a Bluetooth radio or a session peer on this machine.
.PHONY: test-live
test-live:
	cargo test --workspace --test '*' -- --ignored --test-threads 1

.PHONY: build-sysroot
build-sysroot:
	packaging/sysroot/build.sh

# A release builds the sysroots when they are missing. Docker would otherwise mount an empty
# directory in their place, and pkg-config would fail on the first Linux library it looks for.
$(SYSROOT_DONE):
	packaging/sysroot/build.sh

.PHONY: build-docker-image
build-docker-image:
	docker build -t $(CROSS_IMAGE) packaging/cross

# Builds every artifact into dist/ without publishing.
.PHONY: snapshot
snapshot: | $(SYSROOT_DONE)
	$(call GORELEASER,) release --clean --parallelism 1 --snapshot

# Publishes the commit tagged v$(VERSION). .release-env holds GITHUB_TOKEN. GoReleaser takes the
# version from the tag, so a tag that disagrees with VERSION is refused.
.PHONY: release
release: | $(SYSROOT_DONE)
	@test "$$(git rev-parse -q --verify 'refs/tags/v$(VERSION)^{commit}')" = "$$(git rev-parse HEAD)" \
		|| { echo "HEAD is not tagged v$(VERSION); tag it and push the tag: git tag v$(VERSION) && git push origin v$(VERSION)" >&2; exit 1; }
	$(call GORELEASER,--env-file .release-env) release --clean --parallelism 1

# Builds the Mac App Store variant on a Mac, into target/package/macos-app-store: an installer
# package for App Store Connect when .signing/app-store.provisionprofile exists, and an app signed
# to run on this Mac otherwise. See packaging/README.md.
.PHONY: appstore
appstore:
	packaging/macos/build.sh --app-store

# Removes every build output, the Linux sysroots under target/ included, which the next release
# builds again.
.PHONY: clean
clean:
	cargo clean
	cargo clean --manifest-path fuzz/Cargo.toml
	cargo clean --manifest-path scripts/midi2-bindings/Cargo.toml
	-docker volume rm -f $(TARGET_VOLUME)
	rm -rf dist

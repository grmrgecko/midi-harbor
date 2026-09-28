# GoReleaser with Rust added, for building every release artifact from one machine. The base image
# carries GoReleaser, zig and the macOS SDK; cargo-zigbuild links each target through zig.
ARG CROSS_VERSION=latest
FROM ghcr.io/gythialy/golang-cross:${CROSS_VERSION}

ARG RUST_VERSION=1.96

# libclang is needed because the Avahi bindings are generated with bindgen. protoc is vendored by
# the build script, so no protobuf package is required. mingw-w64's windres compiles the icon into
# the Windows executable, and preprocesses with mingw-w64's gcc.
RUN apt-get update; \
    apt-get --no-install-recommends -y -q install libclang-dev pkg-config gcc-mingw-w64-x86-64; \
    rm -rf /var/lib/apt/lists/*

# The toolchain is installed where a build running as the invoking user can read it. Downloaded
# crates go to CARGO_HOME, which the Makefile points at a cache inside target/.
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | CARGO_HOME=/usr/local/cargo sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain "${RUST_VERSION}" --component rustfmt,clippy \
            --target x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu,x86_64-apple-darwin,aarch64-apple-darwin,x86_64-pc-windows-gnu; \
    CARGO_HOME=/usr/local/cargo cargo install --locked cargo-zigbuild; \
    rm -rf /usr/local/cargo/registry /usr/local/cargo/git; \
    chmod -R a+rX /usr/local/rustup /usr/local/cargo

# GoReleaser's own edition makes no app bundles or disk images, so packaging/macos/bundle.sh builds
# them with these: rcodesign signs the bundle, xorriso lays it out as a disk, and libdmg-hfsplus's
# dmg compresses that into the image macOS opens. libdmg-hfsplus is Mozilla's fork, which Firefox's
# own Linux-built disk images come from.
ARG RCODESIGN_VERSION=0.29.0
ARG LIBDMG_COMMIT=ec239599c1f234a4e01ae3fe51214d0c77e5baa3
RUN apt-get update; \
    apt-get --no-install-recommends -y -q install xorriso cmake libbz2-dev liblzma-dev libssl-dev \
        zlib1g-dev; \
    rm -rf /var/lib/apt/lists/*; \
    arch=$(uname -m); \
    case "$arch" in \
        aarch64) sum=4af92c87ddf52f5f2d1258a3b4e56c7dcb8f1b2468df744976c5f139e031961f ;; \
        x86_64) sum=dbe85cedd8ee4217b64e9a0e4c2aef92ab8bcaaa41f20bde99781ff02e600002 ;; \
    esac; \
    name="apple-codesign-${RCODESIGN_VERSION}-${arch}-unknown-linux-musl"; \
    curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/rcodesign.tar.gz \
        "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${RCODESIGN_VERSION}/${name}.tar.gz"; \
    echo "$sum  /tmp/rcodesign.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/rcodesign.tar.gz -C /tmp; \
    install -m 0755 "/tmp/${name}/rcodesign" /usr/local/bin/rcodesign; \
    git clone -q https://github.com/mozilla/libdmg-hfsplus.git /tmp/libdmg; \
    git -C /tmp/libdmg checkout -q "${LIBDMG_COMMIT}"; \
    cmake -S /tmp/libdmg -B /tmp/libdmg/build -DCMAKE_BUILD_TYPE=Release; \
    cmake --build /tmp/libdmg/build --target dmg-bin -j "$(nproc)"; \
    install -m 0755 /tmp/libdmg/build/dmg/dmg /usr/local/bin/dmg; \
    rm -rf /tmp/rcodesign.tar.gz "/tmp/${name}" /tmp/libdmg

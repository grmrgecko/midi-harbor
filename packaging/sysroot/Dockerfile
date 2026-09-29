# The libraries Midi Harbor links against on Linux, for cross-compiling it. Built once per
# architecture by build.sh, and trimmed to headers, libraries and pkg-config files.
FROM debian:bookworm AS debian

RUN apt-get update; \
    apt-get --no-install-recommends -y -q install \
        libasound2-dev \
        libavahi-client-dev \
        libdbus-1-dev \
        libudev-dev \
        libwayland-dev \
        libxkbcommon-dev

# Make every library symlink relative, so it still resolves once the tree is mounted elsewhere.
RUN find /usr/lib -type l -lname '/*' | while read -r link; do \
        target=$(readlink "$link"); \
        ln -sfn "$(realpath -m --relative-to="$(dirname "$link")" "$target")" "$link"; \
    done

FROM scratch
COPY --from=debian /usr/include /usr/include
COPY --from=debian /usr/lib /usr/lib
# The AppImage carries the Avahi client libraries, so it carries their license beside them.
COPY --from=debian /usr/share/doc/libavahi-client3/copyright /usr/share/doc/libavahi-client3/copyright
COPY --from=debian /usr/share/common-licenses/LGPL-2.1 /usr/share/common-licenses/LGPL-2.1

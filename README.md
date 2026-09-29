# midi-harbor

Midi Harbor manages MIDI connections on macOS, Linux and Windows: virtual ports other
applications can use, attached MIDI hardware, RTP-MIDI network sessions to other computers, and
Bluetooth LE MIDI devices. A daemon owns every connection and repairs it on its own. A network
session that drops is reconnected, hardware that is unplugged and plugged back in picks up where
it left off, and notes sounding when a link went away are released rather than left ringing.

The command line and the graphical interface are both clients of the daemon, so closing either
changes nothing.

## Install

Download a package or archive from the releases, or build this project. `midi-harbor-headless`
leaves out the graphical interface, for machines without a display.

```bash
sudo apt install ./midi-harbor_<version>_<arch>.deb    # Debian and Ubuntu
sudo dnf install ./midi-harbor-<version>-1.<arch>.rpm    # Fedora and RHEL
chmod +x Midi-Harbor-<version>-x86_64.AppImage              # any other distribution
```

The release builds need glibc 2.35 or newer, so RHEL 9 and its rebuilds must build from source.

The Windows executable is not signed, so it warns the first time it runs;
[Installation](docs/installation.md#installing-a-package) says how to let it run.

On Windows, virtual ports need Windows MIDI Services. Windows 11 includes it from its late-2026
update; before that, install Microsoft's
[Windows MIDI Services SDK Runtime and Tools](https://github.com/microsoft/MIDI/releases).

## Building

Building needs Rust 1.96 or newer. On Linux it also needs the ALSA, D-Bus and Avahi development
files and libclang, and for the graphical interface the xkbcommon and Wayland development files.

On Debian and Ubuntu:

```bash
sudo apt install build-essential pkg-config libasound2-dev libdbus-1-dev \
                 libavahi-client-dev libclang-dev
sudo apt install libxkbcommon-dev libwayland-dev    # for the graphical interface
```

On Fedora and RHEL (enable CodeReady Builder first on RHEL and its rebuilds):

```bash
sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel dbus-devel avahi-devel clang-devel
sudo dnf install libxkbcommon-devel wayland-devel    # for the graphical interface
```

Then:

```bash
cargo build --release                        # with the graphical interface
cargo build --release --no-default-features  # without it
```

Windows builds are cross-compiled with mingw-w64:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Release packages for every platform are built with GoReleaser in Docker, through `make snapshot`
and `make release`.

## Running as a service

The daemon registers itself with launchd, a systemd user unit, or Task Scheduler, and runs at
every login:

```bash
midi-harbor service install --start
midi-harbor service status
```

## Running from the command line

To try things out, or to see more of what the daemon is doing, run it in the foreground:

```bash
midi-harbor daemon -vv
```

## Config

The setup lives in one YAML file per user, which the daemon writes as things change.
`midi-harbor config path` prints where it is. It can be written by hand; only names and kinds
are required:

```yaml
endpoints:
  - name: Sequencer Bus
    kind: virtual_port
  - name: Studio
    kind: network_port
routes:
  - from: Sequencer Bus
    to: Studio
```

Run `midi-harbor config reload` after editing it while the daemon runs.

## Usage

Every command has help:

```bash
midi-harbor --help
```

Run with no command, `midi-harbor` opens the graphical interface.

A basic setup that sends a local virtual port to another computer:

```bash
midi-harbor port create "Sequencer Bus"
midi-harbor network create "Studio"
midi-harbor network discover
midi-harbor network connect "Studio" "Stage Mac"
midi-harbor route create "Sequencer Bus" "Studio"
```

MIDI played into "Sequencer Bus" now reaches "Stage Mac", and keeps reaching it across sleep,
network changes, and either machine restarting.

## Testing

```bash
make test                # unit tests
make test-integration    # the daemon, CLI and protocols over real files and sockets
make test-live           # tests needing real MIDI, a Bluetooth radio or a session peer
```

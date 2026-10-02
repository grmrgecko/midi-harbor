# Installation

One binary, `midi-harbor`, contains the daemon, the command line, and optionally the graphical
interface. It comes as a package, or can be built from source.

## Installing a package

**macOS**: open the disk image and drag Midi Harbor to Applications. Opening the app shows the
graphical interface, which offers to start the daemon at login. From a terminal, the same binary
is at `/Applications/Midi Harbor.app/Contents/MacOS/midi-harbor`.

The app is signed with a Developer ID and notarized by Apple, so it opens like any other
downloaded app. A copy built from source is signed ad hoc instead: the first time it is opened
macOS refuses, saying Apple could not verify it is free of malware. Open **System Settings →
Privacy & Security**, scroll to the message that Midi Harbor was blocked, and choose **Open
Anyway**; macOS asks once more, and opens it normally from then on.

**Mac App Store**: the App Store build, which needs macOS 13 or newer, runs differently. The app
starts its own daemon and lives in the menu bar: closing the window, or ⌘Q, leaves everything
running with Midi Harbor's item in the menu bar, and **Quit Midi Harbor** there, or ⌥⌘Q, stops it.
Instead of a launchd service, **Start at login** in Settings starts it at login, opening the window
only if it was open when Midi Harbor last quit. Its setup is kept inside the app's container, so
`midi-harbor config path`, run from the app's own binary, prints a path under
`~/Library/Containers/com.mrgeckosmedia.MidiHarbor`. Of the `service` commands, only
`service stop` works there.

**Debian and Ubuntu**: `sudo apt install ./midi-harbor_<version>_<arch>.deb`, or the
`midi-harbor-headless` package for a machine without a display.

**Fedora, RHEL and other RPM distributions**: `sudo dnf install ./midi-harbor-<version>-1.<arch>.rpm`,
or `midi-harbor-headless`.

The Linux packages need glibc 2.35 or newer: Debian 12, Ubuntu 22.04, Fedora 36, RHEL 10 and
anything newer. On RHEL 9 and its rebuilds, build from source. The packages recommend Avahi and
BlueZ. Installing a package does not start anything. Each user who wants the daemon at login runs
`midi-harbor service install --start`.

**Any other Linux distribution**: `Midi-Harbor-<version>-<x86_64|aarch64>.AppImage` runs without
installing. Keep it where it will stay, such as `~/Applications`, make it executable with
`chmod +x`, and open it; it shows the graphical interface, which offers to start the daemon at
login. Opening it also adds Midi Harbor to your applications, with its icon, by writing a desktop
entry and the icon under `~/.local/share`; the entry goes away by itself once the AppImage file
is deleted. The service runs the AppImage file itself, so moving a newer one over it updates the
daemon at its next start; copying into it fails while the daemon runs. A newer one saved under
another name leaves the registration stale until it is opened and set up again. The AppImage
needs FUSE, which desktop distributions include, and a desktop's own libraries: ALSA, D-Bus and
libxkbcommon. It carries the Avahi client libraries, so it still browses and advertises sessions
where only the Avahi daemon is installed. Keep it away from AppImage's portable mode: a `.home` or
`.config` directory beside the file moves where the service is registered, and systemd never
finds it there.

**Windows**: unpack `midi-harbor-<version>.windows-amd64.zip` somewhere it will stay, such as
`%LOCALAPPDATA%\Programs\Midi Harbor`; the service remembers where it is. There is no installer.
The executable is not signed, so the first time it runs Windows may show "Windows protected your
PC"; choose **More info**, then **Run anyway**.

**Without installing**: each release also has a `.tar.gz` of the binary for Linux, and a
`midi-harbor-headless` one for Linux and for each kind of Mac. On the Mac the graphical interface
comes only in the app. The headless binary is not signed, so macOS refuses to run one downloaded
through a browser until the download's quarantine is removed with
`xattr -d com.apple.quarantine midi-harbor`.

`packaging/README.md` describes how the packages are built.

## Updating

Install the new version over the old one. The daemon goes on running the copy it was started
from until it is restarted, so the first time the window is opened afterwards it says **The
daemon is outdated** and offers **Update now**. That registers the copy you opened as the service
and restarts the daemon from it; connections drop for a few seconds and come back. Nothing is
restarted until you choose it, so an update installed during a show waits for you.

The same notice appears when Midi Harbor is installed a second way, as an AppImage and then a
package of another release: Update now makes the service run the copy you opened. Two packages of
one release are the same build, and neither replaces the other's daemon.

Without the window, `midi-harbor service status` says when the daemon is another build, and
`midi-harbor service install --start` replaces it with the program that was asked.

The Mac App Store build updates its daemon with the app: quitting Midi Harbor stops the daemon,
and the updated app starts its own.

## Building from source

### Requirements

**Rust** 1.96 or newer.

**macOS**: the Xcode command line tools (`xcode-select --install`). CoreMIDI, CoreBluetooth and
Bonjour ship with the system.

**Linux**: a C toolchain and the development files for ALSA, D-Bus and Avahi, plus libclang, which
the Avahi bindings are generated with. On Debian and Ubuntu:

```bash
sudo apt install build-essential pkg-config libasound2-dev libdbus-1-dev \
                 libavahi-client-dev libclang-dev
sudo apt install libxkbcommon-dev libwayland-dev    # for the graphical interface
```

On Fedora and RHEL, where RHEL and its rebuilds need the CodeReady Builder repository enabled
first:

```bash
sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel dbus-devel avahi-devel clang-devel
sudo dnf install libxkbcommon-devel wayland-devel    # for the graphical interface
```

A build without the graphical interface, `--no-default-features`, needs neither of the second
lines.

At run time, Linux needs:

- the ALSA sequencer (`snd-seq`), which every desktop distribution loads;
- `avahi-daemon` running, for other machines to find this one's network ports;
- BlueZ 5.50 or newer, for Bluetooth;
- systemd, to run the daemon at login. Without it, run the daemon in the foreground.

Without Avahi, network ports still work. Other machines just have to connect to them by address.

**Windows**: Windows builds are cross-compiled from macOS or Linux with mingw-w64:

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64                       # or apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
```

The linker is set in `.cargo/config.toml`. The binary is
`target/x86_64-pc-windows-gnu/release/midi-harbor.exe`.

At run time, Windows 10 or 11 (tested on Windows 11) needs Windows MIDI Services for virtual
ports. Windows 11 includes it from its late-2026 update; before that, install Microsoft's
[Windows MIDI Services SDK Runtime and Tools](https://github.com/microsoft/MIDI/releases). Before
the update, closing a virtual port stops the Windows MIDI Service answering until it is
restarted; [Platforms](platforms.md#virtual-ports-on-windows) says how. Everything else Midi
Harbor needs is part of Windows.

### Building

```bash
cargo build --release                        # with the graphical interface
cargo build --release --no-default-features  # without it
```

The build without the graphical interface leaves out the whole GUI toolkit, not just the window.
Use it on servers and machines with no display. Building the graphical interface on Linux needs
libcosmic's own build dependencies as well.

To install the binary into `~/.cargo/bin`:

```bash
cargo install --path .                       # or add --no-default-features
```

### Testing

```bash
make test                # unit tests: wire formats, external formats, algorithms
make test-integration    # the daemon, CLI and protocols as a whole, over real files and sockets
make test-live           # tests needing CoreMIDI, ALSA, WinMM, a radio or a session peer
```

The first two need nothing but the build dependencies above. `make test-live` runs the tests that
need something real on this machine, one at a time; each names what it needs, and fails without
it.

## Running the daemon

Nothing connects until the daemon is running. There are two ways to run it.

**At login, as a service.** This is the normal way:

```bash
midi-harbor service install --start
midi-harbor service status
```

`service install` registers the binary it was run from:

| | Registration | Controlled with | Log |
|---|---|---|---|
| macOS | a launchd agent, `~/Library/LaunchAgents/com.mrgeckosmedia.MidiHarbor.daemon.plist` | `launchctl` | `~/Library/Logs/midi-harbor/daemon.log` |
| Linux | a systemd user unit, `~/.config/systemd/user/midi-harbor.service` | `systemctl --user` | `journalctl --user -u midi-harbor` |
| Windows | a Task Scheduler task, "Midi Harbor", started at sign-in | Task Scheduler | `%LOCALAPPDATA%\midi-harbor\logs\daemon.log` |

On macOS and Windows the daemon writes the log file itself. Past 10 MB it moves the file to
`daemon.log.1`, replacing the one before, and starts again, so the two never take more than 20 MB.
On macOS, Console.app shows it under Log Reports. A registration made before this log existed has
none: run `service install` again to add it.

Running `install` again updates the registration in place. It does not add a second one. If the
binary has since moved, `service status` reports the registration as stale, and `install` repairs
it. `service stop` and `service start` stop and start the daemon without changing the
registration.

**In the foreground**, for trying things out or for a machine without a service manager:

```bash
midi-harbor daemon
```

It logs to the terminal and stops on Ctrl-C, releasing any notes still sounding as it goes. Add
`-v` or `-vv` for more detail, or `--log-file <PATH>` to log to a file that rolls over the same
way as the service's.

Only one daemon runs per user. The command line finds it through a socket in the user's runtime
directory (`$XDG_RUNTIME_DIR/midi-harbor/daemon.sock` on Linux, the per-user temporary directory
on macOS). `--socket` points a command, or a second daemon, somewhere else. The socket is readable
only by its owner, and the daemon is never reachable over the network.

On Windows the daemon listens on a named pipe with a random name instead, and writes that name
to `daemon.sock` in the user's temporary directory, where the command line reads it. The pipe
refuses connections from other machines. On Windows the task runs the daemon under a supervisor
that starts it again after a crash, and `service stop` asks the daemon to stop rather than ending
it, so held notes are released first.

## Bluetooth on macOS

macOS asks for permission before a program may use Bluetooth, and grants it to an app. Installed
from the disk image, the daemon runs from inside Midi Harbor.app and should be asked about as
Midi Harbor. If Bluetooth is refused to it, run `midi-harbor daemon` from a terminal, which uses
the terminal application's permission instead. A bare binary registered with `service install`
has neither, so on macOS install the service from the app. `midi-harbor capabilities` reports
which Bluetooth roles are usable, and why not when they are not.

## Removing it

```bash
midi-harbor service uninstall
```

This stops the daemon and removes its registration. The configuration file is left where it is
(see [Configuration file](configuration.md)), so installing again brings the same setup back.
Delete it by hand to start from nothing.

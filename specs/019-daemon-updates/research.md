# Research: Daemon Updates

The investigation behind this spec, under its number in the project-wide research log.

---

## R-108: A daemon left running by another build

**Status**: **DONE** (2026-10-02). Built as T253.

**What happened before.** A client called `GetServerInfo` and refused only another major
protocol version. Nothing compared builds, so after an update the old daemon ran on:

| Install | Updating while the daemon runs |
|---|---|
| macOS disk image, launchd | The old daemon kept running until the next login |
| Mac App Store | Quitting the app stops its daemon, so the updated app started the new one; a daemon left by a window that crashed was attached to and kept |
| `.deb`, `.rpm` | Installing starts nothing, so the old daemon kept running |
| AppImage | A new file moved over the old one was used at the daemon's next start |
| Windows archive | No installer; the old daemon kept running |

`service install --start` did not help under systemd or Task Scheduler either: it rewrote the
registration and started the service, and starting a service that is running does nothing.

**The identifier.** `midi_harbor_core::BUILD_ID`, a UUID set by `crates/core/build.rs`. It takes
`MIDI_HARBOR_BUILD_ID` from the environment when that is set, and makes one otherwise. A package
can hold binaries built separately that must agree: the App Store app and its headless helper,
and each architecture of a universal binary. So `packaging/macos/build.sh` and the release build
each choose one identifier for everything they build in a run. A build outside packaging gets a
new identifier when the core crate or `VERSION` changes, which is as often as cargo runs the
script again without forcing every build to relink. The daemon reports it as
`ServerInfo.build_id` (7), protocol 1.3; an older daemon leaves it empty, which matches nothing.

**One release, several packages.** The `.deb`, `.rpm` and AppImage of a release hold the same
binary and so the same identifier. A user who installs two of them has one build twice, and no
daemon to update, so neither window replaces the other's daemon. Comparing the registered path as
well was considered and left out: it would restart the daemon every time the other copy was
opened, to run the same code.

**Why reinstall rather than restart.** Restarting runs whatever the service is registered to
run, which after a second install is the other copy. Registering this copy and then starting it
is what makes the daemon the one the window came with, wherever it is.

**Asking instead of acting.** The first version replaced the daemon as soon as the window
connected. That needed guards against a loop: two windows of different builds each replacing
the other's daemon when they reconnected, and a replacement that failed being tried again on
every reconnect. The owner asked for a notice with an Update now button instead. Nothing is then
restarted unless someone asks, which removes the loop rather than guarding it, and leaves the
few seconds without MIDI to be timed by the person at the machine.

**What the notice says.** "The daemon is outdated" for an older version or one too old to say,
"a different build" for the same version, and "newer than this window" for a newer one, where the
button reads Use this version: an older program in a newer one's place may not read the
configuration it wrote, which is refused with `SchemaTooNew`. The button is offered only when the
service manager says it is running the daemon and the window is on the service's own socket.
Otherwise there is no registration that says how the daemon was started, and the notice says to
restart it from this copy.

**While the daemon is being replaced** the old one still answers for a moment. The window does
not reconnect until the replacement has finished, or it would put the notice straight back.

**Replacing under each service manager.** `ServiceManager::replace` stops the daemon, installs
the registration and starts it. Stopping comes first so the service manager stops the process it
started. launchd is the exception: installing boots the old job out, which stops its daemon, and
stopping it beforehand only has launchd start the old one again in between.

**The App Store build** has no service. The app starts its helper, or attaches to one already
answering. It now asks an attached daemon for its build and, when it is another, asks it to stop
and starts its own.

**Checked live** on Arch Linux under systemd, with two builds of the tree given the identifiers
`aaaaaaaa-…` and `bbbbbbbb-…`. See T253 for what was seen.

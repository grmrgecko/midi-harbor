# Research: Graphical Interface

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-001: Can libcosmic build and run on macOS?

**Decision**: Yes. Use libcosmic for the optional GUI on both macOS and Linux.

**Status**: **VERIFIED** empirically. A spike crate depending on `libcosmic` with
`default-features = false, features = ["winit", "wgpu", "tokio", "multi-window"]` compiled cleanly
on macOS 15.6 / Apple Silicon, and a real `cosmic::Application` implementing `view`/`update` linked
and **launched a window that stayed alive**. This was the single largest risk in the project and it
is now retired.

**Rationale**: libcosmic's `Cargo.toml` already carries explicit `cfg` handling that excludes
macOS from the Linux-only dependency set (`freedesktop-icons`, `freedesktop-desktop-entry`,
`zbus`, `ashpd`, `cctk`, `cosmic-config`'s dbus path) and substitutes `phf`-based bundled icons on
non-Unix and macOS targets. The default feature set (`wayland`, `x11`, `dbus-config`, `a11y`) is
Linux-oriented and must be turned off on macOS.

**Consequences for the plan**:

- Feature selection must be target-conditional in `Cargo.toml`:
  - macOS: `default-features = false`, `features = ["winit", "wgpu", "tokio", "multi-window"]`
  - Linux: the default feature set, plus `wayland` and `x11`.
- libcosmic is pinned by git revision, not a crates.io version. It publishes as `1.0.0` from
  `github.com/pop-os/libcosmic`; the spike used revision `87ab8179`. Pin an exact revision and bump
  deliberately — this is a fast-moving dependency and the macOS path is not covered by its CI.
- Icon rendering on macOS uses bundled icons rather than a freedesktop icon theme, so the GUI must
  not assume a system icon theme is present.

**Alternatives considered**: `iced` directly (libcosmic is built on it) — rejected because the user
specified libcosmic and the spike proved it viable. `egui` — rejected for the same reason.

---

## R-025: What building the window found that no test had

**Status**: **VERIFIED** against a running daemon on macOS (2026-09-20).

The graphical interface was the last client to be written, and writing it surfaced four defects
that the command line had been quietly tolerating. All four were in the daemon, not the window.

**Enabling a device did not reopen it.** `open_present_devices` was reached only from device
refresh, so a device switched off and on again stayed shut until the next hot-plug or restart,
while still listing as attached and accepting routes. Reconcile now brings hardware up alongside
virtual ports. Verified by sending notes through a device before and after a disable/enable cycle.

**Disabling a device called the virtual port teardown.** A device belongs to the system; only the
ports opened onto it are ours to release. It now closes rather than destroys.

**Monitoring a switched-off endpoint waited forever.** A runtime entry exists even for a disabled
endpoint, so the "is not running" check never fired and a user sat watching a stream that could
never carry anything. The handle, not the entry, decides.

**The endpoint list had no order.** Configured endpoints load first and discovered hardware is
appended, so the same machine listed its endpoints differently after a restart. This was found the
hard way: a toggle clicked in the window landed on a different row than the one read, because the
list reordered between the two. `ListEndpoints` now sorts by kind, then name, then identifier.

**Event kinds were Debug-formatted.** `format!("{:?}", kind)` put `EndpointStateChanged` on the
wire where the contract documents `endpoint_state_changed`, and made every Rust variant rename a
silent contract change. The names are now written out and tested for shape.

**The lesson**: each of these was invisible from the command line, which lists once and exits. A
client that redraws every second and offers a control for each row exercises the daemon in a way
no single command does.

---

## R-026: The window is a client, and recovers like one

**Status**: **VERIFIED** (2026-09-20). Daemon killed and restarted with the window open.

The interface polls a snapshot once a second rather than holding `WatchState` open. The streams
exist so a client does not miss an event; a window redrawing every second misses nothing a person
can see, and polling means a daemon restart needs no reconnect logic — a failed refresh drops the
channel and the next tick reconnects.

Killing the daemon replaces the whole view with what to do about it, rather than leaving a stale
list beside an error, which would suggest those endpoints were still being managed. Restarting the
daemon restored the window with no user action.

The presentation logic is pure functions over generated types, tested without a window or a
daemon: 24 tests cover the wording, including that the two kinds of down never read the same, that
guidance appears only where a user can act, that our own drops are never reported as network loss,
and that attached hardware does not read as a connection.

---

## R-078: The redesigned window, and what it asks of the daemon

**Status**: **DECIDED** (2026-09-23), for Phase 12. Decided by the owner, working through a
libcosmic design study in `crates/gui/examples/designs/`.

The first window was a nav bar over flat lists, with forms built into the pages and a kind named
only by a faint word under each endpoint. The owner compared five layouts drawn in libcosmic,
each navigable through every area, and chose the list layout, then refined it. Several of the
refinements are not wording: they change what a port, a route and a network session are, so they
reach the daemon, the configuration file and the contract.

Decisions on the window:

- **A list per kind.** Endpoints are listed in one section per kind: virtual ports, network
  ports, Bluetooth devices, USB and hardware, and the IAC buses and Apple network sessions macOS
  provides. Each kind Midi Harbor makes ends with its own add action.
- **A docked side panel.** Clicking an endpoint opens its details beside the list, docked rather
  laid over it so a dialog can open without closing it. Edit, Delete, Disconnect and Forget live
  there, not on the rows. Changing page closes it.
- **One dialog to add and edit.** Adding and editing the same thing share one modal dialog,
  filled in when editing.
- **Routes switch on and off on their row only**, never in a dialog.
- **Pages:** Endpoints, Routes, Bluetooth, Activity, Monitor and Settings. There is no network
  page: network ports are endpoints, the machines a network port can reach are in its panel, and
  the machines this one knows, with whether each is let in without asking, are in Settings.
- **No direction shown** for endpoints that always carry MIDI both ways. Hardware shows its MIDI
  In and MIDI Out counts, as Audio MIDI Setup does, with its maker and model.

Decisions that reach the daemon:

- **"Network port", not "session".** RTP-MIDI calls it a session, and so do Apple and rtpMIDI,
  but from the user's side it is a port that reaches the network. It is labelled "Network port ·
  RTP-MIDI", its UDP number is "UDP port", and the dialog says Apple calls it a network session.
  The CLI's `session` commands and the configuration's `network_session` kind are still accepted.
- **Connector counts for virtual ports.** A virtual port has a number of MIDI In connectors and a
  number of MIDI Out connectors, as the IAC Driver's buses do, each at least 1 and at most 16. A
  count above one shows to other applications as numbered ports, and a route names one
  connector. This replaces the in-only and out-only directions, which FR-002 never allowed: an
  existing port of either becomes one of each.
- **An automatic virtual port for a network port**, on by default. Other applications see the
  network port as a MIDI port of the same name, joined to it both ways, the way macOS presents
  its own network sessions. Off, the network port only carries devices over the network. The
  port is the network port's own: renamed and removed with it, and never listed on its own.
- **Two-way routes.** One route can carry MIDI both ways between two endpoints that both send
  and receive, instead of two routes.
- **A network port's machines.** Its panel shows the name other machines see (the Bonjour name,
  `local_name`), and one list of machines: those in it, each with its address, latency and its
  own Disconnect, then those it could connect to, each with Connect, then a way to connect by
  name, address and port, the port defaulting to 5004. Connecting a second machine while one is
  connected carries it beside the first, which needs the daemon to invite it.

Each decision that reaches the daemon is a task in Phase 12, and each contract change there is
additive.

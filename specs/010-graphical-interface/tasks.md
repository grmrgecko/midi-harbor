# Tasks: Graphical Interface

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 9: GUI

**Goal**: A libcosmic interface over the existing IPC contract, adding no domain logic.

- [x] T138 Implement the libcosmic application shell in `crates/gui/src/app.rs` per the verified spike pattern in research R-001, pinning the exact libcosmic git revision — *shell lives in `crates/gui/src/app.rs`, libcosmic pinned at `87ab8179`*
- [x] T139 [P] Implement the endpoint list view with live state, time in phase and traffic activity in `crates/gui/src/views/endpoints.rs` (FR-044) — *views live in `crates/gui/src/app.rs` rather than a `views/` directory; time in phase added, and an idle session reads "listening" rather than as a fault*
- [x] T140 [P] Implement the routing view with creation, enable/disable and broken-route display in `crates/gui/src/views/routes.rs` — *in `crates/gui/src/app.rs`; exercised by clicking through it on macOS, which is how the daemon refusing identifiers was found*
- [x] T141 [P] Implement the invitation prompt with "always accept" in `crates/gui/src/app.rs` (FR-014); the discovery view is still to come
- [x] T142 [P] Implement the Bluetooth scan and advertising views in `crates/gui/src/views/bluetooth.rs`, rendering unavailable capabilities as unavailable (FR-053) — *in `crates/gui/src/app.rs`; each role shows its capability's reason instead of controls when unavailable, decided by a new stable `Capability.id`. Advertising was switched on and off through it on macOS against the real radio*
- [x] T143 [P] Implement the event history and MIDI monitor views in `crates/gui/src/views/diagnostics.rs` (FR-046, FR-047) — *in `crates/gui/src/app.rs`; the monitor streams `MonitorEndpoint` only while its page is showing, and was watched carrying MIDI played into a real CoreMIDI port*
- [x] T144 Implement the service-not-installed onboarding in `crates/gui/src/views/onboarding.rs` offering installation in one action and stating what it will do (FR-042) — *in `crates/gui/src/onboarding.rs`; the offer follows what the service manager reports (install, start, or reinstall over a stale registration), and is withheld under `--socket`. Rendered against the real launchd; the button was not pressed, since it registers a real login item, and runs the same calls as `service install --start`*
- [x] T145 Implement the IPC client with event subscription and cache resync in `crates/gui/src/client.rs`, tolerating resync at any time (contracts/ipc-protocol.md §6) — *the cache is patched from `WatchState` and read again whenever the stream ends, with a two-second poll for what the stream does not carry; writing it found the stream reporting sessions without their state*

**Checkpoint**: Feature complete on both platforms.

## Phase 12: The redesigned window

The owner chose a design by working through a libcosmic study (R-078). The window is rebuilt
first over the contract as it stands; each later task adds what it needs to the daemon, the
configuration file and the contract, and then to the window.

- [x] T180 Rebuild the window on the chosen list layout in `crates/gui/src/`: pages Endpoints (one section per kind, each with its add action), Routes, Bluetooth, Activity, Monitor and Settings; a docked side panel holding each endpoint's details and actions; one modal dialog each for adding and editing ports, network sessions, routes and machines; the known machines in Settings; wording kept in `format.rs` with tests, per R-078, US2, SC-001 — done: the six pages, the docked panel and the add and edit dialogs over the current contract, with IAC buses and other applications' ports told apart from hardware by a new `software` field on device details; editing a route's ends, deleting a network port, a machine's trust, and preferences wait on later tasks
- [x] T189 Remove the design study in `crates/gui/examples/designs/` once the window matches it, per R-078 — done: moved out of the repository once the window was rebuilt on it

## After the split

- [x] T240 Keep the Monitor page's endpoint list inside the window: move the endpoint picker from the header's right corner to the left of the page, under a label that asks for a choice until one is made, since libcosmic's dropdown opens its list rightwards as wide as the longest name without holding it to the window, and from the corner it ran off the edge and cut names short (found 2026-09-27 while checking the test note on screen) — done: checked on screen against a scratch daemon, every name shown whole

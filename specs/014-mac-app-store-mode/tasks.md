---

description: "Task list for the Mac App Store mode"
---

# Tasks: Mac App Store mode

**Input**: Design documents from `/specs/014-mac-app-store-mode/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Two tiers, per the constitution's Principle VI and AGENTS.md "Testing". The mode keys on
`APP_SANDBOX_CONTAINER_ID`, so its effect on the CLI, the daemon and the daemon's owner is proved
by integration tests that run the real binary with that variable set, on any Mac, unsigned. One
unit test pins the `SMAppService` status mapping, a fact about macOS (research R-099). AppKit,
the login item and the signed sandbox cannot run without a desktop session and are proved by
hand through quickstart.md.

**Organization**: Tasks are grouped by user story. The sandboxed build (User Story 4) is
foundational here, since every other story is checked by hand against it.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1 to US4)

---

## Phase 1: Setup

**Purpose**: The governance change and the dependencies every later phase needs

- [X] T001 Amend `.specify/memory/constitution.md` to 1.4.0: Principle II's list of what runs the
  daemon independently of the GUI gains "on macOS through the App Store, a bundled helper the app
  starts and supervises"; add the amendment note dated 2026-09-27 citing feature 014 (then 003) and research
  R-095; mirror it in AGENTS.md's seam table and daemon-contract section — done: constitution 1.4.0, Principle II and the matrix name the App Store helper; AGENTS.md's seam table too
- [X] T002 [P] Add macOS-only dependencies to crates/platform/Cargo.toml: `objc2` 0.6,
  `objc2-foundation` 0.3, `objc2-app-kit` 0.3 (NSApplication, NSMenu, NSMenuItem, NSStatusBar,
  NSStatusItem, NSImage, NSRunningApplication, NSEvent features) and `objc2-service-management`
  0.3 (SMAppService), at the versions winit already locks (research R-098); add
  `midi-harbor-platform` to crates/gui/Cargo.toml — done: objc2 0.6, objc2-foundation, objc2-app-kit and objc2-service-management 0.3 in the platform crate; only ServiceManagement's bindings are new to the lock

---

## Phase 2: Foundational

**Purpose**: The mode's detection and its effect on paths, the service commands and the
capability list, the supervisor's stop, and the sandboxed build every manual check runs against

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [X] T003 Add `pub fn sandboxed() -> bool` to crates/core/src/paths.rs, true exactly when
  `APP_SANDBOX_CONTAINER_ID` is set (R-094), and make `Paths::resolve()` put the runtime directory
  at `std::env::temp_dir()` itself when sandboxed, so the socket is `$TMPDIR/daemon.sock` (R-096);
  document the 103-byte limit it keeps within — done: `paths::sandboxed()`; sandboxed, the runtime directory is the container's `tmp` itself
- [X] T004 [P] Report `CapabilityName::ServiceManager` unavailable with
  `UnavailableReason::NotBuilt` when `paths::sandboxed()` in crates/platform/src/capability.rs — done: in `crates/daemon/src/state.rs`, where the daemon turns `service::detect()`'s error into the capability's reason: `ServiceError::AppStore` reads as `NotBuilt`
- [X] T005 [P] Make `service::detect()` in crates/service/src/lib.rs return a new
  `ServiceError::AppStore` when sandboxed, displayed exactly as
  `the App Store build is started by Midi Harbor itself; turn on "Start at login" in its settings`
  (contracts/cli.md), which crates/cli/src/service_cmd.rs already maps to exit code 4 — done: `ServiceError::AppStore`, exit 4 through the existing mapping
- [X] T006 [P] Add `pub fn peer_pid(socket: &Path) -> io::Result<u32>` to
  crates/platform/src/socket.rs for macOS: connect, `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`, close,
  with a `// SAFETY:` comment (R-095) — done: built, then removed: the sandbox forbids signalling a daemon the app did not start, so a stop request over the socket replaced it (R-095); `LOCAL_PEERPID` itself worked
- [X] T007 Add a stoppable supervisor to crates/service/src/supervisor.rs: `Supervisor::start(program,
  arguments)` runs the existing restart loop on a thread of its own and records the current
  child; `stop(&self, grace: Duration)` sends SIGTERM to the child, stops restarting, and waits up
  to `grace` for the child to exit before killing it; the blocking `supervise()` Windows uses
  keeps its behaviour — done: `Supervisor::start` and `stop`, the restart loop on its own thread, waking early from a restart delay for a stop
- [X] T008 Integration test in tests/app_store_mode.rs, running `CARGO_BIN_EXE_midi-harbor` with
  `APP_SANDBOX_CONTAINER_ID`, `HOME` and `TMPDIR` pointed at a scratch directory: `service install`
  and `service status` exit 4 and print the contracts/cli.md message; `capabilities --json`
  reports service installation unavailable with reason `NotBuilt`; `daemon` binds
  `$TMPDIR/daemon.sock` and `status` reaches it there; unset, `service status` does not exit 4 — done: tests/app_store_mode.rs, macOS only; mutation-checked: without the sandbox refusal and without the container socket layout each test fails
- [X] T009 Integration test in tests/app_store_mode.rs: a `Supervisor` running the real binary as
  `daemon --socket <scratch>` is stopped by `stop(5 s)`, the child exits successfully, is not
  started again, and the socket stops answering; `peer_pid` on the socket while it served
  returned the child's pid (macOS only) — done: stops within the daemon's own grace, not the kill after it, and nothing restarts; mutation-checked without SIGTERM
- [X] T010 [P] Create packaging/macos/app-store.entitlements and packaging/macos/helper.entitlements
  with exactly the keys in contracts/bundle.md — done
- [X] T011 Add the App Store variant to packaging/macos/bundle.sh (a `--helper <binary>` option that
  copies it to `Contents/MacOS/midi-harbor-daemon`, signs it with helper.entitlements first, signs
  the bundle with app-store.entitlements, sets `LSMinimumSystemVersion` 13.0 and `LSUIElement`
  true in Info.plist, and makes no disk image) and to packaging/macos/build.sh (`--app-store`:
  builds the full and `--no-default-features` binaries, universal when both targets are
  installed, into target/package/macos-app-store/) — done: verified: the bundle signs and verifies, each executable carries exactly its entitlements, Info.plist has 13.0 and `LSUIElement`

**Checkpoint**: `make test-integration` passes with T008 and T009; `build.sh --app-store` makes a
bundle that verifies and carries the entitlements in contracts/bundle.md

---

## Phase 3: User Story 1 - Connections keep running from the menu bar (Priority: P1) 🎯 MVP

**Goal**: The sandboxed app starts or attaches to its daemon, hides to a menu bar item when its
window closes, and stops the daemon when quit from that item

**Independent Test**: quickstart.md scenarios 1 and 3 against the variant from T011

### Tests for User Story 1

- [X] T012 [US1] Integration test in crates/gui/tests/app_store.rs for the daemon owner, with the
  real binary on a scratch socket: with nothing serving, `DaemonOwner::start` starts one daemon;
  with a daemon already serving, it attaches and starts none; a daemon that is killed is started
  again; `stop()` ends an attached daemon too, found through `peer_pid` — done: in tests/app_store_mode.rs, since only the root package can run the real binary; stopping the attached daemon is through `StopDaemon`, mutation-checked without it

### Implementation for User Story 1

- [X] T013 [US1] Implement `DaemonOwner` in crates/gui/src/app_store.rs: probe the socket with
  `midi_harbor_daemon::already_serving`-style probing through the IPC transport; attach, or start
  `Contents/MacOS/midi-harbor-daemon daemon` (the helper beside the current executable) with the
  `Supervisor` from T007; `stop()` stops its supervisor, or SIGTERMs the `peer_pid` of an attached
  daemon and waits up to 5 s; log start, restart, attach and stop at info with the reason — done: `DaemonOwner` in crates/gui/src/app_store.rs; an attached daemon is stopped through the contract's new `StopDaemon` (protocol 1.1), chosen by the owner once the sandbox refused the signal
- [X] T014 [P] [US1] Implement the menu bar item and the Dock icon in
  crates/platform/src/appkit/menus.rs and crates/platform/src/appkit/dock.rs: an `NSStatusItem`
  with the app icon as a template image and the items "Open Midi Harbor" and "Quit Midi Harbor"
  (contracts/menus.md); `set_dock_visible(bool)` switching the activation policy between
  `.regular` with activation and `.accessory` (R-098); every action sent on a
  `futures` unbounded channel as a `ShellAction`, exported safely from
  crates/platform/src/appkit/mod.rs — done: the glyph is the anchor alone, `packaging/macos/MenuBarIcon.svg`, since a template image keeps only the alpha
- [X] T015 [US1] Implement the app delegate in crates/platform/src/appkit/delegate.rs, set before
  the event loop starts: `applicationShouldTerminate:` returns `NSTerminateLater` and sends
  `ShellAction::Quit`, and `reply_to_terminate()` answers it once the daemon has stopped;
  `applicationShouldHandleReopen:hasVisibleWindows:` sends `ShellAction::Open` (FR-A11) — done
- [X] T016 [US1] In crates/gui/src/lib.rs and crates/gui/src/app.rs, when `paths::sandboxed()`:
  build Settings with `exit_on_close(false)`; `on_close_requested` hides the main window with
  `window::set_mode(Hidden)` and hides the Dock icon; subscribe to the `ShellAction` channel, where
  `Open` shows the window, restores the Dock icon and focuses it, and `Quit` stops the
  `DaemonOwner` off the UI thread, then replies to the terminate and exits — done: the menus are installed from the first message, since winit builds its default menu after `init` (R-098)
- [X] T017 [US1] In crates/gui/src/onboarding.rs, when sandboxed, never offer the service; while the
  daemon is starting say so, and if it cannot start show why, with Quit still working — done: `Starting Midi Harbor…`, or why it could not, and no service offer
- [X] T018 [US1] In src/main.rs, when sandboxed and running the GUI: install the delegate and menu
  bar item, start the `DaemonOwner`, then run the window, keeping main.rs to dispatch only — done: in `gui::run` rather than main.rs, which stays dispatch only
- [X] T019 [US1] Run quickstart.md scenarios 1 and 3 against the variant; record the results in
  research.md R-098 — done: verified on the debug variant, R-098

**Checkpoint**: The sandboxed app keeps MIDI running with the window closed, survives its window
crashing, and stops everything on Quit

---

## Phase 4: User Story 2 - Quitting the window versus quitting Midi Harbor (Priority: P1)

**Goal**: A real app menu where ⌘Q closes to the menu bar and ⌥⌘Q quits

**Independent Test**: quickstart.md scenario 2

- [X] T020 [US2] Replace winit's default menu in crates/platform/src/appkit/menus.rs with the menu
  in contracts/menus.md: "Close to Menu Bar" on ⌘Q and Close on ⌘W sending
  `ShellAction::CloseWindow`, "Quit Midi Harbor" on ⌥⌘Q sending `terminate:`, and the standard
  About, Hide, Hide Others, Show All and Minimize; installed once the event loop runs, in App
  Store mode only — done: Quit is ⌥⌘Q, not ⇧⌘Q, which is macOS's Log Out and takes precedence; the owner chose ⌥⌘Q
- [X] T021 [US2] Handle `ShellAction::CloseWindow` in crates/gui/src/app.rs exactly as the close
  button (T016) — done
- [X] T022 [US2] Run quickstart.md scenario 2, including a logout-style `terminate:` sent with
  `osascript -e 'tell application "Midi Harbor" to quit'`, and record the results in R-098 — done: verified by clicking the menu items and reading their shortcuts back; no synthetic ⌥⌘Q or ⇧⌘Q was sent after ⇧⌘Q opened the log-out dialog, which was cancelled

**Checkpoint**: ⌘Q never stops a connection and ⌥⌘Q always ends the process

---

## Phase 5: User Story 3 - Starting at login (Priority: P2)

**Goal**: A Start at login switch, and a login launch that restores the window's state at quit

**Independent Test**: quickstart.md scenario 5, with the owner

### Tests for User Story 3

- [X] T023 [P] [US3] Unit test in crates/platform/src/appkit/login_item.rs pinning the mapping in
  data-model.md: `enabled` shows on, `requiresApproval` waiting, and both `notRegistered` and
  `notFound` off with `register()` allowed, citing R-099 for why `notFound` is not "unavailable" — done

### Implementation for User Story 3

- [X] T024 [US3] Implement crates/platform/src/appkit/login_item.rs: `status()`, `register()` and
  `unregister()` over `SMAppService.mainApp`, and the switch state mapping from data-model.md — done
- [X] T025 [US3] Record in crates/platform/src/appkit/delegate.rs whether the launch Apple event's
  `keyAEPropData` is `keyAELaunchedAsLogInItem`, read in `applicationDidFinishLaunching:`, and
  expose it as `launched_at_login()` (R-099) — done: the Apple event's `eventID` and `paramDescriptorForKeyword:` are called by message, avoiding the CoreServices bindings
- [X] T026 [US3] Keep the window state in crates/gui/src/app_store.rs as `window.yaml` in the
  configuration directory, with the fields in data-model.md: `open` ("A missing or unreadable file
  reads as open") and `login_offered`; written atomically only when quitting entirely — done
- [X] T027 [US3] In crates/gui/src/app.rs and crates/gui/src/view.rs: at a login launch start with
  the window hidden unless `open` was true; any other launch opens it; add the Start at login
  switch to Settings with its on, waiting and off states and any `register()` error beside it;
  offer it once on first launch when `login_offered` is not set — done: the offer is a banner above every page until answered
- [x] T028 [US3] Run quickstart.md scenario 5, and record in R-099 whether
  registration from an ad hoc signed sandboxed build and the login-launch check work

**Checkpoint**: The app starts at login and opens its window exactly when it was open at quit

---

## Phase 6: User Story 4 - A sandboxed build to test with (Priority: P2)

**Goal**: Show that everything Midi Harbor does works inside the sandbox, or says why not

**Independent Test**: quickstart.md scenario 4, and scenario 6 with the owner

- [x] T029 [US4] Run quickstart.md scenario 4: Bluetooth from the window, a USB MIDI device's
  identity with and without `com.apple.security.device.usb`, and `capabilities`; drop the USB
  entitlement from packaging/macos/app-store.entitlements and contracts/bundle.md if nothing needs
  it, and record the result in R-100
- [x] T030 [US4] Run quickstart.md scenario 6, and record in R-097
  whether the daemon's replacement after `killall MIDIServer` works as the helper

**Checkpoint**: The variant is proved against every capability

---

## Phase 7: Polish & Cross-Cutting Concerns

- [X] T031 [P] Document the App Store build in docs/installation.md (how it starts, the menu bar,
  Start at login, `config path` in the container) and docs/platforms.md (the sandbox's effects),
  and `build.sh --app-store` in packaging/README.md — done
- [X] T032 Prune any test in the suites this feature touched that breaks the testing rules, and
  confirm no helper or import was left unused — done: the new suites hold four integration tests and one unit test; `peer_pid`, `stop_process` and `Supervisor::pid`, which only tests would have used once `StopDaemon` replaced signalling, were removed rather than kept for them
- [X] T033 Run the gates on macOS, the Arch VM and Windows (cross-clippy, headless build, and
  scripts/windows-test.sh), confirming the headless build still carries no GUI or AppKit code — done: fmt, clippy, `make test`, `make test-integration` and the headless build pass on macOS and the Arch VM; Windows cross-clippy and headless build pass, and all 47 test binaries pass on the Windows VM
- [X] T034 Update research.md statuses and mark the spec implemented — done: research statuses updated; the spec is marked implemented

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies
- **Foundational (Phase 2)**: after Setup; blocks every story. T008 needs T003 to T005; T009 needs
  T006 and T007; T011 needs T010
- **US1 (Phase 3)**: after Foundational. T013 needs T007 and T006; T016 needs T014 and T015; T018
  needs T013 to T016
- **US2 (Phase 4)**: after US1's T014 to T016, which it extends
- **US3 (Phase 5)**: after US1; T027 needs T024 to T026
- **US4 (Phase 6)**: after US1, since it runs the app
- **Polish (Phase 7)**: after the stories

### Parallel Opportunities

- T002 alongside T001
- T004, T005 and T006 alongside each other after T003; T010 at any point in Phase 2
- T014 alongside T013
- T023 alongside T024 to T026

---

## Parallel Example: Foundational

```bash
Task: "Report ServiceManager unavailable when sandboxed in crates/platform/src/capability.rs"
Task: "Refuse service::detect() when sandboxed in crates/service/src/lib.rs"
Task: "Add peer_pid to crates/platform/src/socket.rs"
Task: "Create the entitlements files in packaging/macos/"
```

---

## Implementation Strategy

### MVP First (User Story 1)

1. Phases 1 and 2: the mode is detected, the CLI and daemon behave, and the variant builds
2. Phase 3: the app owns its daemon and lives in the menu bar
3. **Validate** with quickstart.md scenarios 1 and 3

### Incremental Delivery

1. US2 adds the menu and shortcuts
2. US3 adds Start at login, needing the owner for its final check
3. US4 closes the capability checks, needing the owner for the MIDI server check

---

## Notes

- Commit after each task or logical group, in Conventional Commits
- A new integration test fails without the change it covers
- Ask the owner before anything that adds a login item, logs out, or kills `MIDIServer`

# Implementation Plan: Mac App Store mode

**Date**: 2026-09-27 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/014-mac-app-store-mode/spec.md`

## Summary

When the app runs sandboxed, it becomes a menu bar app that owns its daemon: it starts a bundled
headless copy of the program as `daemon`, supervises it, and stops it when the user quits Midi
Harbor entirely. Its window hides to the menu bar on close and Command-Q, the Dock icon follows
the window, and "Start at login" registers the app as a login item. A new `--app-store` option of
`packaging/macos/build.sh` builds the sandboxed variant, signed ad hoc. Unsandboxed, nothing
changes.

## Technical Context

**Language/Version**: Rust 1.96, edition 2024.

**Primary Dependencies**: new in `crates/platform`, macOS only: `objc2` 0.6, `objc2-foundation`
0.3, `objc2-app-kit` 0.3 and `objc2-service-management` 0.3. The first three are already built
for winit at those versions, so only ServiceManagement's bindings are new.

**Storage**: the window's state at quit, one small file beside the configuration (data-model.md).
Everything else stays in the daemon's configuration, which the sandbox moves into the container.

**Testing**: integration tests drive the real `midi-harbor` binary with
`APP_SANDBOX_CONTAINER_ID` set, which is all the mode keys on: `service` commands exit 4 and name
"Start at login", `capabilities` reports service installation unavailable, and the daemon binds
`$TMPDIR/daemon.sock`. The supervisor's new stop is proved against the real daemon, releasing a
held note. One unit test maps each `SMAppService` status onto the switch, since `notFound` meaning
"off" is a fact about macOS (R-099). The AppKit behaviour, the login item and the sandboxed build
are proved by hand on this Mac (quickstart.md).

**Target Platform**: macOS 13 or newer for the App Store variant; macOS 11 for the direct build,
unchanged. Linux and Windows unchanged.

**Project Type**: desktop app and daemon, one binary.

**Performance Goals**: the window opens from the menu bar within 1 s (SC-A02), which hiding
rather than destroying it makes a redraw.

**Constraints**: `unsafe` only in `crates/platform`; the GUI stays a client of the contract; the
proto gains only the additive `StopDaemon` (protocol 1.1); no new `FailureReason` or exit code.

**Scale/Scope**: macOS only. About 700 lines of Rust across platform, service, core, gui and
main, plus packaging.

## Decisions

| Area | Decision | Research |
|---|---|---|
| Detecting the mode | `APP_SANDBOX_CONTAINER_ID` set means App Store mode | R-094 |
| The daemon | A helper, `Contents/MacOS/midi-harbor-daemon`, the headless build signed with `app-sandbox` and `inherit`; the app starts it as `daemon`, restarts it with the supervisor's pacing, stops it with SIGTERM and waits | R-095 |
| An orphaned daemon | A daemon already answering on the socket is used, not started again, and becomes the one Quit stops, through the contract's new `StopDaemon`, since the sandbox forbids signalling it | R-095 |
| The socket | `$TMPDIR/daemon.sock` when sandboxed, keeping every account name under 31 characters within the 103-byte limit | R-096 |
| Service commands | `service::detect()` fails in the sandbox with a message naming "Start at login"; the capability reports `NotBuilt` | R-097 |
| Dock icon | `LSUIElement` in the variant's Info.plist; `.regular` activation policy while the window is open, `.accessory` while closed | R-098 |
| Closing the window | `exit_on_close(false)`; close and Command-Q hide the window | R-098 |
| Menus | The app's own `NSMenu` over winit's default, and an `NSStatusItem`; their actions arrive as window messages through a channel | R-098 |
| Quitting | Everything goes through `terminate:`; the app's delegate answers `NSTerminateLater` until the daemon has stopped | R-098 |
| Second launch | `applicationShouldHandleReopen:` opens the window | R-098 |
| Login item | `SMAppService.mainApp`, status read back each time; `notFound` shows off | R-099 |
| Login launch | The launch event's `keyAELaunchedAsLogInItem`, read in `applicationDidFinishLaunching:` | R-099 |
| The variant | `build.sh --app-store`, on a Mac only, not GoReleaser; macOS 13, same bundle identifier | R-100 |

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

- **I. Resilience**: the app restarts a failed daemon with the supervisor's pacing, as launchd
  does for the direct build. A window crash leaves the daemon running. MIDI server recovery is
  the daemon's own and unchanged.
- **II. Daemon owns state, GUI is a client**: holds, with the helper. The daemon is a separate
  process and the window reaches it only over the contract. Principle II's list of what starts
  the daemon names launchd, systemd and Task Scheduler; this feature adds "the App Store app, as a
  bundled helper it supervises". That is an amendment, MINOR, 1.4.0, made in the first task.
- **III. Real-time safety**: untouched. Nothing new runs in the daemon, and the window and menus
  are in the other process.
- **IV. Platform parity**: the capability set is unchanged; service installation is reported
  unavailable with a reason in the App Store build, as Principle IV asks.
- **V. Protocol correctness**: unchanged.
- **VI. Testable without hardware**: the mode is keyed on one environment variable, so its
  behaviour through the CLI and daemon is tested on any Mac without signing. The AppKit shell
  cannot run without a desktop session and is verified by hand, as the GUI is.
- **VII. Observable**: the app logs when it starts, restarts and stops the daemon, with the
  reason, and the window says why when the daemon cannot start.
- **Quality gates**: unchanged; the App Store variant is built and checked by hand on a Mac.

**Result**: passes, with the Principle II amendment recorded as task work. Re-checked after the
design below: no change.

## Project Structure

### Documentation (this feature)

```text
specs/014-mac-app-store-mode/
├── plan.md              # This file
├── research.md          # R-094 to R-100
├── data-model.md        # Window state at quit, login item status
├── quickstart.md        # Validation on a Mac
├── contracts/
│   ├── menus.md         # Menu bar menu, menu bar item, shortcuts
│   ├── cli.md           # service commands and capabilities in App Store mode
│   └── bundle.md        # App Store variant layout and entitlements
└── tasks.md             # /speckit-tasks
```

### Source Code (repository root)

```text
crates/core/src/paths.rs            sandboxed(); the socket at $TMPDIR/daemon.sock when sandboxed
crates/platform/src/appkit/         macOS only, the one place with AppKit and ServiceManagement FFI
├── mod.rs                          safe API the window calls; ShellAction
├── delegate.rs                     NSApplicationDelegate: terminate, reopen, login launch
├── menus.rs                        the menu bar menu and the menu bar item
├── dock.rs                         activation policy
└── login_item.rs                   SMAppService.mainApp
crates/platform/src/capability.rs   service installation unavailable when sandboxed
proto/midiharbor/v1/harbor.proto    StopDaemon, protocol 1.1
crates/service/src/lib.rs           detect() refuses in the sandbox, naming Start at login
crates/service/src/supervisor.rs    a supervisor that can be told to stop its daemon
crates/gui/src/app_store.rs         the mode: owning the daemon, the window state, the shell messages
crates/gui/src/app.rs               close hides, shell messages, Start at login in Settings
crates/gui/src/onboarding.rs        in the mode, no service offer; starting or why it failed
src/main.rs                         in the mode, sets up the shell before the window runs
packaging/macos/build.sh            --app-store
packaging/macos/bundle.sh           the helper, the entitlements, the variant's Info.plist
packaging/macos/app-store.entitlements
packaging/macos/helper.entitlements
docs/installation.md, docs/platforms.md
tests/app_store_mode.rs             the mode through the real CLI and daemon
crates/daemon/tests/supervisor.rs   stopping a supervised daemon releases held notes
```

**Structure Decision**: the AppKit code joins the platform crate as a macOS-only module with no
trait, since it has one implementation and is not a seam. The mode's logic lives in the GUI
crate, which already depends on the service crate; `src/main.rs` only calls into it.

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| A second executable in the App Store bundle | The sandbox kills the app's own executable started as a child (R-095) | The daemon in the app's process breaks Principle II |
| An `NSApplicationDelegate` of the app's own | Quitting must wait for the daemon; reopen and login launch are only told to a delegate | winit's notifications arrive after AppKit has decided to exit |

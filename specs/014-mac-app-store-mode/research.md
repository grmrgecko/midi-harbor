# Research: Mac App Store mode

**Feature**: 014-mac-app-store-mode | **Date**: 2026-09-27

Numbered after the Windows port's research (013), whose last entry is R-093. Findings marked **VERIFIED**
were measured on the development Mac (macOS 15, Darwin 24.6, Apple silicon) with the app
bundle from `packaging/macos/build.sh`, re-signed ad hoc with the entitlements below. Scratch
bundles lived in `target/sbx/`.

---

## R-094: How the app knows it is sandboxed, and where the sandbox puts things

**Status**: **VERIFIED** (2026-09-27).

**Decision**: App Store mode is on exactly when `APP_SANDBOX_CONTAINER_ID` is set. The
configuration, the socket and every other path keep coming from the same resolution as today,
which the sandbox already redirects into the container.

**Evidence**: A probe run as the executable of a bundle signed with
`com.apple.security.app-sandbox`, launched from Terminal and through `open`, printed:

```text
APP_SANDBOX_CONTAINER_ID=com.mrgeckosmedia.MidiHarbor
HOME=/Users/user/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data
TMPDIR=/Users/user/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/tmp/
```

Unsandboxed, the variable is unset and `HOME` and `TMPDIR` are the user's own. The real binary,
signed the same way, reported its configuration at
`~/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/Library/Application Support/midi-harbor/config.yaml`
with no code change, so the direct-download build and the App Store build never share a setup.

A standalone executable signed with the sandbox entitlement and no bundle dies at launch with
SIGTRAP (exit 133), so the mode only ever arises inside the app bundle.

**Rejected**: Reading the entitlement back with `SecTaskCopyValueForEntitlement`. It answers the
same question through Security framework FFI, where the environment variable is set by the
sandbox itself before `main`.

---

## R-095: The daemon runs as a helper the app starts, not inside the app

**Status**: **VERIFIED** (2026-09-27); decided with the owner.

**Decision**: The App Store bundle carries a second executable,
`Contents/MacOS/midi-harbor-daemon`, a headless build of the same program signed with
`com.apple.security.app-sandbox` and `com.apple.security.inherit`. The app starts it as
`midi-harbor-daemon daemon`, restarts it after a failure with the supervisor's pacing, and stops
it when the user quits Midi Harbor entirely. A daemon already answering on the socket, left
running by a window that crashed, is used rather than started again.

**Evidence**: A sandboxed app that `posix_spawn`s its own executable sees the child die with
SIGTRAP before `main`: a process carrying the sandbox entitlement cannot start inside another
sandbox. The same parent spawning a copy signed with `app-sandbox` and `inherit` ran it to
completion, inside the parent's container:

```text
parent sandbox=com.mrgeckosmedia.MidiHarbor
child sandbox=com.mrgeckosmedia.MidiHarbor tmp=/Users/user/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/tmp/
```

**Why a helper**: Constitution Principle II requires the daemon to run independently of the GUI
process, so that a window that crashes does not drop a connection. The owner first asked for the
daemon inside the app, and chose the helper once the conflict and the tested alternative were
put to them. With the helper, a libcosmic or wgpu crash takes the window and leaves every port
and session running, and relaunching the app finds the daemon answering. Recovery from a dead
MIDI server (R-079) keeps working as today: the daemon replaces itself with `exec`, inside the
same sandbox, without the window going anywhere.

**Stopping a daemon the app did not start**: through the contract's `StopDaemon`, added for it
in protocol 1.1. The plan was to find the daemon's process from the socket and signal it:
`getsockopt(SOL_LOCAL, LOCAL_PEERPID)` on a connection to the daemon's socket did return 64149,
the process identifier `pgrep` gave for the development Mac's running daemon. But the sandbox
forbids that signal. **VERIFIED** (2026-09-27): the sandboxed app, relaunched after `kill -9` of an
earlier instance, attached to the daemon that instance had started, and on Quit left it running;
the same daemon stopped at once on SIGTERM from outside the sandbox. A sandboxed app may signal
the helper it starts, which the supervisor does, but not one it merely found. The owner chose a
stop request over the socket, already limited to the user, over a stop file in the container or a
daemon that dies with its app, which would break Principle II.

**Why headless**: the helper never draws, and the headless build is about 14 MB per architecture
against 36 MB for the full one, with none of the GUI toolkit loaded into the process that carries
MIDI.

**Rejected**: The daemon in the app's process, which the owner first described. It breaks
Principle II, and recovering from a dead MIDI server would mean re-executing the whole app,
window included. A launchd agent registered with `SMAppService.agent`: it would outlive the app
after the user quits, which App Review guideline 2.4.5(iii) forbids without separate consent,
and it adds an "Allow in the Background" approval for no benefit over the helper.

---

## R-096: The socket's path in the container is close to the limit

**Status**: **VERIFIED** (2026-09-27).

**Decision**: Sandboxed, the socket is `$TMPDIR/daemon.sock`, in the container's own `tmp`,
without the `midi-harbor` directory the unsandboxed build uses inside the shared temporary
directory. The container is already private to Midi Harbor, so the directory adds nothing.

**Evidence**: macOS limits a socket path to 103 bytes. The standard layout in the container is
`/Users/<name>/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/tmp/midi-harbor/daemon.sock`,
97 bytes for the development account's nine-letter name, so any account name over 15 characters
could not bind it. Without the directory the limit is 27 characters. (Measured when the
identifier was `dev.midiharbor.MidiHarbor`, three bytes shorter; the figures are for the current
one.) The sandboxed daemon bound
and served the standard layout on this Mac, with the command line reaching it from the same
sandbox.

---

## R-097: What works inside the sandbox

**Status**: **VERIFIED** (2026-09-27) for everything but Bluetooth and hardware, which are
checked from the app in quickstart.md.

The real daemon, signed with the entitlements in R-100 and run from Terminal, with the command
line run from the same bundle:

- Created a virtual port "SBX Test"; an unsandboxed CoreMIDI client listed it as a source and a
  destination.
- Created a network port "SBX Net", which bound UDP port 60868 and was advertised: `dns-sd -B
  _apple-midi._udp` on the same Mac listed it on four interfaces.
- `network discover` found four machines on the LAN, so browsing on raw multicast works.
- `capabilities` reported every capability available, including "service installation", which
  App Store mode must report unavailable (FR-A03).
- SIGTERM stopped it through its graceful shutdown, and the port left CoreMIDI.

**Decision**: "service installation" is reported unavailable in App Store mode with the existing
reason `NotBuilt`, and `service` commands exit with the existing code 4 and a message naming
"Start at login". Neither needs a new variant, so neither changes the contract.

Bluetooth was not started from Terminal, since its permission prompt belongs to the app.

---

## R-098: The menu bar item, the Dock icon, the menu and quitting

**Status**: **VERIFIED** (2026-09-27) running, but for the launch at login, which needs the owner
(R-099).

**Decisions**:

- **The Dock icon** follows the window through `NSApplication.setActivationPolicy`: `.regular`
  while the window is open, `.accessory` while it is closed. The App Store variant's Info.plist
  sets `LSUIElement`, so a login launch with the window closed never shows a Dock icon, even
  briefly; opening the window switches to `.regular` and activates the app.
- **The window** is hidden rather than destroyed when closed. libcosmic's
  `Settings::exit_on_close(false)` stops a close request from exiting (it otherwise returns
  `iced::exit` for the main window, `src/app/cosmic.rs` line 1236), and the app's
  `on_close_requested` hides the window. Reopening shows it again with its state intact, which is
  what makes SC-A02's one second easy to meet.
- **The menu bar menu** replaces winit's default menu, whose Quit item sends `terminate:` on
  Command-Q (`winit-appkit/src/menu.rs`). The app installs its own `NSMenu` once the event loop
  runs: the app menu with About, "Close to Menu Bar" on Command-Q, Hide on Command-H, and "Quit
  Midi Harbor" on Command-Option-Q, which sends `terminate:`; and a Window menu with Minimize and
  Close. Installed from the window's `init`, it was replaced: `init` runs before the event loop,
  and winit builds its default menu as launching finishes. It is installed from the first message
  the window handles instead.
- **Command-Shift-Q**, which the owner first asked for, is macOS's Log Out, in the Apple menu, and
  the Apple menu takes it before the app's menu does. Sent to the running app, it opened the "quit
  all applications and log out" dialog, which was cancelled. The owner chose Command-Option-Q.
- **The menu bar item** is an `NSStatusItem` with a menu of "Open Midi Harbor" and "Quit Midi
  Harbor". Its image is the anchor from the app icon alone, drawn as a template image: a template
  keeps only the alpha, and the full icon's opaque tile would be a plain square.
- **Quitting** goes through `terminate:` whatever starts it (the menu, the menu bar item, logout,
  restart, shutdown). The app sets its own `NSApplicationDelegate`, which winit 0.31 leaves free
  (it listens for launch and termination through notifications, `event_loop.rs` lines 200 to
  227), and answers `applicationShouldTerminate:` with `NSTerminateLater` until the helper has
  stopped, so releasing held notes is never skipped. The same delegate answers
  `applicationShouldHandleReopen:hasVisibleWindows:`, which is how a second launch from Finder,
  and a click on the Dock icon, reach the running app (FR-A11).
- **Actions** from AppKit reach the window as messages: the Objective-C target of each menu item
  sends on a channel that an iced subscription reads, so nothing in AppKit calls into the window's
  state directly.

**Evidence**, the sandboxed debug variant on the development Mac, driven through System Events:

- Launched from Finder, it started its helper on `…/Data/tmp/daemon.sock`, showed the window with
  the Dock icon (`lsappinfo`: `Foreground`), and listed "Open Midi Harbor" and "Quit Midi Harbor"
  in its menu bar item.
- Command-Q left the app and the daemon running, with no window and no Dock icon (`UIElement`).
  "Open Midi Harbor" brought the window back in 0.49 s, measured from the click with AppleScript's
  own delay included, with its content current and the Dock icon back.
- `kill -9` of the app left the daemon running; relaunching attached to the same daemon.
- Opening the app from Finder while it ran with its window closed brought the window back and
  started no second process.
- The menu read back as contracts/menus.md, Quit on Command-Option-Q (`AXMenuItemCmdModifiers` 2).
  Quit from either menu ended the app and a daemon it started in 2.2 s, and, with the helper built
  for protocol 1.1, a daemon it found running in 2.2 s too. `window.yaml` recorded `open: true`.

All of the AppKit and ServiceManagement calls live in `crates/platform`, the only crate where
`unsafe` is allowed. The window calls a safe API there.

---

## R-099: Starting at login, and knowing that it did

**Status**: **VERIFIED** (2026-09-27).

**Decision**: "Start at login" registers and unregisters `SMAppService.mainApp`. Its status is
read back from macOS every time it is shown, never stored. `.enabled` shows on,
`.requiresApproval` shows waiting with a pointer to System Settings > General > Login Items, and
both `.notRegistered` and `.notFound` show off, with `register()` deciding whether it can be
turned on.

A launch at login is recognised from the launch Apple event: its `keyAEPropData` is
`keyAELaunchedAsLogInItem`. The app reads it in `applicationDidFinishLaunching:` on its own
delegate, the only place the event is current.

**Evidence**: A sandboxed bundle signed ad hoc read `SMAppService.mainApp.status` as `.notFound`,
from Terminal and through `open`, before ever registering. Apps on current macOS report the same
for a correctly installed copy that has never registered, and treating `.notFound` as unavailable
leaves the switch stuck off, since `register()` is the only way out of that state
([martonpaulo/mailbell#49](https://github.com/martonpaulo/mailbell/issues/49),
[Apple's documentation of `notFound`](https://developer.apple.com/documentation/servicemanagement/smappservice/status-swift.enum/notfound?language=objc)).
The login-launch check is the one used with `SMAppService.mainApp`
([hisaac.net](https://hisaac.net/blog/how-to-detect-if-your-macos-app-was-launched-as-a-login-item/),
[kernova#1129](https://github.com/nicholas-lonsinger/kernova/issues/1129)).

**Open**: Registering from an ad hoc signed sandboxed build, and the login-launch check at a real
login, are verified with the owner (quickstart.md, scenario 5).

---

## R-100: The App Store variant of the bundle

**Status**: **VERIFIED** (2026-09-27) for the entitlements in R-094 to R-097.

**Decision**: `packaging/macos/build.sh --app-store` builds the variant on a Mac: the full binary
as the app and the headless binary as `midi-harbor-daemon`, both universal when both Rust targets
are installed. It is not built by GoReleaser, since an App Store upload needs Apple's own signing
and packaging tools, which run on a Mac. It differs from the direct-download bundle in:

| | App | Helper |
|---|---|---|
| `com.apple.security.app-sandbox` | yes | yes |
| `com.apple.security.inherit` | | yes |
| `com.apple.security.network.client` | yes | inherited |
| `com.apple.security.network.server` | yes | inherited |
| `com.apple.security.device.bluetooth` | yes | inherited |
| `com.apple.security.device.usb` | yes | inherited |

and in Info.plist, `LSMinimumSystemVersion` 13.0 and `LSUIElement` true. It keeps the bundle
identifier `com.mrgeckosmedia.MidiHarbor`. Signing is ad hoc unless `MIDI_HARBOR_SIGNING_IDENTITY`
names a certificate; a provisioning profile and App Store packaging are the owner's to add.

CoreMIDI needs no entitlement: virtual ports and other applications' ports worked sandboxed with
only the ones above (R-097). USB is declared because the owner asked for USB access; whether
reading a USB MIDI device's serial number and topology through IOKit needs it is checked with
hardware in quickstart.md, and the entitlement is dropped if nothing does.

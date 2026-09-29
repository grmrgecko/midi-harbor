# Feature Specification: Mac App Store mode

**Created**: 2026-09-27

**Status**: Implemented

**Input**: User description: "For the Mac side, can we add an option when it determines that its
running in the MacOS container (when ran from mac app store) that it will instead of running the
daemon separate, run it as part of the GUI app and have it change the function to where it hides
its app icon from the doc and moves to the status menu? Also there are no menu items, for this I
think it would change cmd-q to quit to status menu, and cmd-shift-q to quit entirely. For on boot,
instead of launchd it'll ask for a login item to be created and remember the state of the gui
being open or closed when it was quit. The status menu would allow choosing quit or open GUI, when
the gui is up it should restore the dock icon."

Midi Harbor on macOS is a daemon registered with launchd plus a window that is one of its clients.
An app distributed through the Mac App Store runs inside the App Sandbox, where an app cannot
register its own launchd agent. This feature gives that build its own way of running: the app
starts and stops the daemon itself, the app lives in the menu bar while its window is closed, and
it starts at login as a login item. Everything in specs 001 to 013 applies unchanged to the direct-download
build and to Linux and Windows.

**Decided 2026-09-27**: the mode is for the App Store build only, and is chosen by the app
detecting that it runs sandboxed, not by a setting. The App Store build requires macOS 13 or
newer, the first release where a sandboxed app can register itself as a login item without a
helper app; the direct-download build keeps macOS 11. The project adds a sandboxed build variant,
signed ad hoc, so the mode can be run and tested before the owner has App Store signing.

**Decided 2026-09-27, in implementation**: quitting entirely is Command-Option-Q, not
Command-Shift-Q as first asked. Command-Shift-Q is macOS's own Log Out shortcut in the Apple menu,
which takes precedence over any app's menu (research R-098).

**Decided 2026-09-27, in planning**: the daemon stays a separate process, which the app starts
from a copy of the program bundled for it, rather than running inside the app's own process. The
owner first asked for it inside the app; a crash in the window would then drop every connection,
which Principle II forbids, and the sandbox refuses to start the app's own executable as a second
process, so the copy is what makes a separate daemon possible (research R-095).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Connections keep running from the menu bar (Priority: P1)

A musician installs Midi Harbor from the App Store, opens it, sets up a virtual port and a network
session, and closes the window. The ports and sessions keep working, Midi Harbor's icon leaves the
Dock, and a Midi Harbor item stays in the menu bar. Choosing "Open Midi Harbor" from it brings the
window and the Dock icon back.

**Why this priority**: With no launchd service, the running app is what keeps the daemon alive.
If closing the window stopped MIDI, the product would lose the property it exists for.

**Independent Test**: Launch the sandboxed build, create a virtual port routed to a network
session with another machine, close the window, and play into the port from another application:
the far machine keeps receiving, the Dock shows no Midi Harbor icon, and the menu bar item reopens
the window with its Dock icon.

**Acceptance Scenarios**:

1. **Given** the sandboxed app is running with its window open, **When** the user closes the
   window, **Then** every endpoint and route keeps running, the Dock icon disappears, and the menu
   bar item remains.
2. **Given** the window is closed, **When** the user chooses "Open Midi Harbor" from the menu bar
   item, **Then** the window opens showing current state and the Dock icon returns.
3. **Given** the window is open, **When** the user chooses "Quit" from the menu bar item, **Then**
   the app stops its daemon through its graceful shutdown, releasing sounding notes, and exits.
4. **Given** the sandboxed app starts, **When** it checks for a daemon, **Then** it starts its own,
   or uses one already running from before, and never offers to install a launchd service.

---

### User Story 2 - Quitting the window versus quitting Midi Harbor (Priority: P1)

The app has a standard menu bar menu. Command-Q closes the window and leaves Midi Harbor in the
menu bar with every connection running. Command-Option-Q quits Midi Harbor entirely.

**Why this priority**: Command-Q is the reflex for "I'm done with this window". Quitting the
whole app on it would drop every connection by accident.

**Independent Test**: With the window focused, press Command-Q and confirm connections still pass
MIDI and the menu bar item remains; reopen, press Command-Option-Q, and confirm the process has
exited and its ports are gone from other applications.

**Acceptance Scenarios**:

1. **Given** the window is focused, **When** the user presses Command-Q, **Then** the window
   closes, the Dock icon disappears, and connections keep running.
2. **Given** the window is focused, **When** the user presses Command-Option-Q, **Then** Midi
   Harbor quits entirely, stopping its daemon.
3. **Given** the window is focused, **When** the user opens the app menu, **Then** it lists a
   close-to-menu-bar item with Command-Q, a quit item with Command-Option-Q, and the standard Hide
   and window items, each with its shortcut shown.
4. **Given** macOS asks the app to quit, at logout, restart or shutdown, **When** the request
   arrives, **Then** Midi Harbor quits entirely rather than hiding.

---

### User Story 3 - Starting at login (Priority: P2)

The user turns on "Start at login". After the next login Midi Harbor starts in the menu bar with
its connections restored, and opens its window only if the window was open when Midi Harbor last
quit.

**Why this priority**: A studio machine should bring its MIDI setup back without anyone opening
an app. It comes after P1 because the app is useful, if less convenient, without it.

**Independent Test**: Turn on "Start at login", quit with the window open, log out and in: the
window is open. Quit with the window closed, log out and in: only the menu bar item appears. In
both cases the ports and sessions come back.

**Acceptance Scenarios**:

1. **Given** the sandboxed app is running and not registered to start at login, **When** the user
   opens it for the first time, **Then** it offers once to start at login, saying what that does.
2. **Given** the user turns on "Start at login", **When** macOS needs the user's approval for it,
   **Then** the app says so and where to approve it, and shows the setting as waiting.
3. **Given** Midi Harbor quit with its window open, **When** it starts at login, **Then** it opens
   the window.
4. **Given** Midi Harbor quit with its window closed, **When** it starts at login, **Then** it
   shows only the menu bar item.
5. **Given** the user starts Midi Harbor by hand from Finder or Launchpad, **When** it starts,
   **Then** it opens the window, whatever state it quit in.
6. **Given** "Start at login" is on, **When** the user turns it off, **Then** Midi Harbor no
   longer starts at login.

---

### User Story 4 - A sandboxed build to test with (Priority: P2)

A developer builds the App Store variant on a Mac, signed ad hoc, and runs it to exercise
everything above before any App Store signing exists.

**Why this priority**: The mode cannot be run, and so cannot be proved, without a sandboxed
build.

**Independent Test**: Build the variant, confirm the bundle runs sandboxed, and run the checks in
User Stories 1 to 3 against it.

**Acceptance Scenarios**:

1. **Given** a Mac with the build tools, **When** the developer builds the App Store variant,
   **Then** it produces an app bundle that runs sandboxed, declaring only the access Midi Harbor
   uses: network client and server, Bluetooth, and USB devices.
2. **Given** the variant runs sandboxed, **When** the user creates virtual ports, joins network
   sessions, uses hardware and scans for Bluetooth devices, **Then** each works as in the
   direct-download build, or is reported unavailable with the reason.

### Edge Cases

- The daemon cannot start, because its configuration is from a newer build or its socket cannot
  be bound: the window says why, and "Quit" still works.
- A second copy of the app is launched while one runs: the running one opens its window and the
  second exits, so two daemons never run in one sandbox.
- The direct-download build's launchd daemon is also running on the same Mac: both run with
  separate setups and the user sees two sets of ports; neither can manage the other.
- The window is closed while a dialog is open or an action is in flight: the action finishes, and
  the window opens on the current state next time.
- The user denies the login item in System Settings: the setting shows it as off, with the
  reason.
- macOS sleeps with the window closed: the daemon handles sleep and wake as it does under
  launchd.
- A capability the sandbox withholds, such as the MIDI server recovery that needs another
  process: it is reported unavailable with the reason, and the rest runs.
- The configuration is kept inside the app's container, so it is not the file the
  direct-download build uses, and `midi-harbor config path` run from the app reports where it is.
- The window crashes: the daemon keeps every connection running, and the next launch finds it
  and shows its state. The daemon crashes: the app starts it again, as launchd would.
- The MIDI server dies: the daemon replaces itself as it does under launchd (008-resilience), and the
  window shows it as unreachable for those seconds.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-A01**: The app MUST decide at launch whether it runs sandboxed, and MUST use App Store mode
  exactly when it does. The direct-download build, Linux and Windows MUST behave as before.
- **FR-A02**: In App Store mode the app MUST start the daemon as a separate process of its own,
  restart it after a failure, and stop it when Midi Harbor quits entirely. The daemon serves the
  same contract on its socket inside the app's container, and the window remains a client of it.
  A daemon already running when the app starts MUST be used rather than a second one started.
- **FR-A03**: In App Store mode the app MUST NOT register, start, or offer a launchd service, and
  `service` commands MUST say that the App Store build starts through a login item instead, but
  for `service stop`, which stops the running daemon as the app's Quit does.
- **FR-A04**: The app MUST show a menu bar item while it runs in App Store mode, with "Open Midi
  Harbor" and "Quit".
- **FR-A05**: The app MUST show its Dock icon while its window is open and hide it while the
  window is closed.
- **FR-A06**: Closing the window, by its close button or Command-Q, MUST leave every endpoint and
  route running.
- **FR-A07**: The app MUST provide a menu bar menu with a close-to-menu-bar item on Command-Q and
  a quit item on Command-Option-Q, plus the standard Hide and window items.
- **FR-A08**: Quitting entirely, from the menu bar item, Command-Option-Q, or a logout, restart or
  shutdown, MUST stop the daemon through its graceful shutdown, including releasing sounding notes
  (008-resilience, FR-026), before the app exits.
- **FR-A09**: The app MUST let the user turn starting at login on and off, MUST offer it once on
  first launch, and MUST report when macOS is waiting for the user's approval.
- **FR-A10**: The app MUST remember whether its window was open when it last quit, and when
  started at login MUST open the window only if it was; started any other way, it MUST open the
  window.
- **FR-A11**: Only one copy of the app MUST run at a time; a second launch MUST bring the first
  one's window forward and exit.
- **FR-A12**: A capability the sandbox withholds MUST be reported unavailable with the reason, as
  any other unavailable capability is (002-configuration, Principle IV).
- **FR-A13**: The project MUST build a sandboxed App Store variant of the app bundle, signed ad hoc
  by default, declaring network client and server, Bluetooth and USB device access and nothing
  more, and requiring macOS 13 or newer.
- **FR-A14**: A crash of the window MUST NOT disturb any connection, and relaunching the app MUST
  show the daemon's current state (Principle II).

### Key Entities

- **Window state at quit**: whether the window was open when Midi Harbor last quit, kept per user
  inside the app's container and read at the next launch.
- **Login item**: the app's registration to start at login, owned by macOS and read back from it
  rather than stored by Midi Harbor.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-A01**: With the window closed for 10 minutes, MIDI sent into a virtual port keeps reaching
  a network peer with no message lost that the network did not lose.
- **SC-A02**: The window opens from the menu bar item within 1 second, showing current state.
- **SC-A03**: After a login with "Start at login" on, the ports and sessions are back as fast as
  the direct-download build's launchd daemon brings them back, and the window is open exactly when
  it was open at quit, in every combination tried.
- **SC-A04**: Command-Q never stops a connection, and Command-Option-Q always ends the process, in
  every trial.
- **SC-A05**: The direct-download build passes its existing gates unchanged.

## Assumptions

- The App Store build is signed and provisioned by the owner later; this feature proves the mode
  with an ad hoc signed sandboxed build.
- Clicking the window's close button behaves like Command-Q, closing to the menu bar.
- Settings gains the "Start at login" switch in App Store mode, in place of the service offer the
  direct-download build shows when no daemon runs.
- Command-line use against the App Store build goes through the app's own executable, which runs
  in the same sandbox and so reaches the same socket; the plan verifies this.
- The menu bar item uses the app icon drawn as a template image, so it follows the menu bar's
  light and dark appearance.

# Feature Specification: Daemon Updates

**Created**: 2026-10-02

**Status**: Implemented; the window's part checked on Arch Linux under systemd on 2026-10-02.
Not yet run under launchd or Task Scheduler, or in the App Store build

**Input**: User question, "If I were to update the app while the daemon is running, would the GUI
auto restart the daemon so it can be updated as well?", and then: "When we build a GUI we also
build the daemon used. Maybe make an UUID at build and embed it in the daemon and the GUI, have
the GUI check that UUID to see if its the same as what it knows. If its not, re-install the
daemon. I believe re-install is the best method if someone installed the app image, then
downloaded the RPM (for an example) to have it remove the app image daemon and install its own.
Things like that should be considered so we don't go into a daemon update loop."

Then, on restarting by itself: "Maybe instead of auto restarting we could popup saying `Daemon is
outdated` and have an update now button?"

The window and the daemon are one program, installed together, but the daemon keeps running the
copy it was started from. Updating the app left the old daemon running until the next login, and
nothing said so: a client checked only that both sides spoke the same major protocol version
(research R-108).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Being told the daemon is outdated, and updating it (Priority: P1)

A user updates Midi Harbor while its daemon is running, and opens the window. A notice above
every page says the daemon is outdated, with both versions, and offers **Update now**. Choosing
it registers this copy as the service and restarts the daemon from it; the ports and connections
come back as they do after any restart. **Not now** puts the notice away.

**Why this priority**: an update that does not reach the daemon is not an update, and nobody
would know.

**Independent Test**: Install the service from one build and open the window of another: the
notice shows, and after Update now the service is registered to run the window's copy and the
daemon reports the window's build.

**Acceptance Scenarios**:

1. **Given** the service running a daemon of another build, **When** the window connects,
   **Then** it shows the notice and changes nothing.
2. **Given** the notice, **When** the user chooses Update now, **Then** the window registers its
   own copy with the service manager, the daemon is restarted from it, and the window
   reconnects with no notice.
3. **Given** Midi Harbor installed a second way, as an AppImage and then a package of another
   release, **When** the package's window opens and the user chooses Update now, **Then** the
   service runs the package's copy from then on.
4. **Given** a daemon older than protocol 1.3, which reports no build, **When** a window
   connects, **Then** it is treated as outdated.
5. **Given** the App Store build, **When** the app starts and finds a daemon another build of
   the app left running, **Then** it stops that daemon and starts its own.

### User Story 2 - Nothing restarts by itself (Priority: P1)

Two copies of Midi Harbor are open at once. Neither replaces the other's daemon unless its user
asks, so the daemon is never restarted in a loop, and never while nobody is deciding.

**Why this priority**: every replacement drops each connection for a few seconds. Two windows
replacing each other's daemon by themselves would do that for as long as both were open.

**Independent Test**: With the service running one build, open the window of a second build and
leave it: the daemon keeps running, under the notice.

**Acceptance Scenarios**:

1. **Given** a daemon of another build, **When** nobody chooses Update now, **Then** the daemon
   is never restarted.
2. **Given** a daemon of a newer version than the window, **When** the window connects, **Then**
   the notice says it is newer and offers **Use this version** in place of Update now.
3. **Given** a daemon the service did not start, or a window pointed at one with `--socket`,
   **When** the window connects, **Then** the notice says how to use this version and offers no
   button.

### Edge Cases

- **The same release installed twice**: both copies carry one build identifier, so neither sees
  the other's daemon as different. If the registered copy is then deleted, the daemon stops at
  the next login and the window offers to register this copy, as it did before.
- **No window**: a machine with only the command line is told by `service status` that the daemon
  is another build, and `service install --start` replaces it.
- **Updating fails**: the window says why and reconnects to whatever is running, with the
  notice again if that is still another build.
- **The App Store build**: its app replaces a daemon left by another build as it starts, without
  asking. There the app runs the daemon and stops it on Quit, so one of another build is only
  ever left by a window that crashed, and starting the app is the user asking for it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-B01**: Every build MUST carry an identifier that is the same for everything in one
  package and differs from every other build, and the daemon MUST report it over the contract.
- **FR-B02**: The window MUST say when the daemon it reached is another build than itself, with
  both versions, and MUST offer to update it when the service is running that daemon.
- **FR-B03**: The window MUST NOT restart or replace the daemon unless the user asks.
- **FR-B04**: When the user asks, the window MUST register its own copy with the service manager
  and restart the daemon from it.
- **FR-B05**: `service install --start` MUST stop a daemon already running, so the daemon started
  is the program that was asked.
- **FR-B06**: `service status` MUST say when the running daemon is another build than the
  program asked.
- **FR-B07**: The App Store app MUST stop a daemon another build of the app left running and
  start its own.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-B01**: After an update, the first window opened says the daemon is outdated, and one
  click has the updated daemon running within ten seconds.
- **SC-B02**: However many copies of Midi Harbor are open, the daemon is never restarted without
  a user asking.

## Assumptions

- A user who installs Midi Harbor a second way and chooses Update now means to use the copy they
  opened.
- A notice above the page serves as the popup that was asked for: it shows over every page until
  answered, and does not take over a dialog the user has open.

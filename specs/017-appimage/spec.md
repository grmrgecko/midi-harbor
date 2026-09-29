# Feature Specification: AppImage

**Created**: 2026-09-29

**Status**: Implemented; built for x86_64 and arm64 and checked on 2026-09-29 on Arch Linux and
Ubuntu 22.04

**Input**: User request: "I'd like to see what it'll take to make an app image of this project. I
don't know if its something that'll allow a daemon process, so please research and see.", then
"If you think its feasible, do it."

The `.deb` and `.rpm` packages cover Debian, Ubuntu, Fedora and RHEL. Every other distribution had
only the `.tar.gz` of the bare binary, with no desktop entry and no libraries. A release now
carries an AppImage for each Linux architecture as well, which runs the graphical interface and
the daemon it registers with systemd.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Running Midi Harbor from an AppImage (Priority: P1)

A user on a distribution without a package downloads the AppImage, makes it executable and opens
it. The window offers to start the daemon at login, and from then on the daemon starts at every
login and after every crash, as it does when installed from a package.

**Why this priority**: it is the whole request.

**Independent Test**: On a Linux machine with a systemd user session, run
`service install --start` from the AppImage, then check the unit, the running daemon, a restart
after the daemon is killed, and an advertised network port.

**Acceptance Scenarios**:

1. **Given** the AppImage, **When** `service install` runs from it, **Then** the unit starts the
   AppImage file, not the executable inside its mount.
2. **Given** the service installed from the AppImage, **When** the daemon is killed, **Then**
   systemd starts it again from the AppImage.
3. **Given** the daemon running from the AppImage, **When** the service is stopped or the user
   logs out, **Then** the daemon shuts down as it does from a package, releasing its notes, and
   leaves no mount behind.
4. **Given** a distribution without the Avahi client libraries, **When** a network port is
   created, **Then** it is advertised, the AppImage carrying the libraries.
5. **Given** a newer AppImage moved over the registered one, **When** the daemon next starts,
   **Then** it runs the newer one.
6. **Given** a newer AppImage saved under another name and the old one deleted, **When**
   `service status` runs, **Then** it reports the registration stale, and `service install` from
   the new file repairs it.

### User Story 2 - Building the AppImages (Priority: P1)

`make snapshot` and `make release` build an AppImage for x86_64 and arm64 beside the other
artifacts, and the release publishes them.

**Independent Test**: Run the build and check both AppImages exist and start.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-I01**: `service install` run from an AppImage MUST register the AppImage file, and run from
  anything else MUST register the running executable, whatever AppImage variables it inherited.
- **FR-I04**: Stopping the service MUST let the daemon finish its shutdown before anything else in
  the unit is stopped.
- **FR-I02**: The release MUST build an AppImage for each Linux architecture it builds, from the
  same binary as the packages.
- **FR-I03**: The AppImage MUST run on the distributions the packages do, needing from the host only
  glibc, FUSE and what a desktop already has.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-I01**: From a downloaded AppImage, the daemon runs at login after one setup step, the same
  step as from a package.

## Assumptions

- The AppImage is for desktops. A server takes the headless archive or package, so there is no
  headless AppImage.
- Updates are by hand. AppImageUpdate's update information and `.zsync` file can be added later
  without changing the daemon.
- AppImage's portable mode, a `.home` or `.config` directory beside the file, is not supported:
  it moves where the unit is written, where systemd does not look.

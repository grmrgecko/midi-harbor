# Feature Specification: Midi Harbor — Service and Clients

**Created**: 2026-09-20

**Status**: Implemented

**Split 2026-09-27**: This was the whole of Midi Harbor's first specification. Each capability now
has a spec of its own, from 002-configuration to 010-graphical-interface, holding its user story,
requirements, success criteria, research and tasks under their original numbers; [the
index](../README.md) says which spec holds each. This one keeps the service, the command line, the
daemon contract, the architecture and the original input.

**Input**: User description: "Midi Harbor: a cross-platform (macOS + Linux) MIDI connectivity manager. It manages three kinds of MIDI endpoint from one place: (1) virtual MIDI ports — the IAC-driver equivalent — that let apps on the same computer talk to each other over MIDI; (2) RTP-MIDI network sessions that let computers exchange MIDI over a LAN, discovered via mDNS/Bonjour and interoperable with Apple Network MIDI, rtpMIDI on Windows, and rtpmidid on Linux; (3) Bluetooth LE MIDI links, both connecting out to BLE MIDI peripherals (keyboards, controllers) and advertising this computer as a BLE MIDI peripheral. The defining value is connection resilience: unlike the built-in macOS support, connections must self-heal — automatically reconnecting after network changes, sleep/wake, device unplug, peer restart, and Wi-Fi roaming — and must recover MIDI state so no notes are left stuck. Users configure named virtual ports that persist across reboots, define routes between any two endpoints, see live connection health and traffic, and get a clear reason when something genuinely cannot connect. A headless daemon runs at login and owns all connections; a libcosmic GUI and a CLI are clients of it."

**Amended 2026-09-20**: Ship as a single executable rather than separate daemon and GUI binaries.
The background service is a subcommand of that executable. The graphical interface is optional and
can be excluded at build time, leaving a fully functional headless command-line build. The
executable sets up its own launchd (macOS) or systemd (Linux) per-user service.

**Amended 2026-09-20 (2)**: Physical MIDI hardware attached to the computer (USB and DIN
interfaces, controllers, keyboards) is a first-class endpoint kind alongside virtual ports, network
sessions, and Bluetooth devices. This enables the repeater use case: take a physical device and
carry it over the network or over Bluetooth to another machine.

## User Scenarios & Testing *(mandatory)*

### User Story 2 - One binary: headless service, CLI, and optional GUI (Priority: P1)

A user installs a single Midi Harbor executable. Running it with no arguments opens the graphical
interface; running `midi-harbor service install` registers it as a user service that starts at
login and keeps running. On a headless machine — a rack computer, a stage box, a Raspberry Pi with
no desktop — the same executable is built without the graphical interface and driven entirely from
the command line, with the same configuration and the same self-healing behaviour.

**Why this priority**: This is the delivery vehicle for every other story. Virtual ports cannot
survive a reboot (User Story 1) without the service, and the graphical interface cannot be optional
unless the command-line surface is complete on its own.

**Independent Test**: On a machine with no graphical environment, install the service from the
command line, create a virtual port and a route, reboot, and verify from the command line that
everything came back — without the graphical interface ever being built or run.

**Acceptance Scenarios**:

1. **Given** a freshly installed executable, **When** the user runs the service install command,
   **Then** a per-user service is registered with the operating system's service manager, started
   immediately, and set to start at every login.
2. **Given** the service is installed, **When** the user runs the service status command, **Then**
   they are told whether it is installed, whether it is running, its version, and its uptime.
3. **Given** the service is installed, **When** the user runs the service uninstall command,
   **Then** the service is stopped and deregistered, and the user's configuration is left intact.
4. **Given** a build produced without the graphical interface, **When** the user runs any
   command-line command, **Then** it behaves identically to the same command on a full build.
5. **Given** a build produced without the graphical interface, **When** the user runs the
   executable with no arguments, **Then** they are shown command-line help explaining that the
   graphical interface is not included in this build.
6. **Given** the service is running, **When** the user performs any configuration action available
   in the graphical interface, **Then** an equivalent command-line command exists that performs
   the same action against the same running service.
7. **Given** the user wants to script against Midi Harbor, **When** they request machine-readable
   output from any command-line command, **Then** they receive structured output suitable for
   automated parsing.
8. **Given** the service is not installed, **When** the user opens the graphical interface,
   **Then** they are offered installation of the service in one action, and told what that will
   do.
9. **Given** the executable is run as the service directly in the foreground, **When** the user
   does so, **Then** it runs the service without registering anything, so it can be supervised by
   other means or debugged interactively.

---

### Edge Cases

- **Daemon not running**: When the user opens the interface and the background service is not
  running, the interface offers to start and install it rather than showing an empty or broken
  view.
- **Daemon crash**: If the background service terminates unexpectedly, it is restarted
  automatically by the operating system's service supervisor, and it restores all configured
  endpoints and routes on startup.
- **Version mismatch**: When the interface and the background service are different incompatible
  versions, the user is told clearly which component to update instead of experiencing undefined
  behaviour.
- **Concurrent clients**: When more than one interface or command-line client is connected at
  once, all of them see consistent state and updates.
- **No service manager**: When the platform's user service manager is unavailable or unsupported
  (for example a container, or a Linux system without the expected service manager), service
  installation reports this clearly and the user is told how to run the service in the foreground
  instead.
- **Service already installed**: When the user runs service installation and a registration
  already exists, it is updated in place rather than duplicated, and the user is told it was
  updated.
- **Stale service registration**: When a service registration points at an executable that has been
  moved or deleted, the status command reports the registration as stale and names the missing
  path.
- **Graphical interface requested on a headless build**: When a user asks for the graphical
  interface on a build that excludes it, they are told which build to install rather than seeing a
  crash or silent exit.

## Requirements *(mandatory)*

### Functional Requirements

#### Service architecture and clients

- **FR-036**: System MUST run a background service that owns all endpoints and connections and
  starts automatically at user login.
- **FR-037**: System MUST keep all connections operating when no graphical interface is running,
  and MUST NOT disturb any connection when an interface is opened or closed.
- **FR-038**: System MUST be restarted automatically by the operating system if it terminates
  unexpectedly, and MUST restore all configured endpoints and routes on startup.
- **FR-039**: System MUST ship as a single executable that selects its role from its command-line
  arguments — running the background service, running the graphical interface, performing a
  command-line action, or managing its own service registration. There MUST NOT be a separate
  daemon executable.
- **FR-039a**: System MUST run the background service as a subcommand of that executable, in the
  foreground when invoked directly, so it can be supervised externally or debugged interactively.
- **FR-039b**: System MUST make the graphical interface an optional component that can be excluded
  at build time, producing a fully functional headless build with no graphical dependencies.
- **FR-039c**: System MUST expose, through command-line commands, every configuration and control
  action available in the graphical interface, so the graphical interface is never required to
  operate any feature.
- **FR-039d**: System MUST offer machine-readable output for command-line commands, so the product
  can be scripted and automated.
- **FR-039e**: System MUST show command-line help, explaining that the graphical interface is not
  included, when a headless build is invoked with no arguments.
- **FR-039f**: System MUST provide commands to install, uninstall, start, stop, and report the
  status of its own per-user service registration, using the platform's native user service
  manager, and MUST NOT require the user to author service configuration files by hand.
- **FR-039g**: System MUST install its service as a per-user service that starts at login and is
  restarted by the operating system if it terminates unexpectedly.
- **FR-039h**: System MUST leave the user's configuration intact when its service is uninstalled.
- **FR-040**: System MUST support multiple clients connected at once, each seeing consistent state
  and receiving updates as state changes.
- **FR-041**: System MUST detect incompatible client and service versions and report clearly which
  component needs updating.
- **FR-042**: System MUST offer to install and start the background service in one action when a
  client finds it is not running, and MUST state what installing it will do.
- **FR-043**: System MUST operate without requiring administrator or root privileges.

### Key Entities

- **Endpoint**: Anything MIDI can flow to or from. Has a stable identity, a user-visible name, a
  kind (virtual port, network session, Bluetooth device), an enabled setting, a current state, and
  traffic counters.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-010**: The background service uses under 1% of one CPU core when idle with 10 configured
  endpoints, and under 150 MB of memory under normal use.
- **SC-014**: The background service continues running and carrying MIDI with zero interruption
  across 50 consecutive open-and-close cycles of the graphical interface.
- **SC-014a**: A build produced without the graphical interface has no graphical or display-server
  dependencies and passes the full command-line acceptance suite on a machine with no desktop
  environment installed.
- **SC-014b**: Every configuration and control action available in the graphical interface has a
  documented command-line equivalent, verified as 100% coverage.
- **SC-014c**: A user goes from a freshly downloaded executable to a service running at login with
  a working virtual port using a single install command plus one create command.
- **SC-016**: All features present on one supported operating system are present and behave
  equivalently on the other, except those explicitly documented as platform-limited.

## Assumptions

- Users are musicians, engineers, and hobbyists comfortable with MIDI concepts such as ports,
  channels, and controllers, but not necessarily with networking.
- Both supported operating systems are used on a single-user desktop where the person configuring
  Midi Harbor is the person logged in; multi-user and remote administration are out of scope.
- MIDI 1.0 message semantics are the baseline for routing, recovery, and monitoring. MIDI 2.0 is
  out of scope for this feature but the design should not preclude it.
- The graphical interface is excluded or included at build time rather than being downloaded or
  enabled at runtime, so distributions can offer a headless package without graphical dependencies.
- The platform's per-user service manager is assumed available: launchd on macOS, and systemd user
  units on Linux. Linux systems without systemd are supported only by running the service in the
  foreground under whatever supervisor the user prefers.
- Service installation is per-user and needs no elevated privileges, so it registers only for the
  user who runs it and does not start before that user logs in.
- Windows is not a target for this feature.
- Where the operating system already provides its own inter-application or network MIDI facility,
  Midi Harbor runs alongside it rather than replacing or reconfiguring it, and both may be in use
  at once.

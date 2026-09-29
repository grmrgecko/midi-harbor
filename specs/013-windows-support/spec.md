# Feature Specification: Windows support

**Created**: 2026-09-26

**Status**: Implemented

**Input**: User description: "Add windows support, use ssh user@192.0.2.47 (Windows machine) to
test, do cross compilation here because the VM is slow."

Midi Harbor ran on macOS and Linux, and the original specification declared Windows out of scope. This feature
makes Windows a first-class target: the same single executable, the same daemon, CLI, contract
and configuration, with each platform seam given a Windows implementation. Everything in specs
001 to 010 applies on Windows except where this document says otherwise.

**Decided 2026-09-26**: virtual ports on Windows go through Windows MIDI Services. They first
went through Tobias Erichsen's teVirtualMIDI driver, which loopMIDI and rtpMIDI install, but its
SDK page says software linking to it may not be distributed without his prior clearance, whatever
the project's licence. Windows MIDI Services is Microsoft's, and Windows carries its API
from its late-2026 update. Until then Midi Harbor uses the App SDK, which works with the service
Windows already has once the user installs Microsoft's runtime, and moves onto the in-box API on
any machine where Windows registers it (research R-093).

The service Windows shipped before that update never finishes closing a virtual device
(Microsoft's issue #1236), and answers nothing more until it is restarted. Microsoft fixed it for
the late-2026 update. Until a machine has that update, deleting, renaming or disabling a virtual
port, or stopping the daemon, leaves virtual ports unavailable until the Windows MIDI Service is
restarted.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Virtual ports between applications on Windows (Priority: P1)

A musician on Windows with Windows MIDI Services creates a Midi Harbor port named
"Sequencer Bus", and every MIDI application on the machine lists it and exchanges MIDI through it.

**Why this priority**: Virtual ports are the product's foundation on every platform.

**Independent Test**: With Windows MIDI Services available, create a port, open it from a second
process through WinMM, and pass notes and a system-exclusive dump both ways.

**Acceptance Scenarios**:

1. **Given** Windows MIDI Services is available, **When** the user creates a port, **Then**
   other applications list it, in the directions its connectors give.
2. **Given** neither Windows' own API nor the App SDK runtime is present, **When** the user asks
   what the machine can do, **Then** virtual ports are reported unavailable, naming what to
   install, and the daemon still runs everything else.
3. **Given** a port exists on Windows with the late-2026 update, **When** the user renames it,
   **Then** other applications list it under the new name at once.
4. **Given** the service stops answering, **When** the daemon closes or creates a port, **Then**
   it gives up within seconds, reports virtual ports unavailable until the service is restarted,
   and keeps running everything else.

---

### User Story 2 - Hardware and other applications' ports on Windows (Priority: P1)

A USB MIDI controller, or a port another application created, appears as a device, can be routed,
and is found again after it is unplugged and plugged back in.

**Independent Test**: Open a port another process created through the Windows backend, route MIDI
through it both ways, and see a device arriving and leaving reported.

**Acceptance Scenarios**:

1. **Given** hardware or another application's port is present, **When** devices are listed,
   **Then** it appears once, with its directions, marked as hardware or software.
2. **Given** the device list changes, **When** a second passes, **Then** the daemon is told to
   re-enumerate.
3. **Given** this daemon's own ports, **When** devices are listed, **Then** they are not offered
   back as devices, including one it has just destroyed that Windows still lists.

---

### User Story 3 - Network sessions from Windows (Priority: P1)

A Windows machine advertises its network ports on the local network, finds other machines' ports,
and joins sessions with them.

**Independent Test**: A Windows daemon discovers a Linux daemon's port, connects to it, and the
Linux side lists it; the Linux machine finds the Windows port by browsing.

**Acceptance Scenarios**:

1. **Given** a network port on Windows, **When** another machine browses, **Then** it finds and
   resolves the port, whatever characters its name contains.
2. **Given** a peer on IPv4, **When** the Windows daemon invites it, **Then** the session joins.

---

### User Story 4 - Background service on Windows (Priority: P2)

`midi-harbor service install --start` makes the daemon start at logon without elevation or a
visible window, come back after a crash, and stop cleanly with `service stop`.

**Acceptance Scenarios**:

1. **Given** a standard user, **When** they install the service, **Then** it registers without
   asking for administrator rights and starts at their next logon.
2. **Given** the daemon crashes, **When** two seconds pass, **Then** it is running again.
3. **Given** the service is running, **When** the user stops it, **Then** held notes are released
   and peers told goodbye before the process ends.

---

### User Story 5 - Sleep and wake on Windows (Priority: P2)

A suspend is noticed before it happens, and sessions reconnect on resume, as on the other
platforms.

---

### User Story 6 - Bluetooth and the interface on Windows (Priority: P3)

The Bluetooth central role works where there is a radio, and the peripheral role is reported
unavailable until it can be verified. The graphical interface runs where it builds.

### Edge Cases

- Neither Windows' own API nor the App SDK runtime is present.
- The service before the late-2026 update stops answering once a virtual device closes (R-093).
- That service names a port's WinMM ports after its groups, "Name" and "Name Gr 2", rather than
  after its connectors (R-093).
- A virtual device created from an SSH session, which the service never answers (R-093).
- A session name with a dot, or with characters outside ASCII (R-084).
- The network is marked Public, where Windows Firewall refuses unsolicited inbound traffic.
- Another local user tries to reach, or impersonate, the daemon's control channel (R-087).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-W01**: The project MUST cross-compile for x86_64 Windows from macOS or Linux, and the same
  quality gates MUST pass on Windows.
- **FR-W02**: Virtual ports MUST be created through Windows MIDI Services, through the API
  Windows carries when it is registered and otherwise through the App SDK runtime; without either
  the capability MUST be reported unavailable with the component named, and the daemon MUST still
  start. No call into the service may hold up the daemon for more than a bounded time.
- **FR-W03**: Hardware and other applications' ports MUST be reached through WinMM, identified by
  device interface path for hardware and by name for application ports.
- **FR-W04**: Arrivals and departures MUST be reported to the daemon within about a second.
- **FR-W05**: Renaming a port MUST show the new name to other applications at once, on Windows
  with the late-2026 update.
- **FR-W06**: The control channel MUST stay off the network and limited to the user: a named pipe
  that refuses remote clients, whose random name is readable only by the user.
- **FR-W07**: `service stop` MUST stop the daemon through its graceful shutdown.
- **FR-W08**: The service MUST start at logon, without elevation or a window, and MUST restart the
  daemon after a crash.
- **FR-W09**: Network ports MUST be advertised through the system's own mDNS responder and remain
  discoverable by other machines.
- **FR-W10**: Session sockets MUST accept IPv4 and IPv6 peers.
- **FR-W11**: Suspend and resume MUST be reported from the power manager, with the polled watcher
  underneath.
- **FR-W12**: Real-time rules MUST hold in the WinMM and Windows MIDI Services callbacks: no
  allocation, locking, I/O or logging.

### Key Entities

No new entities. The configuration file, contract and domain types are unchanged.

## Success Criteria *(mandatory)*

- **SC-W01**: The workspace's tests pass on Windows 11, and the Windows-only integration tests
  pass with Windows MIDI Services available; before the late-2026 update, each alone with the
  service restarted between them, and all but the rename test.
- **SC-W02**: A Windows daemon joins a session with a Linux daemon, and each discovers the other.
- **SC-W03**: A renamed port's new name is visible to another application within 3 s, on every
  rename.

## Assumptions

- Windows 10 or 11 on x86_64.
- Before Windows' late-2026 update, the user installs Microsoft's Windows MIDI Services SDK
  Runtime and Tools for virtual ports.
- No Windows machine with a Bluetooth radio or USB MIDI hardware was available; those paths are
  covered by the code shared with the other platforms and by tests without hardware.

# Feature Specification: Virtual Ports

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: Virtual MIDI ports that applications on the same computer use to talk to each other:
creating, renaming, deleting, switching off, their MIDI In and MIDI Out connectors, and their
identity across restarts.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Persistent virtual MIDI ports between local apps (Priority: P1)

A musician runs a DAW and a separate notation or lighting application on the same computer and
needs them to exchange MIDI. They open Midi Harbor, create a virtual port named "Sequencer Bus",
and both applications immediately see "Sequencer Bus" in their own MIDI device lists. After
rebooting the computer, the port is still there with the same name, and both applications
reconnect to it without the user doing anything.

**Why this priority**: This is the foundational capability and the direct replacement for the
platform's built-in inter-application MIDI. It delivers standalone value with no network or
Bluetooth involvement, and every other story depends on endpoints existing.

**Independent Test**: Create a named virtual port, verify two independent MIDI applications on
the same machine can see it and pass note and controller data between them, reboot, and verify
the port reappears automatically with its name and identity intact.

**Acceptance Scenarios**:

1. **Given** Midi Harbor is running with no ports configured, **When** the user creates a virtual
   port named "Sequencer Bus", **Then** other MIDI applications on the computer list an input and
   an output endpoint named "Sequencer Bus" without those applications being restarted.
2. **Given** a virtual port exists, **When** one application sends note and controller messages to
   it, **Then** another application connected to that port receives those messages byte-for-byte
   and in the original order.
3. **Given** virtual ports are configured, **When** the computer is restarted and the user logs in,
   **Then** every configured port is recreated automatically before the user opens any window.
4. **Given** a virtual port is in use by applications, **When** the user renames it, **Then** the
   user is warned that connected applications may need to reselect the port, and the rename is
   applied only after confirmation.
5. **Given** the user attempts to create a port with a name already in use, **When** they confirm,
   **Then** the system rejects the creation with a message naming the conflict.
6. **Given** the user deletes a virtual port that is carrying traffic, **When** they confirm the
   deletion, **Then** all notes sounding on that port are silenced before the port is removed.

---

### Edge Cases

- **Endpoint limit**: When the user tries to create more virtual ports than the operating system
  permits, they are told the limit has been reached rather than seeing a silent failure.

## Requirements *(mandatory)*

### Functional Requirements

#### Virtual MIDI ports

- **FR-001**: System MUST allow users to create, rename, and delete named virtual MIDI ports that
  other applications on the same computer can see and use as MIDI endpoints.
- **FR-002**: System MUST make each virtual port available as both an input and an output endpoint
  to other applications.
- **FR-002a**: System MUST let the user choose how many MIDI In and MIDI Out connectors a virtual
  port has, at least one and at most sixteen of each, show a port with several connectors of one
  kind to other applications as numbered ports, and let a route name one connector (R-078).
- **FR-003**: System MUST persist virtual port configuration and recreate every configured port
  automatically when the computer starts, without any user action or window being open.
- **FR-004**: System MUST preserve a stable identity for each virtual port across restarts and
  across renames, so that routes referencing it survive.
- **FR-005**: System MUST reject creation of a virtual port whose name collides with an existing
  port, and state the conflict. A virtual port also may not take a network port's name, nor a
  network port a virtual port's, on creation or rename, since other applications see a network
  port through its automatic port of the same name (FR-015h, R-078).
- **FR-006**: System MUST silence any sounding notes on a port before that port is deleted or
  disabled.
- **FR-007**: System MUST allow a virtual port to be enabled or disabled without deleting its
  configuration.

### Key Entities

- **Virtual Port**: An endpoint that exists purely on this computer so local applications can
  exchange MIDI. Defined by a name; persists across restarts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user can create a virtual MIDI port and see it appear in another application's MIDI
  device list in under 30 seconds from first opening the application, without consulting
  documentation.
- **SC-002**: 100% of configured endpoints and routes are restored and operational after a reboot,
  with no user action beyond logging in.
- **SC-008**: MIDI passing between two local applications through a virtual port arrives with
  additional delay of under 1 millisecond on average and under 3 milliseconds at the 99th
  percentile.

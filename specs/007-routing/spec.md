# Feature Specification: Routing

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: Routes that carry MIDI between any two endpoints, one way or both ways: validity, broken
routes that wait for their endpoints, loops on one machine and across machines, system-exclusive,
and the repeater use of carrying hardware over the network.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### User Story 4 - Routing and repeating MIDI between any two endpoints (Priority: P3)

A user has a physical MIDI keyboard plugged into a computer in one room and wants to play a sound
module attached to a computer in another room. They plug the keyboard in, Midi Harbor lists it
automatically, and they create a route from that physical device to a network session. The keyboard
is now effectively repeated onto the other machine, where a matching route delivers it to that
machine's physical output. The same mechanism carries a physical device over Bluetooth, merges a
network peer into a local virtual port for a DAW, or fans one controller out to several
destinations.

**Why this priority**: Routing is what turns four independent endpoint kinds into one system, and
the physical-device repeater is the case that makes Midi Harbor useful with hardware a user already
owns. It comes after the endpoint kinds themselves because it has nothing to connect until they
exist.

**Independent Test**: Plug in a physical MIDI device, create a route from it to another endpoint,
play the device, and verify the MIDI arrives at the destination; unplug and replug the device and
verify the route resumes on its own.

**Acceptance Scenarios**:

1. **Given** two endpoints exist, **When** the user creates a route from one to the other, **Then**
   MIDI received at the source is delivered to the destination.
2. **Given** a route exists, **When** the user disables it, **Then** MIDI stops being delivered
   along it and any notes it left sounding at the destination are silenced.
3. **Given** a route whose source endpoint is temporarily disconnected, **When** the endpoint
   reconnects, **Then** the route resumes delivering without being recreated.
4. **Given** a user creates routes that form a loop between endpoints, **When** MIDI enters the
   loop, **Then** the system prevents unbounded message multiplication and warns the user that a
   loop exists.
5. **Given** a route is configured, **When** the computer restarts, **Then** the route is restored
   automatically along with its endpoints.
6. **Given** a route whose destination is removed, **When** the endpoint no longer exists, **Then**
   the route is shown as broken with the missing endpoint named, and is restored if that endpoint
   returns.
7. **Given** a physical MIDI device is plugged into the computer, **When** the user views
   endpoints, **Then** the device is listed automatically with its hardware name and is available
   as a route source and destination.
8. **Given** a route from a physical MIDI device to a network session, **When** the user plays the
   device, **Then** the MIDI arrives on the remote machine with the same messages and ordering.
9. **Given** a route from a physical MIDI device to a network session, **When** the device is
   unplugged and later plugged back in, **Then** the route resumes automatically without being
   recreated and without leaving notes sounding on the remote machine.
10. **Given** a route from a network session to a physical MIDI output, **When** the session
    delivers MIDI, **Then** the attached hardware receives it, completing the repeater in both
    directions.
11. **Given** a route from a physical MIDI device to a Bluetooth endpoint, **When** the user plays
    the device, **Then** the MIDI is carried over Bluetooth to the connected device.
12. **Given** one physical device routed to several destinations, **When** the user plays it,
    **Then** every destination receives the MIDI, and a destination being unavailable does not stop
    delivery to the others.

---

### Edge Cases

- **Repeater loop across machines**: When routes on two machines are configured so MIDI is repeated
  back and forth over a network session, the loop is detected and message multiplication is
  prevented, as it is for local loops.
- **Large messages**: When a very large system-exclusive message is transferred, it is delivered
  intact across every transport or, where it cannot be, reported as rejected with a reason.

## Requirements *(mandatory)*

### Functional Requirements

#### Routing

- **FR-030**: Users MUST be able to create routes delivering MIDI from any endpoint to any other
  endpoint, including many sources to one destination and one source to many destinations.
- **FR-030a**: System MUST support routing between any combination of endpoint kinds, so that a
  physical MIDI device can be carried over a network session or a Bluetooth link, and a network or
  Bluetooth source can be delivered to physical MIDI hardware.
- **FR-030b**: System MUST continue delivering to the reachable destinations of a one-to-many route
  when some destinations are unavailable.
- **FR-031**: System MUST persist routes and restore them at startup.
- **FR-032**: System MUST keep a route intact while its endpoints are temporarily unavailable and
  resume delivery automatically when they return.
- **FR-033**: System MUST detect routing loops and prevent unbounded message multiplication, and
  MUST warn the user when a loop is configured.
- **FR-034**: Users MUST be able to enable or disable an individual route without deleting it.
- **FR-034a**: Users MUST be able to make a route carry MIDI both ways between two endpoints that
  both send and receive, as one route (R-078).
- **FR-035**: System MUST show a route whose endpoint is missing as broken, naming the missing
  endpoint.

### Key Entities

- **Route**: A directed delivery path from one endpoint to another. Has an enabled setting and a
  validity status reflecting whether both of its endpoints currently exist.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-010b**: MIDI played on a physical device routed over a network session arrives on the remote
  machine's physical output with under 10 milliseconds of added delay at the 99th percentile beyond
  raw network round-trip time.

## Assumptions

- Message transformation — filtering, channel remapping, transposition, velocity curves — is out of
  scope. Routes deliver messages unmodified.

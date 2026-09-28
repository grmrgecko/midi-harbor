# Feature Specification: Observability

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: Seeing what the system is doing: state and traffic for every endpoint, the event history,
the live MIDI monitor, and the diagnostic report.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### User Story 5 - Seeing connection health and diagnosing problems (Priority: P4)

A user notices their MIDI stopped working during a set. They open Midi Harbor and immediately see
which link is unhealthy, what happened, when it happened, and whether it recovered — without
reading a log file or restarting anything.

**Why this priority**: Diagnostics turn the resilience work into something users can trust and
verify, but the connections must exist and heal before there is anything to observe.

**Independent Test**: Induce a failure on a connection, then verify the interface shows the state
change, a specific reason, a timestamp, and the recovery, and that the history persists after the
window is closed and reopened.

**Acceptance Scenarios**:

1. **Given** endpoints are configured, **When** the user opens the interface, **Then** each
   endpoint shows its current state, how long it has held that state, and live traffic activity.
2. **Given** a connection fails and recovers while no window is open, **When** the user later opens
   the interface, **Then** they can see that the failure and the recovery occurred, with
   timestamps.
3. **Given** a connection is in a retrying state, **When** the user views it, **Then** they see the
   reason for the last failure and when the next attempt will happen.
4. **Given** any connection, **When** the user inspects it, **Then** they can see counts of
   messages sent, received, lost, and recovered.
5. **Given** a user wants to see exactly what MIDI is flowing, **When** they open a monitor on an
   endpoint, **Then** they see a live, human-readable stream of the messages passing through it.
6. **Given** a user is reporting a problem, **When** they ask for diagnostics, **Then** they can
   export a report containing configuration, connection history, and counters.

---

## Requirements *(mandatory)*

### Functional Requirements

#### Observability

- **FR-044**: System MUST expose, for every endpoint and connection, its current state, the
  duration of that state, the reason for the most recent failure, and the time of the next
  reconnection attempt when retrying.
- **FR-045**: System MUST maintain counters per connection for messages sent, received, lost, and
  recovered.
- **FR-046**: System MUST retain a history of connection state changes that survives the graphical
  interface being closed, so users can see what happened while unattended.
- **FR-047**: Users MUST be able to monitor the live MIDI message stream on any endpoint in
  human-readable form.
- **FR-048**: Users MUST be able to export a diagnostic report containing configuration, connection
  history, and counters.

### Key Entities

- **Event**: A timestamped record of a state change or notable occurrence on an endpoint, retained
  in a bounded history for diagnosis.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-012**: When a connection cannot be established for a reason the user must act on, 100% of
  such cases present a specific reason rather than a generic failure message.
- **SC-013**: A user diagnosing a dropped connection can determine when it dropped, why, and
  whether it recovered, entirely from the interface, without reading log files.

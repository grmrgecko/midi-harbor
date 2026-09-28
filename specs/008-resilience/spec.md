# Feature Specification: Resilience

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: Keeping connections alive without the user: detecting loss, retrying with backoff, sleep,
wake and network changes, releasing notes that were sounding and restoring controller state on
recovery, isolating one failing connection from the rest, and surviving the platform's MIDI service
dying.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### Edge Cases

- **Malformed input**: When a peer or device sends malformed or hostile data, it is discarded and
  counted without destabilising the service or affecting other connections.
- **Rapid connect/disconnect**: When a device or peer flaps repeatedly, retry backoff prevents the
  system from consuming excessive resources, and the user sees that the link is unstable.

## Requirements *(mandatory)*

### Functional Requirements

#### Resilience and self-healing

- **FR-022**: System MUST detect loss of any connection and begin automatic recovery without user
  action.
- **FR-023**: System MUST retry failed connections indefinitely using increasing delays with
  randomisation, up to a bounded maximum delay, and MUST NOT permanently abandon a connection the
  user has left enabled.
- **FR-024**: System MUST re-establish affected connections automatically after the computer sleeps
  and wakes.
- **FR-025**: System MUST re-establish affected connections automatically when network interfaces
  or addresses change.
- **FR-026**: System MUST silence sounding notes on a link when that link is lost, and MUST NOT
  leave notes sounding after any recovery.
- **FR-027**: System MUST restore controller and program state on a recovered link so that the
  receiving side is not left with stale values.
- **FR-028**: System MUST distinguish transient failures, which it retries silently, from
  conditions requiring a user decision, which it surfaces with a specific and actionable reason.
- **FR-029**: System MUST continue operating all healthy connections while any other connection is
  failing or retrying.

### Key Entities

- **Connection State**: The lifecycle position of an endpoint or session — for example
  disconnected, connecting, connected, retrying, or unavailable — together with the time it entered
  that state and the reason it got there.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-004**: After the computer wakes from sleep, all previously connected sessions resume
  carrying MIDI within 15 seconds without user action.
- **SC-005**: Across 100 induced disconnections of every supported kind, zero notes are left
  sounding once the connection recovers.
- **SC-007**: A continuous 24-hour session carrying MIDI traffic sustains zero unrecovered
  disconnections.

## Assumptions

- The default policy on unreachable peers is to keep retrying with backoff, on the assumption that
  a user who configured a peer wants it reconnected whenever it returns.

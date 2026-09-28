# Feature Specification: Network Ports

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: RTP-MIDI network ports between computers: discovery and advertising over mDNS, connecting
and inviting, invitation policy and trusted machines, clock synchronisation, the recovery journal,
several machines in one port, the automatic port other applications see, and interoperating with
Apple Network MIDI, rtpMIDI and rtpmidid.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### User Story 3 - Self-healing network MIDI between computers (Priority: P2)

A user has a studio computer and a stage laptop on the same network. They want MIDI to flow
between them continuously through a whole rehearsal. Midi Harbor discovers the other machine
automatically, they connect with one action, and the link stays up — surviving Wi-Fi roaming, the
laptop sleeping and waking, and the other machine being restarted — without the user touching
anything. This is the headline capability: the built-in tools drop these links and do not
recover.

**Why this priority**: Network MIDI resilience is the core differentiator and the primary reason
this product exists, but it requires endpoints (P1) to route to and from.

**Independent Test**: Establish a session between two machines, then induce each failure mode in
turn — pull the network cable, sleep and wake the machine, restart the peer, move between Wi-Fi
access points — and verify that in every case the session returns to a working state
automatically and MIDI resumes flowing, with no stuck notes.

**Acceptance Scenarios**:

1. **Given** another RTP-MIDI capable machine is on the local network, **When** the user opens the
   network section, **Then** that machine is listed as an available peer with its advertised name.
2. **Given** a discovered peer, **When** the user connects to it, **Then** a session is established
   and MIDI flows in both directions within a few seconds.
3. **Given** an established session, **When** the network connection is interrupted and later
   restored, **Then** the session is re-established automatically and MIDI resumes without user
   action.
4. **Given** an established session, **When** the local computer sleeps and wakes, **Then** the
   session is re-established automatically after wake.
5. **Given** an established session, **When** the peer computer restarts, **Then** Midi Harbor
   continues retrying and reconnects automatically once the peer returns.
6. **Given** an established session on Wi-Fi, **When** the computer roams to a different access
   point and its address changes, **Then** the session is re-established at the new address.
7. **Given** a session experiencing packet loss, **When** messages are lost in transit, **Then**
   the receiving side recovers the lost MIDI state so that no note is left sounding and controller
   values converge to the sender's values.
8. **Given** a session that has dropped mid-performance with notes held down, **When** the session
   recovers, **Then** no note is left sounding from before the interruption.
9. **Given** a peer that requires a connection to be accepted, **When** a remote machine invites
   this computer to a session, **Then** the user is notified and can accept or decline, with an
   option to always accept from that peer.
10. **Given** a peer that cannot be reached because of a genuinely permanent condition, **When**
    reconnection is impossible, **Then** the user is shown a specific reason rather than a generic
    failure.

---

### Edge Cases

- **Duplicate peer names**: When two machines on the network advertise the same session name,
  both are listed and distinguished so the user can tell them apart.
- **Self-discovery**: This computer's own advertised network session is not offered to the user as
  a peer to connect to.
- **Simultaneous invitations**: When two peers invite this computer at the same moment, both are
  handled without either being lost.
- **Address family changes**: When a peer is reachable over both IPv4 and IPv6, or moves between
  them, the session still establishes.
- **No network**: When there is no network at all, network sessions report waiting for a network
  rather than repeatedly reporting connection failures.
- **Clock differences**: When a peer's clock differs substantially from this computer's, timing
  synchronisation still converges and MIDI is delivered in the right order.

## Requirements *(mandatory)*

### Functional Requirements

#### Network MIDI sessions

- **FR-008**: System MUST discover other RTP-MIDI capable machines and devices on the local network
  automatically and present them with their advertised names.
- **FR-009**: System MUST advertise this computer's configured network sessions so other machines
  can discover it.
- **FR-010**: System MUST allow the user to connect to a discovered peer, and to add a peer
  manually by address and port when discovery is unavailable.
- **FR-011**: System MUST interoperate with the platform-native network MIDI implementations on
  macOS and Windows, with the common Linux implementation, and with hardware RTP-MIDI endpoints.
- **FR-012**: System MUST recover MIDI state lost to dropped packets so that notes are not left
  sounding and controller values converge to the sender's values.
- **FR-013**: System MUST maintain timing synchronisation with each peer and deliver messages in
  the order the sender produced them.
- **FR-014**: System MUST allow the user to control whether incoming session invitations are
  accepted automatically, accepted only from known peers, or always prompted.
- **FR-015**: System MUST persist network session configuration and restore it automatically at
  startup.
- **FR-015h**: System MUST, unless the user turns it off for a network session, present that
  session to other applications on the same computer as a MIDI port of the same name that carries
  MIDI both ways between them, renamed and removed with the session (R-078).
- **FR-015i**: System MUST show, for each network session, every machine taking part with its
  address and latency, let the user disconnect any one of them, and let the user connect further
  machines to a session that already has one. A machine the user connected beside the first is
  remembered and reconnected as the first is, after a restart and when its link is lost, until the
  user disconnects that machine (R-078).

### Key Entities

- **Network Session**: An endpoint representing a MIDI connection to another machine or device over
  the network. Has a peer identity, an address that may change over time, a discovery status, an
  invitation policy, and synchronisation state.
- **Peer**: A discovered or manually added remote party a network session can be established with.
  Has an advertised name, one or more addresses, and a known/unknown trust status.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-003**: After a network interruption ends, a network session resumes carrying MIDI within 10
  seconds, in at least 99 of 100 induced interruptions.
- **SC-006**: With 5% packet loss on a network session, no note is left sounding and controller
  values at the receiver match the sender's within 1 second of the loss ending.
- **SC-009**: MIDI passing over a network session on a local network arrives with additional delay
  of under 5 milliseconds at the 99th percentile beyond raw network round-trip time.
- **SC-011**: Another machine running the platform-native network MIDI implementation can discover
  and establish a working bidirectional session with Midi Harbor without any manual configuration
  on either side.

## Assumptions

- Network MIDI targets a local network. Operation across the public internet, including traversal
  of network address translation, is out of scope for this feature.
- Transport encryption and authentication are out of scope, matching the protocol as deployed by
  the platform-native implementations this must interoperate with. Trust is the local network.
- Session invitations default to prompting the user, with an option to always accept a given peer,
  since silently accepting inbound connections is a surprising default.

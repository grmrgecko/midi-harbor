# Feature Specification: Remembered Machines

**Created**: 2026-10-02

**Status**: Implemented; checked on 2026-10-02 between two daemons on one Mac, and from Linux
against Apple's Network MIDI, each side inviting, and rtpmidid. Not yet run on Windows, and following to another host
is covered by test only

**Input**: User report, with a screenshot of a network port offering `192.0.2.13:51474` as "On this
network" and failing to connect with "no peer named '9c22fef1-…'": "The app is showing an old
connection that was removed, and no longer listening." Then a request to look into a connected
machine whose session comes back somewhere else, and on trusted machines: "If we can prove that
they are the same host with an extension to rtp midi … then sure. Follow host. Maybe use a small
ECDSA/ED2559 key and verify with a signature check?", "Maybe you can validate based on name or
midi port id too?", and that a port deleted and made again under a new name "is a different midi
port" and is not to be followed.

A network port remembers each machine it connects to, so it can connect again after a restart. It
remembered the address and nothing else, and kept the record after the connection was removed. The
old record was then listed as present at a port nothing listened on, and a network port still
connected to a machine that came back on another port or address would have invited the old one
for good (research R-105, R-106).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A removed connection is gone (Priority: P1)

A user connects a network port to a machine found on the network, and later disconnects it. The
machine is no longer listed as one this computer knows. A session its host advertises afterwards,
on whatever port, is listed under its own name at its own address, and Connect reaches it.

**Why this priority**: it is the fault that was reported.

**Independent Test**: Connect a network port to two machines by address, disconnect one, and read
the configuration: only the other is remembered. Add a machine by name, connect to it by the
identifier the listing gives, disconnect, and list the machines: it is still there.

**Acceptance Scenarios**:

1. **Given** a machine remembered only because a network port connected to it, **When** the port
   disconnects from it, connects to another in its place, or is deleted, **Then** the machine is
   no longer remembered.
2. **Given** a machine the user named or trusts, **When** the network port disconnects from it,
   **Then** it stays remembered.
3. **Given** a remembered machine, **When** a client connects a network port to it by the
   identifier or name the listing gives, **Then** the port connects to its stored address.
4. **Given** a machine remembered without trust at one port, **When** its host advertises a
   session on another port, **Then** that session is listed as its own entry at its own address,
   and the remembered one is not marked as on the network.

### User Story 2 - A session that moved is followed (Priority: P1)

A network port is connected to a session on another machine. The session comes back on another
port: its program was restarted and the system chose again, or someone changed it. The network
port connects to it there without anyone touching it, and after a restart of the daemon goes
straight there.

**Why this priority**: connections that mend themselves are what Midi Harbor is for.

**Independent Test**: Start a network port whose remembered machine is at a port nothing listens
on, hand the daemon an advertisement of that machine's session at a port where one does, and
check the port connects and the configuration holds the new address.

**Acceptance Scenarios**:

1. **Given** a network port connected to a machine by a session it advertises, **When** the link
   is down and the session is advertised on another port of the same host, **Then** the port
   connects there and the new address is stored.
2. **Given** the same, **When** the session is advertised on another host and the machine is
   neither trusted nor a Midi Harbor, **Then** the port connects there.
3. **Given** the same, **When** the machine is trusted, is not a Midi Harbor, and the session is
   advertised on another host, **Then** nothing moves.
4. **Given** a machine carrying MIDI, **When** a session of its name is advertised somewhere
   else, **Then** nothing moves.

### User Story 3 - A Midi Harbor port is known for certain (Priority: P2)

A network port is connected to a network port of another Midi Harbor, which the user trusts. That
machine is given another address, or its port is renamed. The network port follows it, and the
trust goes with it, because the session at the new address proved it is the same port of the same
daemon. A different port, or another machine using the name, is not followed.

**Why this priority**: it removes the one case story 2 has to refuse, without weakening what
trust means.

**Independent Test**: Start a network port whose trusted machine is remembered with a key at an
address nothing listens on. Advertise that key from a daemon that does not hold it and check
nothing moves; advertise it from the daemon that does and check the port connects there.

**Acceptance Scenarios**:

1. **Given** a network port connected to a Midi Harbor network port, **When** that session is
   advertised where it is connected to, **Then** the port is asked to prove its daemon's key and
   its identifier there, and they are stored once it does.
2. **Given** a machine stored with a key and identifier, **When** its link is down and a session
   advertising them appears on another host and proves them, **Then** the port connects there,
   trusted or not, and a trusted machine is trusted at its new host.
3. **Given** the same, **When** the session advertising them cannot prove them, **Then** nothing
   moves.
4. **Given** the same, **When** the port is renamed, **Then** it is followed under its new name.
5. **Given** the same, **When** the port is deleted and another is made, under the same name or
   not, **Then** the new one is not followed.

### Edge Cases

- **The advertisement arrives before the link is noticed lost**: nothing moves then, and the
  machine is followed when the link is reported lost.
- **A session renamed**: its new name is advertised before the old one is withdrawn, so the
  machine is followed once the old advertisement goes.
- **A machine connected to by typed address**: it is followed too, once a session is advertised
  at that address.
- **A machine that never advertises**: it has nothing to follow, and is invited at its address
  as before.
- **Records written before this**: they take a name, and a key where there is one, the next time
  a session is advertised at their address. One left by a removed connection stays until it is
  forgotten or connected to and disconnected.
- **The machine asleep**: a network port waiting out sleep reconnects to the new address on
  waking.
- **A Midi Harbor that lost its key**: it is asked again where it is connected to, and its new
  key replaces the old. Moved before then, it is not followed.
- **Other implementations**: Apple's Network MIDI, rtpMIDI and rtpmidid advertise no key and are
  never sent the identity exchange.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-P01**: System MUST forget a machine that was remembered only because a network port
  connected to it once no network port connects to it, and MUST keep one the user named or trusts.
- **FR-P02**: System MUST connect a network port to a remembered machine named by the identifier
  or name under which it is listed.
- **FR-P03**: System MUST mark a machine remembered without trust as on the network only when a
  session is advertised at its stored address, and MUST list every other session its host
  advertises as an entry of its own.
- **FR-P04**: System MUST keep, with a machine a network port connects to, the session name
  advertised at its address, and while the link to it is down MUST connect to it where a session
  of that name is advertised and store that address: on another port of its host for any machine,
  on another host only for one that is not trusted.
- **FR-P05**: System MUST give each daemon a key of its own, kept outside the configuration
  document, advertise its public half and each network port's identifier with the port's session,
  and prove both to a network port that challenges it.
- **FR-P06**: System MUST keep the key and identifier of a Midi Harbor network port it connects
  to once proved at the address connected to, and from then on MUST treat a session as that
  machine only when it advertises both, whatever its name, and MUST follow it to another host
  only once it proves both there. Trust MUST move with a machine followed this way.
- **FR-P07**: System MUST NOT send the identity exchange to a session that does not advertise a
  key.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-P01**: After a connected machine's session comes back advertised on another port, the
  network port is carrying MIDI with it again within SC-003's ten seconds, with nothing done by
  the user.
- **SC-P02**: A machine that does not hold a trusted machine's key is never trusted by
  advertising that machine's name, key or identifier.

## Assumptions

- A session name on the local network identifies a session well enough to connect out to it, as
  it does in Apple's Network MIDI. It is not proof of who answers, so it never moves trust.
- The platform's browser reports a session again when its port changes, and reports its
  withdrawal.
- Trust in an address is as strong as the network makes addresses. The proof does not strengthen
  that; it stops trust moving to an address on the say of an advertisement.

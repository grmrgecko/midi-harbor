# Feature Specification: Test Note

**Created**: 2026-09-27

**Status**: Implemented; checked in the window on 2026-09-27 against a scratch daemon: a note
chosen, sent to a virtual port, and its note-on and note-off shown leaving it

**Input**: User request: "New feature request, under monitor allow sending a midi note out the
selected port for testing."

A program that plays a cue on a MIDI note, such as ProPresenter, is tested by sending it that note.
Until now that needed another MIDI application. The Monitor page and the command line now send one
note out of an endpoint, with its note-off after it, and the monitor shows it leaving.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Sending a note to test what listens (Priority: P1)

An operator sets up a cue in ProPresenter on note 62, channel 2. On the Monitor page they choose
the port ProPresenter listens to, pick "62 · D3" from the note list, type the channel, and press
Send note. The cue fires,
and the monitor shows the note-on and note-off leaving the port. On a machine without a window,
`midi-harbor send-note "Cue Bus" --note 62 --channel 2` does the same.

**Why this priority**: it is the whole request.

**Independent Test**: Send a test note to a virtual port through the contract, close the client at
once, and check the note-on then the note-off leave the port and a monitor sees the note.

**Acceptance Scenarios**:

1. **Given** an endpoint a route could send to, **When** a test note is sent, **Then** the note-on
   leaves it at once and its note-off after the note's length, half a second unless given.
2. **Given** the client goes away before the note-off, **When** the length passes, **Then** the
   daemon sends the note-off anyway.
3. **Given** a channel, note, velocity or length outside MIDI's ranges, **When** it is sent,
   **Then** it is refused as an invalid argument and nothing is sent.
4. **Given** an endpoint that only sends MIDI, or one switched off or not connected, **When** a
   test note is sent, **Then** it is refused saying why, and the window greys out Send note for an
   endpoint that only sends.

### Edge Cases

- **Velocity 0**: refused, since MIDI reads a note-on at velocity 0 as a note-off.
- **A long length**: at most ten seconds, so a mistyped length does not hold a note for hours.
- **An IAC bus**: what is sent comes back in on it, as with anything sent to that bus.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-N01**: Users MUST be able to send one note, with its channel, number and velocity, out of
  any endpoint a route could send to, from the window's Monitor page and the command line, with the
  note-off sent by the daemon after the note's length.
- **FR-N02**: System MUST deliver a test note as a route delivers, so monitors, counters and the
  silencing of held notes treat it like any other.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-N01**: A cue on a MIDI note can be tested from Midi Harbor alone, in under 30 seconds,
  without another MIDI application.

## Assumptions

- The window's note list shows each note's number and key, and the command line takes the number.
  Keys follow Apple's octave numbering, middle C (60) being C3, and the list marks middle C, since
  other tools call it C4; `monitor` names notes the same way.
- The owner asked on 2026-09-27 for the note to be picked from a list with its number and key,
  rather than typed as a number.
- The note list opens at note 0 rather than at the note chosen: libcosmic's dropdown, at the
  revision the project pins, has no way to open scrolled to its selection. Scrolling reaches any
  note; changing this needs the widget changed upstream.
- The first MIDI Out connector is used; testing each connector of a virtual port that has several
  is not asked for.

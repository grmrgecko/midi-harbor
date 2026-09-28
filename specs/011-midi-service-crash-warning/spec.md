# Feature Specification: MIDI Service Crash Warning

**Created**: 2026-09-27

**Status**: Implemented

**Input**: User request, after a MIDI cue sent over a network port was missed: find out why, and
change Midi Harbor to help. Then: "I don't think we should name the apps that lost connection",
and "Having a warning that the Midi service died is fine, stating it recovered but there may be
apps that lost its connection."

A USB MIDI adapter dropping off its hub crashed macOS's MIDI service on the Mac that received the
cues. Midi Harbor replaced itself and was carrying MIDI again within seconds
([008-resilience](../008-resilience/spec.md), T192), but the programs playing the cues stayed
attached to the service that had died and missed the next cue. The only trace
was an event in the history nobody read, and nothing on disk said whether the cue had arrived
(research R-101).

**Decided 2026-09-27**: the warning names no applications. Listing them was proposed, from their
start times against the new service's and whether they link CoreMIDI, and the owner declined it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Being told the MIDI service died (Priority: P1)

An operator runs a show with Midi Harbor carrying cues into a presentation program. The MIDI service
crashes and comes back while nobody is looking. Midi Harbor recovers on its own, but other
applications may not have, and only the operator can relaunch them. They are told on the desktop,
in the window, and by `midi-harbor status`, that the MIDI service stopped and was restarted, that
Midi Harbor recovered, and that other applications may have lost their MIDI connection.

**Why this priority**: a cue that silently goes nowhere in the middle of a service is the failure
this incident was.

**Independent Test**: Start a daemon as the replacement of one that lost the MIDI service; check
that `status` warns above its table, `status --json` carries the time, and the history holds a
`midi_server_replaced` event; start one plainly and check none of them appear.

**Acceptance Scenarios**:

1. **Given** the daemon replaced itself after losing the MIDI service, **When** the user runs
   `midi-harbor status`, **Then** a warning above the table gives the time the daemon found the
   service gone and says other applications may have lost their MIDI connection, until someone
   dismisses it.
2. **Given** the same, **When** the window is open, **Then** a banner above every page says the
   same until someone dismisses it.
3. **Given** the same, **When** the replacement starts, **Then** one desktop notification says so,
   except from the sandboxed App Store helper, whose app shows the banner.
4. **Given** the warning is showing, **When** the user dismisses it in any window or with
   `midi-harbor dismiss-warning`, **Then** the daemon clears it, and `status` and every window
   stop showing it.

### User Story 2 - Knowing afterwards whether a message arrived (Priority: P2)

After a missed cue, someone asks whether the cue reached this computer at the minute it was sent. The status and
the diagnostic report say when each endpoint last received and last sent a message, a network
port's automatic port says what it passed to the applications here, and the log has a line for
each endpoint whose traffic moved, kept after the in-memory history is gone.

**Why this priority**: the incident could only be diagnosed by inference; this answers it directly
next time.

**Independent Test**: Send a note into a network port's automatic port over loopback and read,
through the contract, when each side last received and sent.

**Acceptance Scenarios**:

1. **Given** a note arrived over the network, **When** the user runs `status`, **Then** the network
   port shows when it last received and its automatic port shows when it last passed a message to
   applications.
2. **Given** an endpoint's traffic moved, **When** ten seconds pass, **Then** the log has a line
   with its totals and last times, at most once a minute while traffic keeps moving.

### Edge Cases

- **Several crashes in one day**: each replacement carries its own time; a later one shows the
  warning again after an earlier one was dismissed.
- **The service slow to come back**: the replacement waits up to 30 s for it, and the warning
  still gives the time the loss was found, which the process that found it hands over.
- **The daemon restarted after a dismissal**: a daemon started by hand did not replace anything,
  so the dismissed warning does not come back.
- **A daemon started by hand after a crash**: it did not replace anything, so it does not warn;
  the warning belongs to the process that recovered.
- **The sandboxed App Store build**: its helper posts no notification, since the sandbox may refuse
  one and its app is running to show the banner.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-M01**: System MUST, when it has replaced itself after losing the platform's MIDI service,
  record a `midi_server_replaced` event and report the time over the contract, warn in `status`
  and the window, and post one desktop notification, saying the service stopped and was
  restarted, that Midi Harbor recovered, and that other applications may have lost their MIDI
  connection, without naming any.
- **FR-M02**: System MUST report, for every endpoint, when it last received and last sent a
  message, and for a network port the traffic of its automatic port, in the contract, `status`,
  the window and the diagnostic report.
- **FR-M03**: System MUST log each endpoint's traffic when it moves, without logging from the MIDI
  data path, so the log answers whether a message arrived after the history is gone.
- **FR-M04**: The warning MUST give the time the daemon found the service gone, and MUST stand
  until someone dismisses it, in a window or from the command line; dismissing it MUST clear it in
  the daemon, for every client.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-M01**: Within five seconds of the MIDI service being restarted, `status`, the window and
  the desktop say that other applications may have lost their MIDI connection.
- **SC-M02**: Whether a message reached a network port, and whether it was passed on to the
  applications on this computer, can be read to the second from `status` or the log, without
  administrator rights.

## Assumptions

- Only CoreMIDI has a service that can die under the daemon; the ALSA sequencer is in the kernel.
- CoreMIDI offers no way to see whether any application is listening to a source, so what reached
  an application after the automatic port stays unknowable.
- A notification through `osascript` appears as coming from Script Editor, and macOS may ask once
  whether to allow it.

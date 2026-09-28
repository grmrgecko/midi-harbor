# Research: MIDI Service Crash Warning

The investigation behind this spec, under its number in the project-wide research log.

---

## R-101: Other applications after the MIDI service dies

**Status**: **CLOSED** (2026-09-27). Built as T236 to T238.

A cue sent from a presentation program on one Mac, over a network port, did nothing on a second
Mac. The second machine's unified log, collected with `sudo log collect` because the user running
it was not an administrator, gave the order, timed from the device dropping:

| Time | What happened |
|---|---|
| 0.000 s | A generic USB MIDI adapter behind a USB 2.0 hub stalled and dropped off the bus |
| 0.394 s | It enumerated again |
| 0.746 s | `MIDIServer` crashed with a segmentation fault in `GetObjectTree`, inside Apple's code |
| 0.749 s | A video switcher's control software logged `ObjectTreeCache: refresh status -308` |
| 0.765 s | launchd started a new `MIDIServer` |
| about 4.5 s | The daemon noticed, replaced itself (T192) and had its network port connected again 0.3 s later |

The presentation program and the switcher's software had been running for hours and stayed
attached to the dead service until relaunched, as every CoreMIDI client does (R-079). Since the restart
the network port had received one message over the network: the cue most likely arrived and came
out of the automatic port with nothing listening. The daemon's only record was a warning in its
in-memory history, which nobody read, and its log said nothing about traffic.

The daemon cannot reconnect another application, and should not quit one while it is in use. Listing the applications left attached was considered, by comparing their start times
with the new server's and checking which link CoreMIDI, and dropped at the owner's request: the
warning names none. What was built:

- A `midi_server_replaced` event and `StatusSummary.midi_server_replaced_at`, set by the process
  that replaced the one that lost the service. `status` warns above its table for as long as that
  process runs, the window shows a banner until it is dismissed, and the replacement posts one
  desktop notification through `osascript`, which a bare executable run by launchd can use. The
  sandboxed App Store helper posts none; its app shows the banner.
- `last_received` and `last_sent` in `TrafficCounters`, and a network port's
  `automatic_port_counters`, shown in `status` and the diagnostic report. A network port's own
  counts say what the network carried; its automatic port's say what it passed to the
  applications here. CoreMIDI offers no way to see whether any application was listening to a
  source, so that part stays unanswerable.
- A line in the log for each endpoint whose traffic moved, from a task reading the counters every
  10 s, at most once a minute while traffic keeps moving. The data path is unchanged.

A grace period before forgetting absent applications' ports after a replacement was considered
and left out. The first device scan runs inside the daemon's start, before the binary knows it is
a replacement, and all it would keep is an unrouted port's entry, which returns when the
application does.

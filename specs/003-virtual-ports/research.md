# Research: Virtual Ports

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-002: Should Midi Harbor drive Apple's IAC driver and MIDINetworkSession?

**Decision**: No. Midi Harbor creates and owns its own virtual endpoints and its own RTP-MIDI
sessions on both platforms. Apple's configuration may be read once for migration, never written.

**Status**: **VERIFIED** by research; confirmed as a product decision with the owner.

**Rationale**: Neither Apple facility is programmable in a way that can meet the spec's resilience
requirements.

- The IAC driver's configuration lives in `~/Library/Audio/MIDI Configurations/*.mcfg`. Writing it
  works only while Audio MIDI Setup is closed; Audio MIDI Setup locks and reverts external changes
  when open. There is no supported API to add or remove IAC buses.
- `MIDINetworkSession` exposes only a single `default()` singleton. Attempts to construct
  additional sessions programmatically do not produce new sessions in the Network panel, via either
  the Objective-C or the C interface.
- Neither has any Linux analogue, so building on them would produce two different products.

Creating virtual endpoints via CoreMIDI directly gives endpoints that are indistinguishable from
IAC ports to other applications, while remaining fully under our control — which is what the
resilience requirements (FR-022 through FR-029) demand.

**Alternatives considered**: plist manipulation plus restarting `coremidiserver` — rejected as
fragile, macOS-only, user-hostile, and incapable of self-healing.

---

## R-014: Persisting the platform identifier

**Decision**: Store the identifier the platform assigns a virtual port on first creation, and pin
it on every later creation.

**Status**: **VERIFIED** on macOS (2026-09-20), after the first implementation got it wrong.

**What went wrong**: the backend supported pinning and the configuration had a field for it, but
the daemon never wrote the assigned value back. Ports were recreated correctly on restart and
`port list` looked right, so nothing appeared broken — but the CoreMIDI unique identifier changed
every time (observed `2738655218` becoming `275188976` across one restart). Other applications
address a port by identifier, so every daemon restart would have presented them with a *new* port
and silently dropped their connection. That is precisely the failure FR-004 exists to prevent, and
it was invisible from inside the product.

**The lesson for testing**: verifying that a port comes back is not the same as verifying it comes
back as the *same* port. The check has to be made from outside, by a separate process reading the
system endpoint list, because from inside everything looks correct either way.

**Sequence now**: create the port, read back the assigned identifier, store it in configuration if
it differs from what is held, and pass it as `pinned_unique_id` on every later creation. A pin the
platform refuses — because another endpoint already holds that identifier — is logged and the
assigned value kept, since a port with the wrong identity is better than no port at all.

---

## R-048: A message took 54 ms on macOS and 148 ms on Linux to cross a virtual port

**Status**: **FIXED** (2026-09-21). SC-008 and SC-010 now hold on both platforms, measured on real
backends.

T146's harness (`tests/latency.rs`) runs a daemon on the real backend with two ports and a route
between them. A second backend instance plays the other application: it sends into one port,
listens on the other, and times 2,000 messages sent at uneven intervals. SC-008 budgets a 1 ms
mean and a 3 ms 99th percentile. The first runs measured:

| | Mean | p99 |
|---|---|---|
| macOS, as built | 53.8 ms | 57.3 ms |
| Linux, as built | 148 ms | 197 ms |
| macOS, fixed | 120 µs | 170 µs |
| Linux, fixed | 164 µs | 919 µs |

Three delays, each far larger than the budget:

- **The CoreMIDI thread served requests only between 50 ms turns of its run loop**, and a send
  waits for its answer. Every request now stops the run loop, which is safe from any thread, so
  it is served at once.
- **The ALSA thread slept 50 ms between looks**, so input waited as well as output. It now
  `poll`s the sequencer's descriptors together with a wake-up pipe that each request writes to.
- **The daemon drained each endpoint on a 2 ms timer**, a millisecond of waiting on average,
  which is the whole budget. A push now rings a doorbell: one atomic swap and a lock-free unpark,
  so a real-time callback can ring it. A listening thread wakes the drain tasks.

The timer also cost idle CPU. With 13 endpoints on macOS the daemon used 3.6% of a core while
nothing played, against SC-010's 1%.

| Idle, 10 ports plus hardware | CPU | Memory |
|---|---|---|
| macOS, 2 ms timer | 3.6% of a core | not measured |
| macOS, doorbell | 0.67% | 13.0 MB |
| Linux, doorbell | 0.23% | 18.6 MB |

SC-010 allows 1% and 150 MB. Those figures came from process CPU time over 30 s, measured by
hand.

**Found on the way**: the harness's first send to a port failed, and so would the daemon's
sends to any hardware on macOS. A device's destination was looked up by its source's unique
identifier, which it never shares (see the commit "send to hardware through its own
destination").

**SC-009, partly**: SC-009 asks for under 5 ms at p99 *beyond* the raw network round trip.
Two measurements:

| Path | p50 | p99 | Max |
|---|---|---|---|
| Two daemons in one process, loopback session, in-memory ports | 350 µs | 2.8–3.4 ms | 9–15 ms |
| The same route without a session, for comparison | 110 µs | 1.8–2.3 ms | 3–5 ms |
| Round trip, this Mac (CoreMIDI, Wi-Fi) → Linux (ALSA echoing its own port) → back | 3.5 ms | 20.7 ms | 49 ms |
| `ping` over the same Wi-Fi, same rate | 3.7 ms | 7.2 ms | 14 ms |

At the median the whole round trip costs what `ping` does: two daemons, two sessions, CoreMIDI
and the ALSA echo add almost nothing. At the 99th percentile the round trip is well above
`ping`. On Wi-Fi, though, the radio's own tail cannot be separated from the daemons' by
subtracting percentiles.

The in-memory figures are upper bounds. Polling the in-memory platform from the harness disturbs
what it measures: copying the whole sent list while holding its lock pushed p99 up, and
busy-spinning pushed it to about 10 ms. The fake now offers `last_sent`, and the harness sleeps
20 µs between looks. The real-CoreMIDI harness, which polls a lock-free ring, measured 170 µs at
p99 through the same daemon. That the tail grows with CPU contention is itself worth knowing: a
MIDI machine is often busy running a DAW. Each message crosses three wake-ups (callback, doorbell
thread, tokio worker), and fewer wake-ups is where to look first if that tail has to shrink.

**Not covered**: SC-010b (a repeater under 10 ms) needs physical hardware at both ends. The worst single message was 15 ms on
macOS and 4.9 ms on Linux. That is inside the p99 budget's spirit but worth watching under load.

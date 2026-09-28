# Research: Routing

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-029: A route's traffic cannot be derived from its endpoints

**Status**: **VERIFIED** (2026-09-20) on macOS over CoreMIDI and Linux over ALSA.

`Route.counters` was declared in the contract and never populated, so every route row read the
same whether it was carrying music or nothing at all. The obvious repair — subtracting endpoint
counters — does not work: one message arriving at a source that fans out to two destinations is
one `messages_received` on the source and one `messages_sent` on each destination, and no
arithmetic over those three numbers says which of the two routes is the one that stopped working.

Attribution therefore has to happen at the only point that knows which route delivered a message,
which is dispatch. `Router::deliveries` returns the route alongside the destination for this
reason, replacing `destinations`.

Two consequences fell out of route identity being derived from the pair of endpoint names:

- A route deleted and created again is the same route by identity, and is not the same route to
  the person who just remade it. Its counters are dropped with it.
- A rename changes the identity of every route naming that endpoint. Counters are re-keyed in the
  same write, so a rename does not read as a reset — for the same reason a rename rewrites routes
  rather than dropping them.

**Measured**: three notes sent into a hardware input routed to two virtual ports counted 3 on each
route independently, while the endpoint's own counters showed 3 in and 6 out across the pair.

---

## R-030: A lone 0xF0 is worse than no system-exclusive at all

**Status**: **VERIFIED** (2026-09-20) on macOS over CoreMIDI and Linux over ALSA.

Both platform backends carried the same hand-rolled byte scanner, and both got dumps wrong in the
same two ways. `MidiMessage::parse` reports `0xF0` as a one-byte system message — correctly, since
its documentation says the caller scans for the terminator itself — and neither caller did. So a
dump produced a lone `0xF0` forwarded to every destination, and then the scan stopped at the first
payload byte, because a data byte with no running status is unparseable. Everything after a dump
in the same read was lost with it.

A lone `0xF0` is not a harmless artefact. It opens a system-exclusive message on the receiver,
which then swallows what follows as payload until it sees a terminator that never comes.

The scanner now lives in `core::stream`, shared by both backends and testable with no MIDI
hardware, and handles what the duplicated versions did not:

- a dump spanning reads, which is the ordinary case rather than the exceptional one;
- real-time bytes interleaved mid-dump, which are legal and which a sequencer running a clock
  sends constantly;
- a sender that abandons a dump, distinguished from one that completes it, because a truncated
  dump still frames correctly and a receiver will act on it.

The daemon rebuilds whole messages before forwarding, bounded at 256 KB so a source that never
sends a terminator cannot grow the buffer without limit. Nothing partial is ever forwarded, which
is what the edge case in T116 asks for.

**Not yet carried**: a route to a network session counts a dump as undelivered rather than sending
it. RTP-MIDI segments system-exclusive through the recovery journal, which is US3 work; counting
it makes the gap visible on the route instead of silent. Carried since R-066.

**Measured**: a 604-byte dump sent through a route arrived byte-identical on both platforms
(checksum 36910), with a note sent between two dumps arriving in its original position.

---

## R-032: A route that reads as fine while dropping everything

**Status**: **VERIFIED** (2026-09-20) on macOS and Linux.

`RouteValidity` had two states for a route that was not delivering — broken and part of a loop —
and neither covered the common one. Switching off an endpoint, or unplugging hardware, left every
route touching it reporting `ok` while carrying nothing. The endpoint list said "disabled" and the
route list said "ok" about the same thing in the same breath.

That is the worst of the three states to debug, because the display agrees with the user that it
should be working.

`Suspended { waiting_on }` is deliberately not a kind of broken. Broken means an endpoint is gone
and the user has something to restore; suspended means everything is present and the route resumes
on its own. Conflating them would send someone looking for a problem that is not there, so
`needs_repair` distinguishes the two and `route list --broken` keeps meaning broken.

The router cannot see runtime state, so the daemon passes it the set of endpoints that could carry
MIDI this instant — a port with an open handle, or a session with a supervisor. That is exactly
the condition dispatch uses to decide whether to attempt delivery, so the route's answer and
dispatch's behaviour cannot disagree.

**Two defects fell out of writing the test for unplugged hardware:**

- Nothing ever closed a device that went away. The runtime entry and its handle survived the
  unplug, so the daemon kept sending into a handle for hardware that was not there.
- Because that entry survived, `open_present_devices` saw the device as already open and skipped
  it, so **a replugged device never reopened**. The same shape as the earlier disable-then-enable
  defect, reached by a different route.

**The fake was hiding it.** `FakeMidiPlatform` never overrode `open_device_with_sink`, so the
default implementation dropped the sink and no test could make opened hardware send anything.
Every routing test therefore used virtual ports — the same blind spot recorded in R-028. The fake
now keeps the sink, and the replug test asserts MIDI actually arrives rather than that the state
says it should.

---

## R-062: Loops across machines, caught where they close

**Status**: **DONE** (2026-09-21), for T114 (FR-033, RISK-9). The owner left the design to us.

A loop within one machine shows in its route graph (R-019). One across machines does not. For
example, machine A sends a keyboard to B over a session, B routes that session back over another,
and A routes that one to the first. Every route is sensible on its own machine, and a note goes
round for as long as they run. The plan named a per-message origin marker, carried in a private
extension of the protocol. That was not built:

- RTP-MIDI carries plain MIDI, so a marker would work only between Midi Harbor machines.
- A packet extension might be refused by Apple's implementation or rtpmidid.
- It would miss a loop passing through any other implementation.

Routing is direct. MIDI arriving on a session can leave on another only through a route from one
to the other, so every loop across machines closes on such a route on some machine. That is where
it is watched:

- Each session remembers what it sent in the last 250 ms, and where each message came from, in a
  fixed ring of 128.
- A route from one session to another counts an echo when a message it is about to forward is one
  that session sent moments ago from a different source.
- A relay forwarding the same note over and over sees its own earlier forwards, which share its
  source, so relaying is never an echo.
- Sixteen echoes within 250 ms is a loop. A real loop echoes on every round, hundreds of times a
  second. Two players striking the same note on two machines at once cannot reach that.

On a loop, the route is switched off as `route disable` would, releasing what it held. It stays
off across a restart, and the history names the route, the reason and the command to switch it
back on. That was chosen over dropping the echoed messages quietly, which would leave a route
carrying less than it should with nothing to say why.

`crates/core/src/loops.rs` holds the logic as pure functions over injected time. It has tests for:

- an echo;
- a relay's repeated notes, which are not echoes;
- an expired send;
- a loop tripping exactly once, where a player's coincidences never do.

`crates/daemon/tests/loops.rs` builds the loop from two daemons over loopback. One note gets
the closing route switched off and explained, while the keyboard's own route stays on. A second
test has a relay across three daemons carrying the same note 50 times in 250 ms without
tripping. Each test fails when its rule is removed.

---

## R-019: Routing is direct, not transitive

**Decision**: MIDI arriving at an endpoint is delivered to that endpoint's immediate destinations
and no further.

**Status**: **VERIFIED** (2026-09-20). A design decision made while building the router, recorded
because the alternative is superficially attractive.

**The alternative and why it was rejected**: with transitive delivery, `A -> B` and `B -> C` would
mean MIDI from A reaches C. That sounds convenient until you notice two consequences. Adding one
route silently changes where existing traffic goes, which is the opposite of what a patchbay
should do. And any cycle in the configuration becomes unbounded amplification rather than a
harmless oddity.

Direct delivery matches what FR-030 actually asks for — many sources to one destination, one
source to many — and matches how hardware patchbays behave, which is what users expect.

**What this makes of cycles**: a cycle cannot multiply messages under direct delivery, so a route
that completes one is still created and still delivers. It is reported because drawing one is
almost always a mistake, which is what FR-033 asks for. Cycles are detected over endpoint *names*
rather than resolved endpoints, so a cycle drawn through an endpoint that does not exist yet is
still reported rather than hidden behind a broken route — and the warning appears before the user
recreates that endpoint, not after.

**The loop that does multiply** spans two machines: machine A repeats a device onto a session,
machine B repeats that session back. No local graph can see it. That is caught by the session
identity carried on the wire, per R-013.

**A broken route is never deleted.** Removing an endpoint marks every route naming it broken and
names what is missing, and the routes resume unchanged when that endpoint returns. Verified by
deleting a virtual port and recreating it.

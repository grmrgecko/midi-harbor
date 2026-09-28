# Research: Network Ports

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-004: RTP-MIDI implementation

**Decision**: Implement AppleMIDI/RTP-MIDI ourselves in a dedicated `no_std`-friendly protocol
crate, including the RFC 6295 recovery journal.

**Status**: **ASSUMED**; this is the largest single body of work in the project.

**Rationale**: The existing `rtpmidi` crate explicitly does not implement the recovery journal.
The journal is precisely the mechanism that prevents stuck notes and stale controller values when
UDP packets are dropped — FR-012, FR-026 and FR-027 are unachievable without it, and SC-005 and
SC-006 measure it directly. A reconnect-only implementation would not deliver the product's
defining value.

**Protocol scope to implement**:

- **Session control (AppleMIDI)**: a UDP port pair — control on port `N`, data on port `N+1`. The
  five commands are `IN` (invitation), `OK` (accept), `NO` (reject), `BY` (end session), and `CK`
  (clock synchronisation), plus `RS` (receiver feedback) on the data channel.
- **Clock synchronisation**: the three-message `CK` exchange, repeated periodically, establishing
  the offset and latency used to order incoming messages. Also the liveness signal — a peer that
  stops answering `CK` is how a silent disconnection is detected.
- **MIDI payload**: RTP packets with the RTP-MIDI payload header, supporting both the short and
  long header forms, delta-time encoding between messages in a packet, and running status.
- **Recovery journal (RFC 6295)**: the session journal plus per-channel chapters. Minimum viable
  chapter set for correctness: Chapter P (program change), Chapter C (control change), Chapter W
  (pitch wheel), Chapter N (note on/off — the one that prevents stuck notes), and Chapter T
  (channel aftertouch). Chapter M (parameter system) and system chapters are lower priority.
- **Journal trimming** driven by the receiver's reported highest-received sequence number, so the
  journal does not grow without bound.

**Interoperability targets (FR-011)**: Apple Network MIDI (macOS), rtpMIDI (Windows), rtpmidid
(Linux), and hardware RTP-MIDI endpoints. Each must be tested explicitly; interoperability is not
assumed.

**Apple interoperability VERIFIED (2026-09-20), before the journal was written.** A full session
was driven against a Mac running Network MIDI using this project's own codec: the control channel
was invited and accepted, the data channel one port above was invited and accepted, a clock
exchange completed with a 2 ms round trip, and three RTP-MIDI note packets were sent and the
session closed cleanly. The wire format therefore comes from a live peer rather than from reading
documentation — Apple's acceptance packet is kept as a fixture in `control.rs`, and the live run
is `tests/applemidi_live.rs`, ignored by default and pointed at a peer with `HARBOR_PEER`.

One correction this produced: the documentation summary omits the leading `0xFF 0xFF` signature on
control packets, which is present on the wire and is what distinguishes control traffic from RTP
on the shared port pair.

**Alternatives considered**: `rtpmidi` crate as-is — rejected, no journal. Wrapping the crate now
and replacing later — considered and rejected by the owner, because the journal shapes the packet
and session data structures deeply enough that retrofitting it would mean a rewrite anyway.

---

## R-015: The journal only repairs on a later packet

**Decision**: A session must keep emitting journal-carrying packets while any state is
unacknowledged, rather than falling silent after its last MIDI message.

**Status**: **VERIFIED** (2026-09-20). Found by the loss-injection harness, not by reading.

**What the harness showed**: with 5% loss, one seed in twenty left a note sounding. The cause was
not a journal bug. The recovery journal rides on *subsequent* packets, so if the final packet of a
phrase carries a note off and that packet is lost, nothing follows to repair it. The receiver is
left holding a note with no way to learn otherwise.

This is inherent to the format and cannot be fixed inside the journal. It is a requirement on the
session layer:

- After its last MIDI message, a sender MUST continue emitting packets carrying the journal until
  the receiver acknowledges the sequence that covers them. Modelled in the harness as a fixed
  number of trailing journal-only packets; the session implementation should instead keep emitting
  while `JournalState::build()` returns anything, at a modest interval.
- A receiver MUST still silence everything when a session times out, because a sender that is
  switched off mid-phrase cannot send anything at all. The journal narrows that window; it does
  not remove it.

**Why it matters**: this is exactly the failure the product exists to prevent, it appears only
under loss at the end of a phrase, and it would have shipped looking correct — the journal
encodes, decodes and recovers perfectly in every other case.

---

## R-016: RTP streams start at a random sequence number

**Decision**: The journal checkpoint is established from the first packet actually sent, not
assumed to be zero.

**Status**: **VERIFIED** (2026-09-20), after the first implementation got it wrong.

**What went wrong**: `JournalState` initialised its checkpoint to zero. Sequence numbers are
16-bit and wrap, so comparisons are done in wrapped space. On a session that happened to start at
a high sequence number — which RTP senders choose at random — every acknowledgement looked like it
came from *before* the checkpoint, so trimming never ran and the journal grew for the life of the
session. Sessions starting at a low number worked perfectly, which is what made it easy to miss.

The checkpoint is now unset until the first packet is sent. A stale acknowledgement still cannot
move the history backwards.

**Bound confirmed**: 20,000 note pairs with the receiver acknowledging slightly behind keeps the
encoded journal under 512 octets.

---

## R-005: mDNS / DNS-SD service discovery

**Decision**: `mdns-sd` 0.21.3, a pure-Rust implementation with both responder and querier roles.
Service type `_apple-midi._udp`.

**Status**: **REVISED** (2026-09-20). Browsing stays on `mdns-sd`; advertising moved to the
platform responder, which is the fallback this decision originally named. See R-022.

**Original status**: VERIFIED on macOS (spike T007). RISK-3 retired.

**Spike results**: `mdns-sd` 0.21.3 started, registered an `_apple-midi._udp` service, and browsed
successfully **while `mDNSResponder` held port 5353** — no special socket configuration was
needed. It resolved a real Apple Network MIDI session already advertising on the LAN
(`Studio Mac._apple-midi._udp.local.` at `192.0.2.4:5004`), over both IPv4 and IPv6, which also
gives early evidence for the FR-011 interoperability path.

It also resolved **its own** advertisement, confirming that the self-exclusion edge case in the
spec is real and must be implemented (T094), not theoretical.

**Rationale**: Pure Rust avoids depending on Avahi being installed on Linux or on linking Apple's
`dns_sd` on macOS, which keeps the headless Linux build dependency-light (FR-039b) and keeps
behaviour identical across platforms (Principle IV). `mdns-sd` supports both registering
(FR-009) and browsing (FR-008), which the browse-only crates do not.

**Port 5353 coexistence — resolved.** The concern was that `mDNSResponder` (macOS) and
`avahi-daemon` (Linux) already hold UDP 5353. On macOS this is a non-issue: `mdns-sd` handles the
socket options itself and coexists cleanly. Linux coexistence with `avahi-daemon` was
verified on 2026-09-25, which retired RISK-3.

One consequence for addressing: the spike returned a large set of addresses per service across
every interface, including link-local IPv6 with scope ids and bridge interfaces. Peer address
selection must therefore be deliberate — prefer a routable address on the interface the session
was discovered on — rather than taking the first address returned.

**Alternatives considered**: `zeroconf` (wraps Bonjour/Avahi) — the fallback. `simple-mdns` —
smaller community, less proven. `mdns_sd_discovery` — browse-only, cannot advertise.

---

## R-017: A dual-stack socket refuses a plain IPv4 address

**Decision**: Rewrite IPv4 targets into their v4-mapped IPv6 form before sending from a
dual-stack socket.

**Status**: **VERIFIED** (2026-09-20), after two local sessions failed to reach each other.

**What happened**: the session transport binds `[::]` so that a peer reachable only over IPv6 is
still reachable, which on every supported platform also serves IPv4. Sending to a plain IPv4
`SocketAddr` from that socket fails outright:

```
plain ipv4 target: Err(Os { code: 22, kind: InvalidInput, message: "Invalid argument" })
v4-mapped target : Ok(2)
```

IPv4 peers are the common case on a local network, so this made **almost every peer unreachable**.
It did not show up in the live Apple interoperability run, because that test binds its own
IPv4-only socket — a reminder that a passing interop test does not exercise the daemon's transport.

**Fix**: the socket records whether it bound dual-stack, and maps `IpAddr::V4` targets through
`to_ipv6_mapped()` on the way out. A test sends from a dual-stack socket to an IPv4 listener and
asserts the datagram arrives, rather than asserting the send returned `Ok`.

---

## R-018: Both ends of a session must drive the same handshake

**Decision**: Session phases name the handshake stage rather than the role, and a responder
advances through them as invitations arrive.

**Status**: **VERIFIED** (2026-09-20).

**What was missing**: the first implementation only handled the initiator. A peer inviting us was
answered with an acceptance but never advanced a phase, so an inbound session stalled and this
machine was effectively unreachable — other machines could see the advertisement and never
connect. Naming the phases after the initiator's view (`InvitingControl`, `InvitingData`) is what
made the gap easy to miss.

Two further rules fell out of it:

- A responder does **not** open the clock exchange. Both ends opening one at once wastes packets
  and muddles the estimate.
- A responder does **not** time out waiting to be invited. The peer that invited us knows where we
  are and will retry; chasing it is not our job, and treating silence as failure would tear down a
  session that is merely waiting.

---

## R-020: Publishing an mDNS service that peers can actually see

**Decision**: Advertise under a hostname derived from the machine's but distinct from it, with
explicitly enumerated routable addresses.

**Status**: **VERIFIED** on two machines (2026-09-20). Three separate faults, each of which made
the service look advertised while being invisible to every peer.

This was only found by testing against a second machine. On one machine everything appeared
correct at every step.

**Fault 1 — automatic address detection published loopback only.** `enable_addr_auto()` resolved
to `127.0.0.1` on the second machine. The service appeared in that machine's own browser, pointing
at itself. Fixed by enumerating interface addresses with `if-addrs` and passing them explicitly,
filtering out loopback and link-local.

**Fault 2 — the machine's own hostname is already taken.** Publishing an A record for
`Studio-Mac.local.` conflicts with the system `mDNSResponder`, which owns it. The record is
accepted, appears in a browser, and is then **withdrawn a second or two later**:

```
13:06:37  Add  3  22  _apple-midi._udp.  Studio Rig
13:06:39  Rmv  0  22  _apple-midi._udp.  Studio Rig
```

A browse that happens to run in that window sees a healthy service. Fixed by publishing under
`<machine>-midiharbor.local.`, which nothing else owns.

**Fault 3 — self-exclusion compared the wrong name.** It checked the resolved instance against
the *machine* name, but a session advertises its *own* name. So every session this machine
published came back as a connectable peer — the machine offered to connect to itself. Fixed by
tracking the set of names actually advertised.

**What this says about testing**: all three are invisible from one machine. The service registers
without error, appears in the local browser, and resolves. Only a second machine, and Apple's own
`dns-sd` for a neutral view of the wire, distinguished "published" from "reachable".

**Still outstanding**: with all three fixed the record persists on the real interfaces and the
remote machine discovers us, but we do not discover the remote. Discovery is therefore one-way
between two Midi Harbor machines, while both discover Apple's Network MIDI. Connecting by address
works in both directions, so this is a propagation problem in the responder rather than anything
in the session layer. Carried as RISK-11.

---

## R-021: Two Midi Harbor machines carrying MIDI end to end

**Status**: **VERIFIED** on two machines (2026-09-20).

Notes played by an unrelated application into a virtual port on one Mac arrived on a second Mac
across the network, through the whole chain: CoreMIDI callback, real-time ring buffer, dispatch
task, route graph, RTP-MIDI session over UDP, the peer's session, and its own route graph. Zero
delivery failures and zero drops.

This required one late correction: MIDI arriving from a peer was being logged rather than
dispatched, so a session was a route *destination* but not a route *source*. A repeater built that
way carries traffic one way only, and looks entirely healthy from the sending end.

---

## R-022: The pure-Rust responder does not answer cross-machine queries

**Decision**: Advertise through the platform responder (`zeroconf`: Bonjour on macOS, Avahi on
Linux). Keep browsing on `mdns-sd`.

**Status**: **VERIFIED** on two machines (2026-09-20). Resolves RISK-11.

**The symptom**: two Midi Harbor machines could not discover each other, while both discovered
Apple's Network MIDI. Earlier notes called this "one-way discovery"; that was wrong, and the
earlier reading was an artefact of testing inside the hostname-conflict window described in R-020.
With those faults fixed it was symmetric: neither saw the other.

**The isolating test**, which is what made this tractable:

| Advertised by | Seen by our browser | Seen by Apple's browser |
|---|---|---|
| Apple `dns-sd -R` on the remote | **yes** | **yes** |
| `mdns-sd` on the remote | no | no |
| `zeroconf` (Bonjour) on this machine | **yes** | **yes** |

The same machine, the same service type, the same browsers. **Browsing was never the problem.**
`mdns-sd`'s responder announces once at registration and then does not answer continuing queries
from other machines, so a service registered through it is visible for a moment — if something
happens to be listening at that instant — and invisible afterwards to every peer, including to
Apple's own browser.

That intermittency is what made it confusing: a browse started at the right moment does see the
service, which looks like a flaky network rather than a responder that has stopped answering.

**Why the split rather than one stack**: `mdns-sd` browses reliably and reports usable addresses;
`zeroconf`'s browser returned `0.0.0.0` for most entries in testing. Each library is used where it
demonstrably works, which is worth more than the tidiness of a single dependency.

**Cost accepted**: Linux now needs Avahi for advertising, which R-005 identified as the price of
this fallback. Added to CI. Browsing still needs nothing.

---

## R-036: The invitation policy was a setting that did nothing

**Status**: **VERIFIED** (2026-09-20) between the macOS and Linux machines, all three outcomes.

`InvitationPolicy` was stored per session, persisted, displayed, settable from
`session create --policy`, mapped both ways across the contract, and defaulted to `Prompt` with a
comment saying *"silently accepting inbound connections is a surprising thing for a program to
do"*. It was never consulted. `adopt_invitation` accepted every invitation that arrived.

Anything that could reach the control port was in, and could inject MIDI into whatever the
session was routed to, while the user's configuration said they would be asked first.
`EventKind::InvitationReceived` was declared, given a wire name, and never emitted — the same
shape of gap as `NotesSilenced`.

**Answering a prompt is a decision about a machine, not about a packet.** An invitation has a few
seconds of patience — three attempts, two seconds apart — and a person does not. So `Ask` leaves
the invitation unanswered and records who is knocking, and an answer of yes marks the peer
accepted, which the invitation already in flight or the next retry then satisfies. `always`
persists that; without it the peer is accepted for this run only.

Unanswered rather than refused, on purpose: a refusal tells the initiator it was turned away, and
the user may be seconds from saying yes. `RejectAll` does refuse explicitly, because there the
answer will not change.

**Two defects found while testing it:**

- Reporting an invitation awaited a bounded channel. A peer invites repeatedly, so a queue nobody
  was draining would have blocked the supervisor — and with it every message that session carries.
  Offered rather than awaited now: the same question being asked again is not worth stalling MIDI
  for.
- The sockets are dual-stack, so an IPv4 peer arrives as `::ffff:192.0.2.94`, which never equals
  the plain address a user typed or a configuration stored. Trust would have been recorded and
  then silently never matched. Addresses are compared canonically.

**Measured** across two machines: the default held the connection at `connecting` and listed the
caller by name; answering with `--accept --always` connected both ends; the peer was let straight
in after a daemon restart with no prompt; and switching to `reject` turned that same remembered
machine away, ten refusals logged, with the caller told it was rejected rather than left hanging.

**The window has to be able to answer too.** Making the default enforce turned a GUI-only setup
into one where an inbound session hangs with nothing on screen explaining why, which is a worse
failure than the one being fixed. The prompt is a banner above whatever page is open rather than a
page of its own: a machine is waiting and will stop waiting, so a prompt the user has to think to
go and look for is a prompt nobody sees. Verified with the Linux machine inviting: the banner named
it, pressing Always connected both ends, and the banner went away.

---

## R-037: Trust you can grant but not see or revoke

**Status**: **VERIFIED** (2026-09-20) between the two machines, granting and revoking.

Answering a prompt with "always" wrote a trusted peer into the configuration. `ListPeers` returned
only what discovery could see this instant, with `trusted: false` hardcoded, so a machine the user
had trusted never appeared in any list. `RemovePeer` was a stub. Trust could be granted and then
neither seen nor taken back, except by editing the file by hand.

The contract's `Peer` already carries `discovered` and `trusted` as separate fields, which is the
shape this needed: one list of machines this one knows about, each marked with whether it is
advertising now and whether it is let in without asking. A machine the user trusted is worth
showing whether or not it is switched on this minute, and it cannot be forgotten if it cannot be
seen.

Revocation takes effect at once rather than at the next restart, because trust that stays in force
until a restart is not revoked.

**Matching is by host, not by socket.** A peer is remembered at the standard port and invites from
whatever port its session bound, so comparing sockets would never match. What a user trusts is a
machine; the port is neither chosen by them nor stable across restarts.

**A defect this found in the previous commit**: an invitation recorded as waiting was only ever
removed by being answered. Trusting the machine by address instead, or the session connecting some
other way, left the prompt on screen forever — a question already settled, asking the user to
decide it again. Invitations are now dropped when the peer becomes trusted or the session
connects.

**Measured**: `session peer add 192.0.2.20` on one machine let in the invitation the other was
already sending, with no prompt answered; `session discover` showed it as remembered and trusted
beside two machines that were merely advertising; `session peer remove` held the next invitation
again without a restart.

---

## R-046: Our own responder dropped our own initiator every thirteen seconds

**Status**: **FIXED** (2026-09-21), verified between this Mac and the Linux machine.

T106 asks that a keyboard routed to another machine survive being unplugged with no note left
sounding on the far side. Writing it turned up two defects, one per machine.

**Silencing never reached another machine.** Stopping a route's notes sent the note offs through
the destination's platform port. A network session has no platform port, and neither does a
Bluetooth link, so the release was skipped, after the record of what was held had already been
cleared. A keyboard unplugged mid-note, or switched off, left the note ringing on the other
machine or the Bluetooth synth for good. Routing and silencing now share one delivery function
that knows every transport, so silencing cannot reach fewer places than routing does.

**The responder judged liveness against the wrong clock.** Run between two machines, the
responder dropped the session 5 ms after accepting it, again after 1.2 s, and then every ~13 s
for as long as it ran. Each drop sent a full sixteen-channel silence to the far synth, while the
initiator believed the link steady. The cause was in the clock exchange. On the responder, the
exchange's opening and closing timestamps belong to the initiator's clock, and the exchange was
dated by them. Liveness then measured the responder's own clock against that date:

- Initiator's clock behind by more than the 35 s timeout (here: started a minute later): the
  session was dropped as soon as it was accepted, then after every clock exchange.
- Initiator's clock ahead: the date sat in the future, and a responder could never notice a peer
  that had gone.

Each machine counts ticks from when its own session started, so which case applies depends only
on which side started first. That is why the in-process test, whose two sessions start
milliseconds apart, could not show it. Exchanges are now dated by the local clock on both sides.
The responder's offset had the opposite sign to the initiator's, which was latent since nothing
converts peer timestamps yet, and was fixed alongside.

Across the two machines afterwards: one "established" and no drop in 45 s. The far synth
received nothing until a note was played. Switching the source off on the Mac then released that
note on the Linux synth, on its own channel only.

---

## R-049: A session switched off and on came back somewhere its peer was not looking

**Status**: **FIXED** (2026-09-21). Found by T148's soak on its first fault.

The soak (`crates/daemon/tests/endurance.rs`) joins two daemons in one process with a loopback
session. A keyboard on one is routed through the session to a synth on the other, and notes play
throughout. It then injects faults in turn:

- the far session is switched off for two seconds;
- the keyboard is unplugged and replugged;
- the synth port is switched off and on.

After each fault, a probe message has to reach the synth within SC-003's ten seconds.

The first fault was never recovered. The near session retried for good, and the far session
listened, but on a different port. Three defects compounded:

- **A session left to the system's choice of port chose again on every start.** Switching it
  off and on, or restarting the daemon, moved it. A peer that had connected by address went on
  retrying the old port. Discovery would have found the new one, but a peer added by address
  never looks. The first port the system gives is now written into the configuration and
  reused, the way a virtual port keeps its CoreMIDI identifier.
- **Ports the system chose were not held to the even-port rule.** Binding moves an odd control
  port up to an even one, because some implementations refuse an even data port. That applied
  only to ports we asked for, and the system hands out odd ones as readily as even. An odd port,
  once recorded, moved up by one on the next start, so pinning alone changed nothing. An odd
  choice is now refused and the even port above it tried.
- **Stopping a session did not wait for it to stop.** `shutdown` sent the supervisor a command
  and returned while it still held both sockets. A session switched straight back on found its
  own port taken, and binding fell back to a nearby pair, away from the peer again. `shutdown`
  now awaits the supervisor's task, so the ports are free when it returns.

`a_session_keeps_the_port_the_system_chose` covers the first and third defects, and
`a_port_the_system_chooses_is_even` covers the second. Each fails with its fix reverted. With
the pin reverted, the soak fails on its first fault again.

**The harness had a defect of its own.** It looked for the probe as the last message the synth
received, and background notes every 10 ms landed on top of it. The first probe passed only
because it ran before the notes started. Notes now pause while a probe is out and play through
every fault and recovery.

| Run | Faults | Recovered | Slowest recovery | Messages played |
|---|---|---|---|---|
| macOS, 2 minutes | 35 | 35 | 2.7 s | 14,660 |
| Linux desktop, first 6 h 49 min of a 24-hour run | 6,900 | 6,900 | 2.6 s | 3,267,162 |

The 24-hour run completed with every fault recovered; R-067 records it, and T148 is ticked.

---

## R-056: What running the quickstart between two machines found

**Status**: **FIXED** (2026-09-21), for T154. Scenario 2 run between this Mac and the Linux
desktop, each with a daemon on a scratch configuration.

Discovery found the Linux session by name, and `session connect "QS Near" "QS Far"` connected
to it with no address typed. A note, a controller change and a note off played into a port on
the Mac arrived in order at a synth port on Linux, read by `aseqdump`. The Linux daemon was then
stopped for 5 s and started again. The Mac noticed the loss and was reconnected about 7 s after
the peer came back, within SC-003's 10 s. Getting that far found two defects.

**A one-way port on macOS carried nothing.** The model names a port's direction from the routes'
side: an input port is one routes take MIDI from, so applications send into it. ALSA had it
right. CoreMIDI had it backwards:

- an input port was given a virtual source, which applications receive from and cannot send
  into;
- an output port was given a destination, and the daemon had no source to send out of.

So `port create --direction in` on macOS gave applications nothing to send to. The quickstart's
sender reported "no destination". Two-way ports, the default, have both ends, which is why this
went unnoticed. The ends are now the right way round. The source is still created first, so a
two-way port pins its identity to the same endpoint as before, and existing configurations keep
their identifiers. `a_one_way_port_offers_applications_the_end_it_is_for` asks CoreMIDI what
another application sees, and fails with the ends swapped back.

**A session dropping never reached the history.** A session logged "network session lost" and
"established", but recorded neither. The history, which is what FR-046 exists for and what
scenario 4 checks, showed nothing having happened to the session at all. A session now reports
to the daemon, on the channel that already carried invitations:

- connecting;
- every loss;
- the first failure of a streak of failures.

Each is recorded like any endpoint's: "Stage Out lost its connection to Stage In: peer not
responding", then "Stage Out connected to Stage In". A notice is offered, never awaited,
so a full queue cannot stall the supervisor. `a_session_losing_its_peer_and_getting_it_back_is_in_the_history`
switches the far session off and on, and fails with the notices withheld.

---

## R-057: The journal over a real network, and what it repeated

**Status**: **FIXED and VERIFIED** (2026-09-21), for T154 scenario 2 and SC-006.

The quickstart's packet-loss check was run between this Mac and the Linux desktop. The Mac played
a note on, two controller changes and a note off into a port routed to the session, 630 rounds
in 60 s. On Linux, a rule in a table of its own dropped 5% of packets arriving at the session's
data port.

| | Before | After |
|---|---|---|
| Packets dropped | 125 | 132 |
| Counted lost / recovered | 124 / 272 | 132 / 132 |
| Note on repeated with no note off between | 55 | 0 |
| Notes left sounding | 0 | 0 |
| Final controller values | matched | matched |

Nothing was left sounding before either, but a synth that stacks voices would have sounded 55
notes doubled. Two defects together:

- **Each journal described its own packet.** The sender added a packet's messages to its
  journal before building the journal it sent with them. RFC 6295 has a journal describe the
  packets before the one carrying it. A receiver recovering from a gap applies the journal and
  then the packet, so it played the packet's notes twice. That includes any RFC-following
  receiver, Apple's among them. The journal is now taken first.
- **Recovery repeated what had arrived.** A journal covers everything since the last
  acknowledgement, including packets that arrived. Replayed whole, it repeated them.
  The receiver now keeps what it has delivered on each channel: notes sounding, controller
  values, program, bend and pressure. It passes on only what changes that state, as RFC 6295
  has a receiver do. A note off always passes, since one held back by a wrong state would leave
  a note ringing.

With both fixed, each lost packet is rebuilt as exactly the one message it held.

**Found on the way, and fixed**:

- **Nothing a session carried was counted, and it could not be monitored.** Sessions have no
  runtime entry, and counting and the monitor feed hung off one. `status` showed a session that
  had carried 2,700 messages as having received none. `monitor` called a session carrying MIDI
  "not running", against FR-045 and FR-047. Each running session now has counters and a feed
  of its own.
- `status --json` left out `recovered`, the number that shows the journal working, and the
  quickstart's `jq` path named fields that never existed.
- "connected after 1 failed attempts"; two tests asserted the wrong form with `contains`, which
  the right one also satisfies, so they now check the end of the line.

**Seen, and left**: a lost link is silenced twice, once by the protocol layer and once by the
connection state machine. Each is a deliberate guarantee. The cost is 48 controller messages
repeated, harmless to a synth.

**Scenario 2 results**, Mac to Linux over the LAN:

- **Discovery:** the Linux session was found by name.
- **Connecting:** by name, with no address typed.
- **Peer restart:** reconnected about 7 s after the peer returned.
- **30 s network cut:** the held note was released on the far synth during the cut; reconnected
  0.7 s after the cut ended.
- **5% loss:** results as in the table above.

Both histories showed every loss and reconnection. A peer running Apple's Network MIDI was run
in R-068, and Wi-Fi roaming and sleep in R-069 and R-070.

---

## R-060: Sessions that are not announced, and a port with nothing above it

**Status**: **DONE** (2026-09-21).

**Announcing is now a preference.** `advertise_sessions`, on by default, decides whether sessions
are announced on the network. Off, a session still listens and can be connected to by address;
no machine browsing sees it. It serves a network where announcing is unwelcome, and it keeps
test runs off the network. Every daemon test that starts a session now writes it off first. The
24-hour soak had announced "Soak In", which accepts anyone, on the local network for a day.
A change applies on `config reload`: each running session is announced or withdrawn, and none
restarts. `announcing_sessions_can_be_switched_off_and_on` checks all of this: a session
unannounced with the preference off but still listening, announced after a reload on the same
port, and withdrawn again. It fails with the preference ignored at start.

**A session could fail to bind when the system gave it port 65535.** Binding moves an odd
control port up to the even one above. For 65535 there is none: the move saturated, stayed on
65535, and repeated for every attempt until binding gave up with "no port pair". macOS hands out
ports from 49152 to 65535, so a session left to the system's choice could fail to start, rarely
and at random. It surfaced as one failure of `a_port_the_system_chooses_is_even` in a full run of
the suite, which 23 runs since did not repeat. Past the top, binding now asks the system again.
`the_highest_port_is_not_a_dead_end` binds starting from 65535. It failed with "no port pair"
before the change.

---

## R-063: A peer that restarts, and traffic from anyone else

**Status**: **VERIFIED** (2026-09-22) between two Midi Harbor daemons on one Mac. Untested against
Apple's Network MIDI and rtpmidid.

A daemon killed and started again keeps its session's port but has no session on it. Its peer did
not know. It went on sending clock exchanges, the new daemon ignored everything but an
invitation, and the peer found out only when its 35 second liveness timeout ran out. On this Mac,
with `kill -9` twelve seconds after the last exchange, Alice lost Bob 24 seconds after he came
back and reconnected 0.25 seconds later.

Three changes, in `crates/daemon/src/session.rs` and `crates/rtpmidi/src/session.rs`:

- A session with no peer answers a clock exchange, receiver feedback, MIDI, or a late acceptance
  with `BY` on both ports. The peer ends its session and retries. The same run now loses Bob 0.5
  seconds after he comes back, and reconnects 0.25 seconds after that. `IN` gets no `BY`, since it
  asks for a session. `NO` and `BY` get none either, or two sides would trade them forever. One
  goodbye a second goes to an address, however much it sends.
- A control `IN` under a new SSRC, once the handshake is over, is the peer restarting. The old
  session ends silenced, since the restarted peer will never release what it held, and the same
  invitation begins the next. The same SSRC is an acceptance that went missing, and is answered
  again as before.
- A session with a peer no longer hears anyone else. Before, anything reaching its ports went
  into the session. A `BY` from any machine ended it. An `IN` from another machine replaced the
  peer's SSRC and name, while the acceptance went to the peer. Now an outsider's `IN` gets `NO`,
  and its session traffic gets the goodbye above. The peer is matched by address and port, since
  one machine can run several sessions.

Whether Apple honours a `BY` whose SSRC it has never seen is not known. The daemon that restarted
has a new random SSRC, and does not know the old one. Midi Harbor's own sessions do not check it.
Against Apple, the worst case is the old behaviour: the timeout.

The failure is still reported as `PeerTimeout`, "peer not responding". A peer that said goodbye
has no reason of its own in `FailureReason`, and adding one is a contract change.

Tests, each failing when its rule is removed:

- `a_peer_that_restarted_is_reconnected_in_seconds`: Bob crashes and restarts, and Alice hears of
  it within five seconds.
- `a_peer_that_restarted_and_invites_again_replaces_its_old_session`: Bob drops the old session,
  and Alice is connected within 1.5 seconds, less than her two second wait to invite again.
- `another_machine_cannot_end_or_join_a_session_with_a_peer`: a third socket's `BY` changes
  nothing, and its `IN` gets `NO`. Matching the peer by address alone fails it.
- `a_peer_inviting_again_under_a_new_ssrc_has_restarted` and
  `a_repeated_invitation_from_the_same_peer_is_answered_again`, in the session machine.
- Pure tests of which packets get a goodbye, and of the once-a-second limit.

---

## R-065: Recording against Apple's Network MIDI on the same Mac

**Status**: **SUPERSEDED** (2026-09-22) by R-068, for T081. The session control and clock
interoperated, but MIDI did not pass reliably in either direction. R-068 found the likely cause
the same day, a conflicted Bonjour name on this Mac, and carried MIDI both ways with Apple. What
follows is left as it was recorded.

Apple's session was "Session 1" in Audio MIDI Setup, on port 5004, macOS 15. `tests/interop.rs`
drove our side, with `HARBOR_RECORD_PEER` now taking an address as well as a local port. A Swift
client played the same script into "Network Session 1" and printed what it delivered.
`tcpdump` recorded both sides. There were three peers: the driver on this Mac, the same binary
on the Linux desktop (built on Debian), and a minimal initiator in Python written to rule our
encoder out.

What Apple did:

- With "Who may connect to me" left as it was, every invitation was answered with `NO`.
- Set to Anyone, it accepted both ports and kept the clock in sync. Apple opens its own exchange
  a second after joining. It ends the session with `BY` about two seconds in, unless the initiator
  has opened another exchange by then. Ours opens one every second at first, so it was never
  ended.
- No MIDI arrived from Apple, and Apple sent no receiver feedback for ours, from any of the three
  peers. That held for packets in Apple's own form too: no delta time and no journal.
- With two participants, Apple sent its MIDI, journal included, to the second only, and sent
  feedback for the first one's notes. That did not repeat.
- The Swift client playing the moment the connection came up sent nothing on the wire. Apple's
  own clock exchange comes about a second later, and the client now waits two seconds.
- Restarting `MIDIServer` did not change it. Nothing was logged under MIDIServer.

Apple's own packets from those runs are in `tests/interop/apple_network_midi.txt`: `OK` with its
name, `NO`, `BY`, `CK` in each role, and three MIDI packets. The first MIDI packet has no journal,
and the other two carry Chapter N. `everything_apple_sent_is_understood` parses each of them. Two
things in them are worth knowing:

- Apple's MIDI list uses no delta time before the first command. Our packets use one, which the
  RFC allows.
- Apple clears the Y bit in every note log, which RFC 6295 (A.6) makes a recommendation to skip a
  recovered note on rather than play it late. Our recovery honours it, so the journals recorded
  here rebuild nothing.

Apple's `RS` was seen but its bytes were not kept.

Not tried: Apple initiating, from Audio MIDI Setup's directory, which is how most people
connect. The interop test is left for when someone is at the Mac.

---

## R-066: System-exclusive over network sessions

**Status**: **DONE** (2026-09-22). Tested between two daemons over loopback. Not yet seen against
another implementation.

Sessions did not carry dumps. A route into one counted each dump as undelivered (R-030), and a
dump arriving from a peer was worse. The packet parser read its `F0` as a one-byte message and the
first data byte after it as malformed, so it refused the whole packet, and the notes sharing it
were lost too.

RFC 6295 (section 3.2) carries a dump in the MIDI list, divided into segments when it is longer
than one packet should carry. A segment's first and last bytes say where it falls:

| Segment | Framing |
|---|---|
| a whole dump | `F0 ... F7` |
| the first of several | `F0 ... F0` |
| a middle one | `F7 ... F0` |
| the last | `F7 ... F7` |
| an abandoned dump | closed by `F4` |

What was built:

- **The packet codec** reads and writes segments in their place among the messages. A dump ends
  running status. A real-time byte inside a dump is played as a message just before it, not kept
  in the dump. A segment the list ends before closing is refused, as is any other status byte
  inside one.
- **The session machine** sends a dump as whole segments of at most 1024 bytes, one per packet,
  each with its journal like any packet. With the headers that stays inside one Ethernet frame.
  On receipt it puts the segments back together and delivers the dump in its place among the
  notes.
- **Loss** discards a dump in progress, because a gap in the sequence may have held one of its
  segments, and part of a dump is never delivered. A dump that never ends is given up past
  256 KB, the bound the daemon puts on one from a port. The journal carries no Chapter X, so a
  lost dump is not rebuilt. It is simply not delivered.
- **The daemon** gives each session one ordered channel for messages and dumps, so a note sent
  after a dump cannot overtake it. A route into a session now sends the dump.

Found on the way, and fixed separately: a packet arriving after a newer one, or repeated, was
delivered again. Played after its note off, a late note on left the note sounding. Such packets
are now dropped.

`a_long_dump_crosses_a_network_session_whole_and_in_order`, in `crates/daemon/tests/sysex.rs`,
sends 3000 bytes and a note from one daemon's port to another's, and gets both back in order.
The codec and session tests cover each framing, a clock inside a dump, a missing segment, the size
bound and delivery order, and each fails when its rule is removed. The packet fuzz target was
seeded with segmented packets and run for five minutes.

---

## R-068: Sessions with Apple, whichever side invites

**Status**: **DONE** (2026-09-22), for T081. Sessions with Apple carry MIDI both ways, whichever
side invites. rtpMIDI and rtpmidid are still not recorded.

The owner connected "Session 1" to a Midi Harbor session called "Harbor" from Audio MIDI Setup's
directory, on this Mac, macOS 15. The daemon ran from a scratch home, with a virtual port routed
to the session in both directions. A small CoreMIDI client played notes into each end and printed
what arrived at the other. `tcpdump` recorded loopback and Wi-Fi.

**Why Apple did nothing at first.** Clicking Connect put Harbor under Participants, but Apple
sent nothing: no packet on any port, on either interface, and nothing logged by `MIDIServer`.
Adding Harbor by address made no difference. The session's Bonjour name read "Studio Mac
(2)", the name macOS gives a service after a conflict. With the name set back to "Studio Mac", Apple
invited at once. That is probably what R-065 ran into as well: every one of
its runs used the conflicted name. R-065 is left as it was recorded.

**Apple inviting us** works. Apple invited over IPv6 loopback, `[::1]:5004` to `[::1]:5006`,
naming itself "Studio Mac". Notes 60, 62 and 64 went from Apple to Harbor, and 65, 67 and
69 from Harbor to Apple, each on and off, all in order. Apple's packets are in
`tests/interop/apple_network_midi_inviting.txt`, and `apple_inviting_us_is_understood` parses
each one: both invitations, the clock exchange Apple opens and closes, its receiver feedback, and
six MIDI packets. Each note off carries Chapter N for the note on before it. The Y bit is clear,
as in R-065, so recovery sounds nothing.

**Receiver feedback was the wrong shape.** Apple sends `RS` on the control port. The 32-bit
sequence field holds the RTP sequence number in its upper half: `ffff5253 37193c27 fde50000`
acknowledges packet `0xFDE5`. The lower half is not always zero: between two of Apple's own
sessions it read `907c`, so it is ignored. We read the lower half and sent ours there, on the
data port. Two Midi Harbors agreed with each other, so this never showed between
them. Against Apple, every `RS` Apple sent moved our journal checkpoint to 1. Our next packet's
journal read checkpoint `0x0001` instead of `0xFDE6`, and Apple got an acknowledgement of packet
0 each time. Both ends now use the upper half, and ours goes on the control port. A Midi Harbor
from before this change mistrims against one from after it, the same way it did against Apple.

**Stopping the daemon left its sessions open.** The daemon silenced notes on its way out and
exited, and no `BY` went out. Apple kept the old session under Participants and sent its MIDI to
the old ports. The next daemon did answer that stray MIDI with a goodbye, under its own SSRC.
Apple did not recognise it, so it went on sending there, and none of it reached the new session.
The daemon now ends every session before exiting. Checked against Apple: the exit sent `BY` on
both ports, and Apple removed the session from its list.

**Midi Harbor inviting Apple** connected and carried no MIDI at first. Apple accepted on both
ports, ran the clock exchange from each side, and listed the session under Participants, but it
neither delivered our MIDI nor sent its own. That held over IPv4 and IPv6, and under a session
name Apple had never seen.

Apple inviting another of its own sessions on this Mac carried MIDI both ways, so Apple's invited
side works. The difference was the clock exchange. Apple's initiator exchanges once on connecting,
again 1.5 s later, and then the two sides trade a burst of back-to-back exchanges. Ours ran one
on connecting and one a second later, then settled at every ten seconds. With notes played every
0.6 s after connecting, Apple's first arrived 21.5 s in, straight after our fourth exchange. Six
had completed by then, counting Apple's two. Apple-to-Apple reaches the same count within 1.5 s.

Apple, invited, evidently carries nothing until about six exchanges have completed. Ours now run
eight at 250 ms before settling. Measured with a note every 0.1 s from the moment of connecting,
Apple's first note arrived 1.73, 1.73 and 1.72 s in over three runs, and ours reached Apple after
1.2 and 1.73 s over two. Apple dropped the notes played before that, and the rest arrived.

---

## R-071: rtpmidid, each way

**Status**: **DONE** (2026-09-22), for T081. Sessions with rtpmidid carry MIDI both ways,
whichever side invites, and rtpmidid's packets replay through the parser. Its feedback put the
sequence number where we no longer looked, which is fixed. rtpMIDI, which runs only on Windows,
is still not recorded.

rtpmidid came from its main branch at `7f552d2`, built on an Arch Linux VM with a
self-contained cmake. The VM's own cmake needed a newer `jsoncpp` than the VM had, and the owner
then had the VM upgraded. It ran with discovery off, since mDNS does not cross between the VM's
subnet and the Mac's. The Midi Harbor end was a scratch daemon on the Mac, with a session routed
to a virtual port each way. `aseqsend` and `aseqdump` played and read rtpmidid's ALSA port, and
`tcpdump` on the VM recorded both.

**Both directions, both roles.** With the Mac inviting rtpmidid, three notes went from the Mac to
rtpmidid. Notes, a controller, a program change, pitch bend and a SysEx came back. rtpmidid invites
only when an ALSA client subscribes to the port its `connect` command creates, and it names the
invitation after that client, here "aseqdump". It invited from ports it picked, 51434 and 51435,
and said goodbye when the client went away. The same kinds of message crossed each way. Its
packets are in `tests/interop/rtpmidid.txt`, and `everything_rtpmidid_sent_is_understood` parses
every one: both acceptances and invitations, clock packets in each position, feedback, twelve
channel messages, two SysEx and both goodbyes. rtpmidid sends no journal, and no delta time
before its first command.

**Its feedback reads one half and writes the other.** rtpmidid reads `RS` from the upper half of
the 32-bit field, as Apple does, but writes the lower half: `ffff5253 00003325 00004711`. The
number it writes is not the last packet it received but the checkpoint of the journal in that
packet, so it names the packet before. Since R-068 we read the upper half only. rtpmidid's
feedback therefore named packet 0, which looked stale against sequence numbers near `0x4711`, so
it was ignored and our journal was never trimmed: every packet to rtpmidid carried checkpoint
`0x4711`. Had our numbers sat just below the wrap, the checkpoint would have gone back to 1
instead. `RS` now keeps the field whole, and the session takes whichever half names a packet sent
since the checkpoint, upper first. Checked against rtpmidid: each packet's checkpoint moved to the
one before it, `6fc2`, `6fc3` and on.

**What rtpmidid made of ours.** Its log reads "This RTP MIDI header has journal. WIP." and "This
RTP MIDI payload has delta time for the first command. Ignoring." Both are notes, not refusals:
every message arrived. Its journal support is unfinished, so it recovers nothing we lose. The
second came once for every packet we sent, since we always set the Z flag and wrote a first delta
of zero. Apple and rtpmidid both leave a zero first delta out, and so do we now. The Z flag and
the delta are still written when the first command waits. Against rtpmidid afterwards, six notes
arrived and the warning did not appear.

---

## R-073: rtpMIDI on Windows, each way

**Status**: **DONE** (2026-09-22), for T081. Sessions with rtpMIDI carry MIDI both ways,
whichever side invites, and its packets replay through the parser. That completes T081: Apple's
Network MIDI, rtpmidid and rtpMIDI are all recorded.

rtpMIDI 1.1.14.247, the current release, was installed silently on a Windows 11 VM. Its
configuration app and its virtual MIDI ports work only in the logged-in desktop session, so the
app was driven there through a one-off scheduled task: clicks by position, checked against
screenshots, and a small WinMM client to play into and read from the session's port. The Midi
Harbor end was a scratch daemon on the Mac, on another subnet, with `tcpdump` on the Mac.

**Both directions, both roles.** With the Mac inviting, three notes reached Windows, and notes, a
controller, a program change, pitch bend and a SysEx came back. rtpMIDI, given the Mac as a remote
peer, invited it, and the same crossed each way. Its packets are in `tests/interop/rtpmidi.txt`,
and `everything_rtpmidi_sent_is_understood` parses every one. Like rtpmidid, it sends no journal
and no delta before its first command. Its feedback puts the sequence number in the upper half, as
Apple's does, and its RTP sequence numbers start at zero, not at a random value.

**Worth knowing:**

- Inviting the Mac, rtpMIDI sent its control-port invitation four times, a second apart, and went
  on to the data port only after the fourth. It did so twice, the second time by itself, and we
  accepted each invitation within 2 ms. Inviting the Linux VM, on the same subnet as Windows, it
  went on after the first. The firewall rule rtpMIDI installs allows any address, so that does not
  explain it. Nor does the Bonjour conflict below: with the owner's hostname fixed and the machine
  restarted, rtpMIDI again sent four, at 20:15:31.56, 32.57, 33.57 and 34.64, each answered within
  3 ms. The session came up three seconds late and then worked.
- rtpMIDI keeps a peer it invited as a participant. Disconnecting from our side, it invited again
  33 s later, so disconnecting from a Midi Harbor session does not stick against rtpMIDI. That is
  rtpMIDI's choice, as the side that made the connection.
- Its Bonjour registration failed with -65548, a name conflict with another of the owner's cloned
  Windows guests of the same hostname. The owner renamed the machine. rtpMIDI kept the old name as
  the session's Bonjour name until it was changed by hand. By address, nothing depended on it.
- After the restart, the rtpMIDI service timed out at boot ("waiting for the rtpMIDIService service
  to connect", after 45 s) and had to be started by hand.

---

## R-076: A session carrying several machines

**Status**: **DONE** (2026-09-22), for T168. Changed at the owner's direction.

The spec's edge case asks that two peers inviting at the same moment are both handled without
either being lost. A session held one peer and refused anyone else, so the second was turned away
until the first left. Offered keeping one peer and recording why, or carrying several, the owner
chose several, as Apple's own sessions do.

Decisions:

- **A peer and guests.** The session keeps one peer: the machine it connects to, retries,
  reconnects after sleep and reports in its status. A machine that invites it while it has a
  peer, and that the invitation policy lets in, becomes a guest. Each guest has a session machine
  of its own, with its own handshake, clock exchange, sequence tracking and recovery journal.
- **Same MIDI.** What is routed into the session goes to the peer and every guest, and what any
  of them sends arrives as the session's.
- **Let go, not chased.** A guest invited itself in, so one that says goodbye or stops answering
  is dropped, with its notes released, rather than invited back.
- **Handover.** When the peer leaves and the session goes back to listening, a guest takes the
  peer's place, so a session carrying a machine never reads as listening.
- **Visible.** The contract gains `NetworkSessionDetail.guests`, a new field, and `session list`
  and the window name the guests. The history records each arriving and leaving.

Tests: `crates/daemon/tests/guests.rs`, in which two machines connect to one session, MIDI crosses
from each and to each, and the peer leaving hands the session to the guest. The session test that
expected a second invitation to be refused now expects it accepted beside the peer.

---

## R-080: A port macOS gives to two sockets

**Status**: **CLOSED** (2026-09-25). Fixed in T196.

The daemon's unit tests failed about one run in twenty, always a session test and a different one
each time: a session stayed connecting, or an invitation was never reported. Traced, the
datagrams were sent and never arrived. Bob answered Alice's invitation to `127.0.0.1:49996` three
times, each send succeeded, and the socket Alice held on `[::]:49996` received none of them. The
session tests alone never failed in 30 runs; with the transport tests beside them, two failed in
25.

The transport tests hold plain IPv4 sockets, and macOS lets a dual-stack socket share a port with
one. Measured on this Mac, with Python sockets:

| Held first | Then bound | macOS | Linux (Arch) |
|---|---|---|---|
| 3000 × `127.0.0.1:0` | 3000 × `[::]:0` | 534 on a held port | not tried |
| 3000 × `0.0.0.0:0` | 3000 × `[::]:0` | 564 on a held port | none |
| `0.0.0.0:P` | `[::]:P` by number | allowed | refused |
| `127.0.0.1:P` | `[::]:P` by number | refused | not tried |
| 3000 × `[::]:0` | 3000 × `127.0.0.1:0` | none | not tried |

In every shared case the IPv4 socket receives what is sent to the port over IPv4, and the
dual-stack socket gets nothing. For the daemon this is not only a test fault. A program holding
5004 over IPv4 alone leaves a session on 5004 deaf to every IPv4 peer, and a session whose port
the system chose can land on any port such a program holds. Either way the session binds cleanly
and reports nothing wrong.

Binding over IPv4 is checked against every IPv4 socket, and the system's IPv4 choice avoids ports
in use. So each port is now claimed over IPv4 first, by number or by letting the system choose,
and the dual-stack socket is then bound to that number. A test holds a pair's control port, then
its data port, over IPv4 alone, asks for that pair, and sends to both ports. It failed on every
run before the fix. The unit suite then passed 40 runs in a row.

A port the system chooses is not covered by a test that always fails without the fix: catching
the old behaviour needs the system to pick a held port, which it did for 534 binds in 3000 with 3000
ports held, and far less often with the few a test can hold. Only the explicit case is pinned.

---

## R-082: A port held by a program being started

**Status**: **CLOSED** (2026-09-25). Fixed in T200.

On the Arch VM the session tests failed about one run in thirty: a session switched off and
straight back on came back on another port pair. Less often a session restarted with its daemon
did the same, or one never connected at all. The Mac never showed it. Stopping a session waits
for its sockets to close, and the log showed them closed before the port was asked for again, yet
binding it failed with "address in use". Run with `ss` on every refusal, nothing held the port by
the time `ss` looked.

The holder was a program being started. Every daemon start runs `systemctl --user
show-environment` to learn whether systemd can run it as a service. On Linux the child begins as
a copy of the daemon's process, holding a copy of every socket it has, and gives them up only
once `systemctl` is running in its place. The audit log showed 33 attempts to run it in one run of the
session tests, three for each start as the child tried each place on `PATH`, and a child was
running one in the same millisecond as a refused bind. Tests run many daemons in one process, so one daemon starting held
another's port for those few milliseconds. On macOS a program is started in one step, and no copy
of a socket is ever held.

A daemon running as a service starts that program before any session binds, so the collision
needs two daemons in one process. Whatever the daemon or a library starts later would do the
same, though, and moving away from a port leaves every peer that knew it retrying a port with
nothing behind it. So a port pair asked for by number is now tried again while it is held, every
25 ms for up to 225 ms, before another pair is chosen.

Tracing the binds turned up a second fault. When the claim over IPv4 succeeded and the dual-stack
bind was then refused, the port was bound over IPv4 alone, a fallback meant for a system without
IPv6. The pair then had an IPv4 data socket beside a dual-stack control socket. Every send to an
IPv4 peer is addressed as IPv6 on such a pair, and an IPv4 socket refuses all of them, so the
session never got past inviting the data port. A port refused because it is in use is now treated
as taken.

| Session tests on the Arch VM | Failures |
|---|---|
| Before | 3 in 80 |
| After | 0 in 100 |

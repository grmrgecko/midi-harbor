# Research: Resilience

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-010: Detecting sleep/wake and network changes

**Decision**: Platform-specific event sources behind a single `SystemEvents` trait, **backed by a
platform-independent heuristic**.

| Event | macOS | Linux |
|---|---|---|
| Sleep / wake | IOKit `IORegisterForSystemPower` | `org.freedesktop.login1.Manager.PrepareForSleep` over D-Bus |
| Network change | `SCNetworkReachability` / `nw_path_monitor` | netlink `RTMGRP_IPV4_IFADDR` / `RTMGRP_IPV6_IFADDR` |

**Status**: **ASSUMED**; moderate risk.

**Rationale**: These are the events that drive FR-024 and FR-025. The important finding is that
**the platform APIs are a convenience layer, not a correctness layer** — a machine can suspend
without the API firing, and logind is known not to emit `PrepareForSleep(false)` when resuming from
hibernation. Relying on them alone would leave exactly the silent-dead-connection failure this
product exists to eliminate.

**Therefore**: the authoritative recovery mechanism is the per-session liveness check — missed
clock-synchronisation responses on RTP-MIDI, and characteristic notification silence on BLE. The
platform events are an *optimisation* that makes recovery near-instant instead of waiting for a
timeout. Correctness must not depend on them, and the test suite must prove recovery works with the
platform event source disabled.

A third, cheap signal worth adding: a monotonic-versus-wall-clock gap detector. A task that ticks
every second and observes that substantially more wall-clock time passed than expected has almost
certainly just come back from suspend, on any platform.

**Alternatives considered**: `choreo-power-events` — a convenience wrapper over the same APIs;
reasonable, but the trait boundary is needed regardless and the dependency adds little. Polling
interface lists — too slow, too coarse.

---

## R-031: The comment said it silenced; the code did not

**Status**: **VERIFIED** (2026-09-20) on macOS over CoreMIDI and Linux over ALSA.

`delete_virtual_port` carried the comment *"Silence before tearing down, so nothing is left
sounding on the far side"* directly above the teardown, and the CLI help said "Delete a port,
silencing it first". Neither did. Disabling a port did not either, and stopping the daemon did
not: a held note sounded on whatever received it until something else stopped it, and nothing
else would — the only thing that knew a note was playing had just exited.

`EventKind::NotesSilenced` had been defined, given a wire name, and never emitted.

**Per channel, not all sixteen.** Sending a reset on every channel is the usual approach and is
what the network session path already did, but a virtual port is shared: resetting a channel
nothing was playing clears sustain and cuts sound for whatever else is driving that port.
`core::sounding` keeps a bit per note per channel, updated from what leaves an endpoint, so
silencing touches only the channels that were actually played and an idle port is left alone.

The record is updated from outbound traffic only. A note sounds on whatever *received* it, so the
endpoint that sent it is the one that will have to stop it.

**Measured**: a held note, then `port disable`, produced sustain-off, all-notes-off and
all-sound-off on channel 1 alone, on both platforms. Repeating with `SIGINT` to the daemon
produced the same three, before the socket was removed.

**Not covered**: a device unplugged mid-note. The endpoint that went away cannot be told
anything, and the notes are sounding on its route destinations, which other sources may also be
playing. Stopping only what came from the removed source needs per-source attribution, which is
route-suspension work (FR-032).

---

## R-033: Nothing was watching the machine

**Status**: **VERIFIED** (2026-09-20) by stepping a real machine's wall clock.

The `SystemEvents` seam existed, a fake existed, `Event::Nudge` existed in the connection state
machine with a doc comment describing exactly what it was for, and `on_retry` already reset the
backoff when nudged. Nothing produced a nudge. No backend existed on either platform and nothing
in the daemon consumed the seam, so a machine waking from sleep was invisible.

**The number that makes this matter**: the backoff caps at thirty seconds. A session that was
retrying when the lid closed waits up to thirty seconds after it opens — against SC-003's ten.
The mechanism to avoid that was fully built except for the part that notices.

**Why the clock gap rather than IOKit and logind first.** R-010 already records that the platform
power APIs miss suspends. Comparing the monotonic clock against the wall clock catches every
suspend on both platforms with no bindings and no privileges, because both platforms stop the
monotonic clock while suspended and keep the wall clock running: the difference is how long the
machine was away. The platform backends remain worth adding — they report a suspend *before* it
happens, which this cannot — but they are an addition to this, not a replacement for it.

Both failure modes are benign, which is what makes a heuristic acceptable here. A false positive
costs one reconnection attempt that was going to happen anyway. A missed detection costs the wait
this removes, which is what happens today. Neither can lose MIDI.

Nudges are ignored by sessions that are connected or switched off, so a hint arriving while
everything is fine cannot turn into a dropout.

**Measured**: stepping the wall clock forward twenty seconds on the Linux machine produced
`away_seconds=19` in the log and a `system_resumed` event, through the whole chain from detector
to event log.

**The network half, by the same reasoning.** `NetworkChanged` is produced by comparing the set of
addresses this machine answers on, loopback excluded. An address appearing or disappearing is how
a network comes back without anything announcing it — a cable plugged in, a laptop joining a
different network, a lease renewed on a different address — and a session bound to the old address
never recovers by waiting. `if-addrs` was already in the dependency tree through `mdns-sd`, so
this costs nothing in the headless build.

The platform-native sources are still worth adding, for the same reason as the power ones: they
report the change the instant it happens rather than within a second, and they can say what
changed. They are an addition, not a replacement (T097, T098).

**Measured**: adding an address to a throwaway interface on the Linux machine produced
`addresses=30` and a `system_resumed` event; removing it produced `addresses=28` and another. The
event detail names which change prompted the reconnection, because a reconnection that happens on
its own reads as a spontaneous dropout otherwise.

---

## R-034: A suspended session looks exactly like a working one

**Status**: **VERIFIED** (2026-09-20) between the macOS and Linux machines, measured both ways.

R-033 framed the wake case as a session waiting out a thirty-second backoff. That was the wrong
mechanism, and the nudge built for it did nothing at all in the case it was named after.

After a suspend, a session is still in `Connected`. Nothing ran while the machine was away, so
nothing failed and nothing was scheduled. The nudge only acted on sessions that were retrying, so
it skipped the one case that mattered. What actually notices is `LIVENESS_TIMEOUT`, which is
**thirty-five seconds** — more than twice SC-004's budget, and the backoff never enters into it.

Thirty-five seconds is right for an ordinary quiet session: a link that is merely idle is not a
link that is broken, and a shorter timeout would drop working sessions. What was missing is that
a hint changes the question. After a wake or an address change the link is *suspect*, and the
right move is to ask the peer now and give up in seconds rather than in half a minute.

`Session::probe` sends a clock exchange immediately and sets a two-second deadline. An answer
clears it, so a working link survives any number of hints. Repeated hints do not push the deadline
out, or a machine reporting changes steadily would never let a probe conclude — which is not
hypothetical: adding one interface produced three address changes in three seconds.

**Measured**, by dropping the session's UDP between the two machines so the link died silently:

| | time to notice |
|---|---|
| with a hint to prompt a probe | **3.8 s** |
| with nothing to prompt one | **37.2 s** |

The second number is the behaviour every release before this one had, and it is what SC-004's
fifteen seconds was being missed by.

---

## R-035: A thirty-second retry ceiling cannot meet a ten-second promise

**Status**: **VERIFIED** (2026-09-20) by simulation over the real backoff and measured between the
two machines.

Writing the test for SC-003 was what found this. An interruption that leaves this machine's own
addresses alone — an upstream router rebooting, a switch power-cycling — reports nothing when it
ends. The address watch cannot see it and the peer cannot tell us. The only thing that finds out
is the next scheduled retry, so the ceiling on that delay *is* the recovery time.

The ceiling was thirty seconds, with full jitter. Simulating a hundred interruptions over the real
backoff: **53 of 100 took longer than ten seconds**, some over twenty-five. SC-003 asks for at
most one in a hundred. It was never close.

Thirty seconds is a sensible default for a retry that costs something. A network session's retry
is three small packets, and waiting half a minute between them saves nothing worth having while
being most of the time a user spends wondering why their keyboard stopped working.
`BackoffPolicy::responsive` caps at five seconds and is what network sessions use; the default
stays for everything else.

**Measured** on the two machines, blocking the session's UDP without touching any address: the
session gave up, retried through a twenty-second outage, and reconnected **2.8 seconds** after the
block was lifted.

The division of evidence is deliberate. The distribution — "at least 99 of 100" — is a property of
the jitter and the ceiling, which a simulation over the real `Backoff` can assert every run. That
one interruption recovered in 2.8 seconds is evidence the mechanism works end to end, not evidence
about the ninety-nine.

---

## R-044: A flapping link was retried four times a second, forever

**Status**: **FIXED** (2026-09-21) in the state machine, with the delays tested against an
injected clock.

The spec's rapid connect/disconnect edge case asks two things: the backoff keeps a flapping link
from consuming resources, and the user can see that the link is unstable. The state machine did
neither. It reset the backoff on every successful connection, so a link that came up and dropped
at once went straight back to the shortest delay. Its eight retries were spaced at 250 ms, 250 ms,
250 ms and so on, which is what the regression test prints when the old reset is put back. Nothing
distinguished it from a link that had simply failed once.

A connection now resets the backoff only once it has stayed up for `STABLE_AFTER` (10 s). Until
then the backoff carries on growing, so the same eight flaps end with retries 16 s or more apart.
After `UNSTABLE_AFTER` (3) brief connections in a row the state reports the link as unstable,
until a connection lasts. `ConnectionState.unstable` carries this over the wire (field 6, added
within `midiharbor.v1`). The CLI shows `connected (unstable)`, and the GUI says it keeps dropping
within seconds of connecting.

Two further findings:

- **Bluetooth reconnected on sight.** A remembered device was reconnected every time it was
  heard, whatever its backoff said, so the damping above never reached it. It now waits out the
  backoff. Simply ignoring an early sighting would be worse than no damping, because the radio
  may not report the device again. So an early sighting schedules the reconnect for when the
  backoff ends. The test flaps a device until it is unstable, then brings it back once, and
  requires that single sighting to reconnect it. Dropping early sightings instead fails it.
- **The session supervisor had a second backoff that decided nothing.** It was reset on
  connect, on nudge and on success, and never asked for a delay. Retry timing always came from
  the state machine. It was removed.

**Not covered**: hardware that flaps by being unplugged and replugged gets a fresh state on each
arrival, because the operating system decides when it comes back, so there is nothing to space
out and no history to call it unstable by.

---

## R-045: "No network" and "the peer is not answering" were the same message

**Status**: **VERIFIED** (2026-09-21) on Linux, with one daemon inside `unshare -n`, a network
namespace with no network at all, and one outside it.

The spec's no-network edge case asks that sessions report waiting for a network rather than
repeatedly reporting connection failures. Every unanswered invitation was reported as
`network unreachable`, with a warning for each retry. That happened with no network at all, and
equally on a working network where the peer was simply not running.

**Deciding that there is no network.** Counting the machine's addresses does not work: an
interface that is up carries an IPv6 link-local address with no network behind it, and on macOS
tunnel interfaces carry one even with Wi-Fi off. What is reliable is the kernel's own answer. A
send with no route is refused at once with `NetworkUnreachable`, `HostUnreachable` or
`AddrNotAvailable`, before anything reaches the wire. The session marks itself waiting for a
network when that happens and clears the mark on the next send that goes out.
`ConnectionState.waiting_for_network` carries it (field 7, added within `midiharbor.v1`).

| | Before | After |
|---|---|---|
| No network (`unshare -n`) | `retrying`, `network unreachable`, a warning per retry | `waiting for network`, logged once |
| Network up, peer not running | `network unreachable`, a warning per retry | `peer not responding`, one info line |

The `PeerTimeout` message changed from "peer stopped responding" to "peer not responding", which
also fits a peer that never answered. The slug, which is the contract, is unchanged. A lost
connection still warns every time; only repeated failed attempts went quiet.

---

## R-050: One endpoint being retried stopped MIDI everywhere

**Status**: **FIXED** (2026-09-21), for T071a (FR-029).

FR-029 requires healthy connections to carry on while another is failing or retrying. Network
sessions and Bluetooth links always ran on tasks of their own. Virtual ports and hardware that
failed to open were retried by one loop in `supervisor.rs`, and that loop broke FR-029 three ways:

- **It held the daemon's lock through the platform open.** Every route takes that lock to find
  its destinations, so a device held by another application, retried every few seconds, stopped
  all MIDI in the daemon for as long as each attempt took to answer. With opens that took two
  seconds, a healthy route between two other ports stalled for 1.98 s. The lock is now taken to
  claim the attempt and to record its outcome, and released in between.
- **It ran the open on a runtime worker.** A backend's open waits for its platform thread to
  answer. Waiting on a worker held up whatever task tokio had queued behind it there, which no
  idle worker can take over. With the lock released but the open still on a worker, the
  daemon's own tasks and the test's alike stalled for the full two seconds. Opens now run through
  `spawn_blocking`.
- **It retried one endpoint after another.** Three devices slow to answer took 4.7 s to come
  back, against 1.5 s for one. A fault in one attempt would also have ended retrying for every
  endpoint. Each attempt now runs on its own task.

Releasing the lock opens a window: the endpoint can be switched off, deleted or reopened while
the platform answers. Switching it off or deleting it removes its runtime, and switching it back
on starts a new one. The outcome is therefore recorded only onto the same attempt's runtime,
still connecting with nothing open. Otherwise whatever the attempt opened is closed again.

`crates/daemon/tests/isolation.rs` covers all three defects and the window:

- a healthy route stays under 250 ms throughout two seconds of slow retries;
- a device switched off mid-retry is not left open;
- three slow devices come back in parallel.

The fake platform gained `delay_opens` to make opens slow. Each test fails with its fix reverted.
One case is not induced: a re-enable whose own open finishes before the retry's. A uniform delay
cannot order the two that way, so the guard against it is reasoned rather than tested.

Two other opens are unchanged:

- `reconcile` still opens virtual ports under the lock. It runs when the configuration changes,
  which is a user acting rather than an endpoint failing.
- Hardware plugged in is opened outside the lock but on a worker.

Both happen once per change rather than every backoff step, and a real backend answers in
milliseconds. They matter only if an open ever hangs.

---

## R-051: Releasing held notes before the machine sleeps

**Status**: **VERIFIED** (2026-09-21), for T097 and T098, with a real suspend on each platform in
R-070 and R-072.

R-033 added the polled watcher, which finds a suspend only once the machine is back. That is
enough to reconnect (FR-024), but too late for one thing: a note held on another machine. When
this machine sleeps, the far side hears nothing more. It releases the note only when its own
liveness check gives up, more than half a minute later. Only the platform says a suspend is
coming, and both platforms will wait for a process that asks them to:

| | Announcement | Hold |
|---|---|---|
| Linux | logind `PrepareForSleep(true)` | a `delay` inhibitor lock, released by closing its descriptor, bounded by `InhibitDelayMaxSec` (5 s by default) |
| macOS | IOKit `kIOMessageSystemWillSleep` | answering with `IOAllowPowerChange`; an unanswered message holds the machine for 30 s |

On a suspend still to come, the daemon silences every route, network sessions and Bluetooth
links included. It then calls `SystemEvents::ready_for_sleep`, and the backend lets the suspend
go. Each backend also lets it go after `READY_BOUND` (3 s) on its own: a stalled daemon must not
keep a machine awake. The polled watcher reports a suspend only paired with its resume, after the
fact, and the daemon ignores a suspend that its resume already follows. Releasing notes on waking
could cut one someone has just started playing.

Both backends run beside the polled watcher (`CombinedSystemEvents`), never instead of it, since
R-010 records that both platforms miss suspends. They report sleep and wake only. Network changes
stay with the polled address watch, which finds them within a second. A native source would save
that second and nothing more.

**Measured on Debian 12**, with the real binary and a scratch configuration:

- `systemd-inhibit --list` shows `Midi Harbor … sleep … delay` while the daemon runs, and
  nothing after it exits.
- A forged `PrepareForSleep(true)` sent with `dbus-send` is ignored: only logind's own bus name
  is believed.
- With that sender check removed in a throwaway copy, the same forged signal went through the
  whole chain:
  - the daemon logged `releasing held notes`;
  - the lock was gone within two seconds;
  - `PrepareForSleep(false)` took it again.
- The check restored, the forgery is ignored once more.

**On macOS**, `registers_for_power_notifications` confirms that IOKit accepts the registration,
which is the step that fails if the declarations are wrong. Answering the messages has only been
reasoned through: every `kIOMessageSystemWillSleep` path reaches `IOAllowPowerChange`.

`crates/daemon/tests/suspend.rs` covers the daemon's half. A note held through a loopback
session is released at the far synth on a pending suspend, and the platform is told it may
proceed. A suspend already over releases nothing. Each fails with its rule reverted.

---

## R-067: The 24-hour soak, and a session between the two machines

**Status**: **DONE** (2026-09-22).

**The soak (T148, SC-007)** ran on the Linux desktop for a full day, from `0206fa6`: two daemons in
one process joined by a session, with faults injected in turn. It finished with

```text
done: 24278 faults over 86400.134791996s, every one recovered, slowest 2.558007105s,
11511276 messages played
```

Every fault recovered, and the slowest took 2.56 s against SC-003's ten seconds. The run does not
cover the session work done the same day, which landed after it started.

**A session between this Mac and the desktop**, over the LAN, with today's build on both:

- A 3002-byte dump and a note behind it went from the Mac's port to the desktop's, arriving whole
  and in order, and then the same in the other direction, checked byte for byte (R-066).
- The Mac disconnecting left both sides listening. The desktop did not invite the Mac back, and
  its history reads "MacLink ended its session with PCLink; PCLink is listening again".
- On the desktop, nothing was remembered but the kernel's own Midi Through port: the `aseqdump`
  and BlueZ clients that had opened ports during the day left no entry behind.

**A second soak** (2026-09-23 to 24) covered the session and routing work since. The first
attempt, from before `c4f3c1a`, failed in hour 22: at cycle 22,579 a keyboard replug was not
recovered within ten seconds, and the run printed nothing that said why. `c4f3c1a` made an
unrecovered fault print each side's session phase, attempt and peer, every route, the last 30
events and the keyboard's handle. The run from `c4f3c1a` on the Arch VM then finished clean:

```text
done: 24654 faults over 86402.498228452s, every one recovered, slowest 3.215248496s,
10316356 messages played
```

The hour-22 failure did not recur, so its cause is unknown. If it comes back, the diagnostics
will say what state it was in.

---

## R-069: Roaming between two access points

**Status**: **DONE** (2026-09-22), for T154 scenario 2. The session survived one roam and
reconnected a second after the other. No note was left sounding. The history blamed the peer
for a network this machine had lost, which is fixed.

The owner has two access points with the same network name and authentication. A session ran
from this Mac, on Wi-Fi, to the Linux desktop on the LAN, today's build at each end. The desktop
routed the session to a virtual port whose output `aconnect` fed back into its input, so whatever
the Mac sent came back. A CoreMIDI client on the Mac sent a numbered message every 50 ms and
logged when each returned. A CoreWLAN watcher logged the Wi-Fi channel, since macOS hides the
access point's identity from unprivileged tools. Each roam was forced by restarting the access
point the Mac was on. The Mac kept 192.0.2.10 throughout, so neither roam tested a change of
address.

**First roam, pitch bend.** Messages sent from 14:40:40.95 to 14:40:46.45 were lost, 103 of
them, and the Mac was on the other access point (channel 36 to 157) at 14:40:46.65. The session
never dropped, and messages came back from the moment the Mac rejoined. The round trip, 7 ms
before, ran up to 180 ms for about half a minute on the new access point and then settled. The
counters put almost all the loss on the Mac's side: 4,565 sent, 4,467 received by the desktop.
Every packet in this stream carried a newer pitch bend than its journal, so recovery correctly
applied nothing, and this pass shows only that the session held.

**Second roam, notes.** Each 50 ms tick turned the previous note off and the next one on.
Messages stopped coming back at 14:48:02.86. The Mac left its access point at 14:48:13, lost its
addresses at 14:48:16, and joined on channel 40 at 14:48:35.
The history read:

```text
14:48:16  system_resumed          this machine's addresses changed; reconnecting
14:48:18  endpoint_state_changed  MacLink lost its connection to PCLink: peer not responding
14:48:36  system_resumed          this machine's addresses changed; reconnecting
14:48:37  endpoint_state_changed  MacLink connected to PCLink after 3 failed attempts
```

The session reconnected 1.0 s after the addresses came back, well inside SC-003's ten seconds.
Dropping the session released the notes sounding through it. At the end, of 9,795 notes played
and 9,198 heard back, none was left sounding.

**The lost connection blamed the peer.** At 14:48:15.6 the session logged that it had no network
to reach the peer by. Three seconds later its check on the peer timed out and was recorded as
"peer not responding". A timeout was always reported as the peer's; only unanswered invitations
took the missing network into account. A check that timed out with nothing able to leave the
machine is now "network unreachable" as well.

**Not covered:** a roam that changes the address, which needs two networks, and messages the
network loses during an outage. Those are only as recoverable as the journal makes them, as in
SC-006.

---

## R-070: Sleep and wake, and a session that carried nothing

**Status**: **DONE** (2026-09-22), for T154 scenario 2. A session from this Mac to the Linux
desktop comes back within half a second of the Mac waking, and no note is left sounding. The
first run found a session that said it was connected and carried nothing, and the desktop kept
waking the sleeping Mac. Both are fixed.

The setup was R-069's: the Mac, on Wi-Fi and mains power, invited the desktop, which sent back
whatever arrived. A note played every 50 ms, each turning the previous one off. The owner slept
the Mac from the Apple menu and woke it by hand. `pmset -g log` gave the Mac's own sleep and wake
times.

**The Mac did not stay asleep.** Each time, about 32 s after the Mac went to sleep, the desktop's
liveness check gave up on it and the desktop invited it again. Within a second the Mac woke
briefly, a "DarkWake" that `pmset` puts down to `wifibt`. The session reconnected, the Mac slept
again about 45 s later, and the cycle repeated. The first run woke five times this way, between
15:08:10 and 15:13:27, and the second once, at 15:30:46. Every one came a second after the
desktop's invitation. So a Midi Harbor peer keeps a sleeping Mac on mains power awake for about
45 s in every 80. The desktop keeps inviting because a session whose peer went quiet chases
it, whichever side invited: only a peer that says goodbye is let go.

**A session that said it was connected carried nothing.** After the owner woke the Mac at
15:14:03, both ends read connected. The Mac sent a note every 50 ms, and the desktop's received
count stayed at 3,706. Clock exchanges went both ways, so neither side timed out. The Mac's
packets carried sequence `0x72af` and a journal checkpoint of `0xab65`, which is ahead of
anything that session had sent. At the last dark wake, the desktop's fresh session had taken in
MIDI still coming from the Mac's old session, whose sequence numbers were near `0xab65`, because
a session took MIDI from any source. The Mac then began a new session from a random sequence
14,500 packets behind that. The desktop dropped every one of those packets as late, and would
have gone on dropping them for about twelve minutes. Its feedback acknowledging `0xab65` also
trimmed the Mac's new journal. A session now takes MIDI and feedback only from the source agreed
in the handshake.

**On the fix,** the second run slept at 15:30:13. The desktop re-invited at 15:30:45, and the Mac
dark-woke and reconnected at 15:30:46. MIDI was back 31.5 s after it stopped, and it carried on
through the owner's full wake at 15:31:05. The only note sounding afterwards was the one playing.
SC-004 asks for a reconnect within 15 s of waking, and both runs reconnected within a second of
each wake.

**Saying goodbye before sleep.** The owner chose to have a machine end its sessions as it goes
to sleep, so the peer stops inviting it. The first try sent an ordinary goodbye and reconnected
after five seconds awake without a wake notice. It failed twice on the Mac:

- macOS went on running for five seconds after the notice of sleep, and a slow Bluetooth
  acknowledgement for another one and a half. The fallback counted that as time awake and
  reconnected just as the Mac went down, so the desktop woke it 35 s later. The fallback is now
  sixty seconds awake. Normally the wake notice reconnects at once.
- When the desktop had made the connection, an ordinary goodbye is what a restarting peer sends,
  so the desktop invited again 0.3 s later. The Mac, not yet asleep, accepted. Who made the
  connection also drifts: whichever side re-invites after a timeout becomes the one that made it.

The goodbye before sleep now carries `SLEP` in its token field, which other implementations
ignore on a goodbye. A Midi Harbor peer that receives it goes back to listening, whichever side
made the connection, and the sleeping machine reconnects on waking. Checked with the desktop
having made the connection: the Mac slept at 17:23:43 for 62 s. `pmset` shows no wake in that
time, and the desktop's history reads "MacLink ended its session with PCLink; PCLink is listening
again". The owner woke the Mac at 17:24:45, and the session was up 0.4 s later. Apple and other
implementations see an ordinary goodbye and may still invite a sleeping Mac.

---

## R-072: Sleep on Linux, and NetworkManager going first

**Status**: **DONE** (2026-09-22). A Linux machine going to sleep now tells its peer, as the Mac
does (R-070). Before, NetworkManager had the network down before the daemon looked.

An Arch Linux virtual machine slept through logind with
`systemctl suspend`, into s2idle. An RTC alarm did not wake it from there, and a key press through
`virsh send-key` did. A Midi Harbor session on the VM was connected to one on the Mac, by address,
and `tcpdump` on the Mac recorded what reached it.

**What happened first.** logind announced the suspend at 18:38:58.917, and NetworkManager, reacting
to the same signal, took `eth0` down 0.2 ms later, finishing in 31 ms. The daemon holds a delay
lock, so logind waited for it, but it looked for events only once a second. It saw the suspend at
18:38:59.19 and found no network to send the goodbye by, so the Mac heard nothing. The Mac timed
out 32 s later ("peer not responding") and invited the sleeping machine until it came back. The
notes released before sleep went nowhere for the same reason, so on a machine running
NetworkManager, notes held on the peer sounded until its liveness check gave up.

Worse, NetworkManager's teardown arrived in the same batch as an address change, and the daemon
read that as a reason to reconnect. The session just ended for sleep was reconnected at once, on
the way down.

**The fix.** A backend that reports a suspend now signals the daemon, which acts on it at once
rather than at its next look. The watcher also remembers that the machine is going to sleep and
ignores address changes until it wakes, or until it has been awake as long as the sessions wait
(R-070). On the next suspend the daemon ended its session at 18:46:58.7111, 0.2 ms before
NetworkManager logged the request. The Mac, which had made the connection, recorded "VMSide ended
its session with MacSide; MacSide is listening again", and nothing reached the VM until it woke 50
s later. It reconnected 0.8 s after waking.

This is a race the daemon now wins by a fraction of a millisecond, not an ordering anything
guarantees: NetworkManager does not wait for other holders of the delay lock. A slower machine may
still lose it now and then, and the peer then falls back to its liveness check as before.

---

## R-074: Restoring controller state on a recovered link

**Status**: **DONE** (2026-09-22), for T155 (FR-027, constitution Principle I). A converge pass
found that the connection state machine asked for a restoration on every recovery and nothing
acted on it. A fresh session starts with an empty recovery journal, so a volume moved while a link
was down stayed stale at the receiver, and a Bluetooth synth switched off and on came back at its
defaults.

`midi_harbor_core::controls::Controls` keeps, per channel, the last value of every controller
below the channel-mode range, the program, the pitch bend and the channel pressure an endpoint was
sent. On recovery it produces, per channel, bank select, then the program it qualifies, then the
other controllers in order, then pitch bend and pressure.

Decisions:

- **Where it is kept.** A network session's supervisor records everything routed into it, and
  keeps the record across attempts, since routes to a session stay live while it reconnects. A
  control moved during an outage is therefore restored. A Bluetooth link's routes are suspended
  while it is down, so its record holds what was last delivered: a device that lost its settings
  gets them back, and a change made during the outage does not reach it.
- **What is left out.** Notes, which FR-026 silences and which must never be replayed; the
  channel-mode controllers (120 and up), which are commands, not state; and data entry,
  increment, decrement and the RPN and NRPN selectors (6, 38, 96 to 101), which only mean
  something in sequence. Replayed in numeric order, data entry would land on whichever parameter
  the later selector chose. A stale pitch-bend range is the lesser harm than a wrong one.
- **Reset all controllers** clears the channel's record except its program, which it does not
  reset.

Tests: `crates/core/src/controls.rs` for the rules, `crates/daemon/tests/restoration.rs` for a
fader moved while a session was down, and a Bluetooth test for a device that drops and returns.
Each fails with the restoration switched off.

---

## R-079: Hardware on real Macs, and a MIDI server that dies

**Status**: **CLOSED** (2026-09-25). Recovery on macOS is built and checked (T192). Linux has
nothing to recover from, and its checks with the pad controller turned up four smaller faults, all fixed
(T193 to T195). Replacement under an installed launchd service was checked on 2026-09-25, with
the owner's permission, and works.

Two Macs, then one Linux machine, were checked with real USB MIDI hardware.

A Mac running a presentation program, with a generic USB MIDI interface (maker "Generic", model
"USB Midi ", with a trailing space) that has nothing cabled to it. A scratch daemon with its own
config and socket, built without Bluetooth so no permission prompt reached that screen, found the
interface, read its maker and model, bound routes to it by name despite the trailing space,
refused a two-route loop, carried 8 messages out to it, kept its virtual ports' CoreMIDI ids across
a restart, and paused and resumed its routes across `device disable` and `device enable`. Two
CLI labels were wrong there: the program's Apple network session read "physical", and a
switched-off device read "attached". Both are fixed (they read "provided" and "disabled").

The development Mac, with a USB pad controller.
Pads routed to a virtual port arrived through the route in 0.26 ms median, 6.3 ms worst, with
every note-on and note-off accounted for. A first count seemed to lose note-offs; the listener
read only the first message of each CoreMIDI packet, and a corrected listener saw 5 of 5.

The first unplug crashed Apple's `MIDIServer` (SIGSEGV in `MIDIObject::GetIntegerProperty`,
under `MIDIGetNumberOfSources` and `ObjectTreeCache::GetObjectTree`, serving a client's request
for the object tree while the device was being torn down). The daemon re-reads the device list
the moment a removal is reported, so it may have been the client that asked; the notification
logger that also enumerated inside its callback did not crash the server on a later unplug, so
this is a race, not a certainty. launchd restarted the server, and the daemon never recovered:
the device stayed absent after it was plugged back in, and creating a port failed with
OSStatus -50.

What a client sees when `MIDIServer` dies, measured by killing it (`killall MIDIServer`, which
runs as the user):

- No notification of any kind.
- Every call that reaches the server fails from then on: a property of the client's own endpoint
  and creating a port return -50, finding its own endpoint by unique id returns -10842.
- Sending on its own virtual source returns 0 and goes nowhere.
- Cached counts, such as the number of sources, stay at their old values.
- Creating a new client in the same process also returns -50, and still did 150 s later with a
  new `MIDIServer` running and serving other processes.

So nothing inside the process can recover. The daemon has to notice by asking, since nothing
tells it, and has to be replaced by a new process. The owner accepted recovery taking longer than
the usual five seconds for this case, as long as it comes back without anyone stepping in.

**The recovery (T192).** Every two seconds the CoreMIDI thread creates and drops a private output
port; a failure means the server is gone. The daemon then stops serving, silences what it can and
ends its network sessions, and replaces itself with the same executable and arguments through
`exec`, so the process identifier launchd or systemd knows does not change. The new process
reads the configuration, recreates the ports with their pinned identifiers, reopens the hardware
and routes, and records a warning saying why it started. If the server is not answering yet, it
waits up to thirty seconds for it rather than failing. Killing `MIDIServer` under a scratch
daemon with the pad controller routed:

- The probe noticed 2.8 s after the kill, and the new process was serving 0.4 s later.
- The virtual ports came back with the same CoreMIDI identifiers, and a note crossed a routed
  pair of them.
- The owner's pads came through the route, 5 note-ons and 5 note-offs of 5, and after an unplug
  and replug, 11 of 11. That unplug did not crash the server; the crash is a race, which is why
  killing the server is how this is tested.
- The window showed "cannot be reached" while the daemon was down and filled in again by itself.

**Stopping with the window open never finished.** Found while checking the window: stopping
waited for every connection to close, and a window keeps streams open that never close by
themselves. A daemon asked to stop, by Ctrl-C, by `service stop` or to be replaced, kept running
until it was killed, and a kill skips silencing held notes. Stopping now waits two seconds for
requests already being answered and then closes the remaining connections. With the window open,
a stop request now ends the daemon in 2 s.

**Under launchd.** The app bundle was built from the current code, copied to `~/Applications`,
and its binary ran `service install --start` on the development Mac, with a fresh configuration
holding one port, "Launchd Check". `killall MIDIServer` at 18:45:47 UTC. The probe reported the
server gone at 18:45:51.46, the daemon logged that it was restarting to reach it, and the new
process was listening at 18:45:51.96 with the port open again. launchd listed the same process
identifier, 67582, and a last exit status of 0 before and after: it saw no exit and started
nothing. The daemon started exactly twice in its log, and the history read "restarted because the
MIDI server stopped answering; ports, hardware and routes were set up again". The service,
configuration, log and app copy were removed afterwards.

**Linux, with the pad controller.** On an EndeavourOS machine (kernel 6.18), a headless build
from the Arch VM ran as a scratch daemon with its own config and socket. Nothing was built on
that machine and no kernel module was touched.

- Pads routed to a virtual port arrived identical and in order, 9 of 9 notes, under 1 ms behind
  a listener on the device itself (shell timestamps, which resolve no finer).
- Seven unplugs. Each time the device was reopened about 10 s after it went away, the route went
  to waiting and back, and notes came through afterwards.
- A pad held through an unplug was silenced 1 ms after the daemon saw the removal.
- There is no server to lose. The ALSA sequencer is part of the kernel, not a process that can
  crash and be restarted under a client. Unloading `snd-seq` is the nearest equivalent, and it
  cannot be done while PipeWire holds it, so it was not tried.

Four faults turned up there:

- **`Midi Through` read "physical".** A client counted as hardware when its number was below
  128. `Midi Through` is a kernel client numbered among the sound cards, with no card. Belonging
  to a card is now what makes a client hardware (T193).
- **Other programs' ports were described as hardware.** `aseqdump` was logged as "opened
  attached hardware", and a routed application port that closed would have been recorded as
  "unplugged". Those now read "provided port", "went away" and "is back" (T193).
- **Every replug zeroed the device's counters.** The pads read "received 0" beside a route from
  them that had carried 58 messages. A device's counters now last while it is unplugged and carry
  on when it returns (T194).
- **No note-off was sent for a held note.** Silencing sent sustain-off, all-notes-off and
  all-sound-off, while the comment on it said a synth ignoring all-notes-off would still honour
  explicit note-offs. Nothing sent them, though every held note is tracked. Each held note now
  gets its own note-off, after the pedal release. The same check found the broad resets breaking
  the rule above that only the notes the stopped route carried may be stopped: two controllers on
  the drum channel into one sampler, and unplugging one cut the other's held note. A channel
  another route still has notes sounding on, at the same destination and MIDI Out, now gets only
  the stopped route's note-offs. A channel nobody else is playing still gets the full reset,
  which is what stops a drone or an envelope that ignores note-off (T195). Measured: a held pad,
  then the cable pulled, and the virtual port received sustain-off, note-off 82, all-notes-off
  and all-sound-off, the received count carried from 15 to 17, and the route agreed at 17.

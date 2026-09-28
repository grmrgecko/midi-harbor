# Research: Hardware Devices

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-024: Listing a device is not the same as opening one

**Status**: **VERIFIED** (2026-09-20). Found by a user trying the monitor, not by any test.

Attached hardware was discovered, listed as an endpoint, and accepted as a route source — and
carried nothing, because the platform backends never opened it. `route list` reported such a route
as `ok`, and the monitor answered with a raw identifier the user had never seen.

That combination is worse than an outright failure: every surface claimed the setup was working.

**What it took to fix**: open each present device, connect an input port to it so what it sends
reaches the ring buffer, hold an output port so it can be sent to, and start a dispatch loop for
it exactly as for a virtual port. A device that cannot be opened now records the failure on its
connection state instead of appearing available.

**The lesson for the test suite**: every routing test used virtual ports, which the daemon does
open. The tests were right and the product was wrong, because both sides of the test shared the
one assumption that was false.

---

## R-027: ALSA has no concept of opening a device

**Status**: **VERIFIED** against real ALSA on Debian (2026-09-20). MIDI carried end to end on
Linux for the first time.

CoreMIDI and ALSA disagree about what connecting to hardware means, and the seam has to absorb it.

CoreMIDI opens an input port and calls `connect_source` on the device. ALSA has no equivalent: a
client creates a port of its own and *subscribes* it to another port, and MIDI flows along the
subscription. So the Linux backend creates a bridge port per device and subscribes it in each
direction the device offers, which is what makes the device's MIDI reach the ring buffer and ours
reach the device.

Two consequences worth recording:

**Subscriptions outlive the port unless removed.** Deleting a bridge port without unsubscribing
leaves the device believing it still has a reader. Each bridge records its subscriptions and
removes them before deleting the port, sender side first so the device stops sending before what
it sends to disappears.

**The address is not the identity.** A device comes back at a different client and port number
after a replug, so the fingerprint's topology path is tried first for an exact match and the port
name is the fallback. That fallback is what lets a route survive a device being unplugged and
plugged back in.

Verified with `aconnect -l` showing the bridge port subscribed both ways to the kernel loopback,
then by playing a file into the hardware and watching the notes arrive at a routed virtual port.

---

## R-038: Hot-plug was never wired up

**Status**: **VERIFIED** (2026-09-20) on macOS with a real device taken offline, and on Linux with
an ALSA client appearing and leaving.

Setting out to implement FR-015f — silence the destinations of a route when its source device is
removed — turned up two larger gaps underneath it.

**Nothing drained the platform's events.** Both backends collect device arrivals, removals and
setup changes on their own thread and hold them until they are taken. Nothing took them. The
enumeration only ever ran at startup, so FR-015c did not hold at all: hardware plugged in while
the daemon ran never appeared, and hardware removed stayed listed as attached forever. The same
shape as R-033 — every piece built except the one that connects them.

**An offline endpoint is not a removed one.** On macOS an interface that is switched off, or an
IAC bus that has been disabled, keeps its endpoint and is marked `offline` rather than being
removed. Enumeration ignored the property, so such a device was listed as attached and routes to
it reported that they were fine. Linux has no equivalent: an ALSA port that is gone is simply
absent.

**FR-015f itself is per route, not per endpoint.** A destination can be fed by several sources, so
only the notes that came along the route that stopped may be stopped. Silencing the whole
destination because one source went away would turn one silent instrument into all of them, which
is why the notes are tracked per route as well as per endpoint. The same moment arrives when a
route is switched off or deleted while holding notes, so all three go through one path.

**One consequence worth recording**: with a background task re-enumerating, two runs of
`refresh_devices` can now overlap — the watcher's and a caller's. Each reads the platform, decides
what changed, and then writes, so interleaved they can undo each other. It is serialised.

**Measured** on macOS: a held note, the device taken offline, and the destination received
sustain-off, all-notes-off and all-sound-off on the one channel that was playing, with the route
moving to waiting. Bringing it back made the route live again in **0.99 s**, inside SC-010a's two
seconds. On Linux a new ALSA client appeared in the device list within 2 s and was marked absent
within 2 s of going away.

---

## R-039: Hardware could be remembered but never forgotten

**Status**: **VERIFIED** (2026-09-20) on macOS, attached and unplugged.

Every device ever seen is remembered so its routes survive being unplugged, which is FR-015d and
right. `ForgetPhysicalDevice` was a stub, so nothing could say a device is not coming back. Making
hot-plug work in R-038 sharpened this: on Linux every application that opens a sequencer port is
now recorded the moment it starts, so a config accumulates every tool that has ever run.

**Forgetting attached hardware is not a mistake to prevent.** It is how someone starts over with a
device whose settings have gone wrong: the entry goes, the hardware is rediscovered, and it comes
back with defaults. What would be wrong is doing that silently, so the response says the device is
still plugged in and has been listed again.

That needed an explicit re-enumeration. Nothing changed on the platform, so no event would have
prompted one, and the device would have been missing from the list until something unrelated
happened — wrong about hardware that is plugged in.

Routes naming a forgotten device are kept and reported, not removed. They read broken with the
name missing, which is the same treatment a deleted port gets: the user decides whether to repair
or remove them.

**Measured**: forgetting an attached device listed it again immediately with a new identity;
forgetting an unplugged one named the route it orphaned, which then read broken with the device
missing.

---

## R-040: Two identical devices grew the configuration by an entry every refresh

**Status**: **VERIFIED** (2026-09-20) on Linux with three genuinely identical ALSA clients.

FR-015g asks that two physically identical devices be told apart. Attaching two produced **three
endpoints all answering to one name**: the stored entry, marked absent because the match was
ambiguous, and both attached devices as new hardware. Routes name their endpoints and the router
looks them up in a map, so a route to that name bound to whichever one the map yielded — silently,
and not necessarily the same one after a restart.

Underneath that were two worse defects, neither visible with one device:

- **`endpoints_for` claimed the first device that matched at all, not the best one.** With
  identical hardware every candidate compares as *something*, so both stored entries bound to
  whichever device came first in the list, leaving the rest unclaimed and added again on the next
  pass. Three `aseqdump` clients on the Linux machine produced **284 entries in about seventy
  seconds** — a configuration file growing twice a second for as long as the devices stayed
  plugged in.
- **`best_match` called it a tie when position could have settled it.** `compare` returns
  `Probable` for a full descriptive match before it ever looks at where the device is plugged in,
  so two identical devices tied at `Probable` even when one of them was in the very socket the
  entry remembered. Ties now prefer the candidate in the remembered position.

Names are made unique from the most stable thing that differs — serial, else position, else the
platform's own identifier. Moving the cable therefore renames the device, which is inherent: two
identical devices with no serial cannot be told apart by anything except where they are.

`ResolveAmbiguousDevice` binds a stored entry to the hardware the user names, adopting its
fingerprint and removing the stand-in entry, so the routes bound to that name follow the device
they chose.

**Measured**: three identical ALSA clients gave three distinct names and **four** configuration
entries, unchanged after a further ten seconds of refreshing.

---

## R-047: Linux hardware identity from the USB device, not the sequencer's numbering

**Status**: **VERIFIED** (2026-09-21). The sysfs reading was checked on real USB hardware.

Linux fingerprints were the ALSA client's name and `alsa:client:port`. The client number
depends on the order things appeared, so a device replugged under a different number matched its
stored entry by name alone, which is `Ambiguous`, and gained a new entry each time (R-043).

A hardware client belongs to a sound card, and a USB card's device directory in sysfs holds what
stays put:

| Field | From | Stable |
|---|---|---|
| `usb_serial` | `serial`, plus `#port` | Wherever it is plugged, when the maker set a serial |
| `topology_path` | the device directory's name, such as `usb-1-1.1:0` | In the same socket |
| `manufacturer`, `model` | `manufacturer`, `product` | Always, and together they make a descriptive match |

The port number is appended to the serial because one serial covers every port of an interface,
and a serial match is taken as certain. Without it, the second input of an interface would be
taken for its first. Software clients have no card and keep `alsa:client:port`.

On the Linux machine, the three USB sound cards read back as the Yeti (serial `REV8`, socket
`1-1.1`), the C920 (serial, but no manufacturer string) and the FiiO DAC (no serial, socket
`1-8`). The kernel's `Midi Through` client, which has no card, kept its ALSA numbering. The
parsing is also tested against a constructed tree, which runs on any machine.

**Pre-release note**: fingerprints stored on Linux before this change use the old form, and
will be added again once, as new hardware. There are no users to migrate.

---

## R-055: Two daemons on one Linux machine opened each other's ports without end

**Status**: **FIXED** (2026-09-21). Found while preparing the two-machine quickstart run, when a
daemon left over from an earlier test was still running on the Linux desktop.

On Linux, opening a device means creating a bridge port of our own and subscribing it to the
device. The bridge ports, and the port that receives the sequencer's announcements, were created
with the subscribe permissions, so every other client could connect to them. Device enumeration
takes any port with those permissions for a device.

With two daemons on one machine, each found the other's bridge ports and announcements port and
opened them. Each opening made another bridge port, which the other daemon found and opened in
turn. The leftover daemon had reached 23 ports within minutes, nearly all named `announcements`
or `Midi Through Port-0`, and was on its way to the kernel's limit of 254 per client. Two users
each running the service on one machine would do the same. So would a test run beside a real
daemon.

ALSA asks for the subscribe permissions only on the end of a subscription that the subscriber
does not own. Midi Harbor owns its end of every bridge and of the announcements subscription, so
those ports now have none, and are marked `NO_EXPORT`. Other clients cannot connect to them, and
enumeration skips them. Virtual ports keep the permissions, since connecting to them is their
purpose.

**Measured** with the real binary, two daemons with separate configurations side by side:

- the ports each holds stayed at 2 (the announcements port and one bridge to Midi Through) after
  5 s and after 30 s;
- a client started afterwards (`aseqdump`) appeared as attached hardware within 3 s, so
  announcements still arrive;
- the Midi Through loopback test still passes, so bridges still carry MIDI both ways without the
  permissions.

`another_instance_does_not_take_our_own_ports_for_devices` has two backend instances. One opens
Midi Through, and the other must list Midi Through once and no announcements port. With the
permissions restored it lists Midi Through twice.

macOS is not affected. Opening a device there connects to its endpoints without creating any,
so there is nothing for another instance to find.

---

## R-013: Physical MIDI device identity and hot-plug

**Decision**: Physical devices are surfaced by the same platform trait that creates virtual ports
(R-003), and identified by a composite key rather than by index or name.

**Status**: **ASSUMED**; moderate risk, and the main reason R-003 chose direct bindings over
`midir`.

**Rationale**: FR-015e requires that reattaching the same device — possibly to a different USB port
— restores its routes rather than presenting a new endpoint, and FR-015g requires telling two
identical devices apart. Neither is possible from a display name alone, and port index is exactly
the thing that changes on replug.

**Identity strategy**:

- **macOS**: read `kMIDIPropertyUniqueID` from the entity or device, plus manufacturer, model and
  driver-reported name. CoreMIDI's unique ID is persistent per device across replug for most
  class-compliant hardware, which is the primary key.
- **Linux**: the ALSA sequencer's client name and port, combined with the underlying card's USB
  serial number where the device reports one, read from sysfs. Devices that report no serial
  number fall back to name plus physical topology path, which is stable per USB socket but not
  across sockets — a documented limitation to surface honestly rather than paper over.
- Stored identity is recorded with a confidence level, so the interface can say "this looks like
  the device your route refers to" instead of silently binding a route to the wrong hardware.

**Hot-plug notification** (FR-015c) uses the same notification sources R-003 already requires: the
`MIDINotifyProc` on macOS, and ALSA sequencer announce-port subscriptions on Linux. There is no
extra machinery — this is a direct dividend of choosing direct bindings.

**Repeater loop detection** (edge case, FR-033): loops formed across two machines cannot be
detected by inspecting local route configuration alone. The mitigation is a per-message origin
marker carried in the internal routing representation and, for network sessions, a session-level
identifier that lets a receiver recognise MIDI it originally sent. This costs a small amount of
per-message state and must be designed in from the start rather than retrofitted.

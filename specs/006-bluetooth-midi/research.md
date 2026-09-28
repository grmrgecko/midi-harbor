# Research: Bluetooth MIDI

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-006: Bluetooth LE MIDI — central and peripheral roles

**Decision**: Split by role behind the platform trait.

| Role | macOS | Linux |
|---|---|---|
| Central (connect to devices) | `btleplug` 0.13.1 | `btleplug` 0.13.1 |
| Peripheral (advertise ourselves) | `ble-peripheral-rust` 0.2.0 | `bluer` 0.17.4 |

**Status**: peripheral role **VERIFIED** on macOS (2026-09-20, spike T008). RISK-2 is retired.
Central role **VERIFIED** on Linux against a real adapter (2026-09-20, T132). Linux peripheral
advertising **VERIFIED** on the air (2026-09-20, T134). The macOS peripheral backend reaches
`start_advertising` without error on a real adapter (2026-09-20, T133), which is as far as one
radio can check it. MIDI crossing a real link in both directions was verified in R-064, with
the Mac as central and Linux advertising; a Linux central connecting to the Mac is refused by
BlueZ, for the reason R-064 gives.

**One gap the library imposes.** `ble-peripheral-rust` reports the adapter's state as a single
boolean, while CoreBluetooth distinguishes powered-off from unauthorised. The backend therefore
reports `AdapterOff` for both, which is at least true of each. Telling them apart needs
CoreBluetooth's authorization query directly, and is what T063 will have to resolve — a user
refused for want of permission should not be told to switch their radio on.

**Resolved** (2026-09-22): `bluetooth/authorization.rs` asks `CBManager`'s class-level
authorization, through the `objc2-core-bluetooth` version btleplug already uses. A denied or
restricted process is reported as Bluetooth permission not granted, with the guidance to grant
it in System Settings, for the advertised port when CoreBluetooth reports it unpowered and for the
central, which btleplug calls ready whatever the permission. The mapping is unit tested.

**Spike results (macOS)**: `ble-peripheral-rust` 0.2.0 reported the adapter powered, accepted a
GATT service carrying the standard BLE MIDI service and characteristic UUIDs with Read /
WriteWithoutResponse / Notify, and **successfully started advertising** under the name "Midi
Harbor" (`is_advertising() == true`). Despite being version 0.2.0 it covers exactly the surface
FR-018 needs.

**Consequence: platform parity is preserved.** The plan's anticipated Complexity Tracking row and
the constitution's declared-limitation escape hatch are **not needed**. BLE peripheral support
ships on both platforms.

**One caveat carried forward**: the spike ran as a bare executable from a terminal, not from a
`.app` bundle, and was not prompted for Bluetooth permission — most likely inheriting the terminal
application's existing grant. This does **not** prove a bundle is unnecessary for a service started
by launchd, which has no such inherited grant. The `.app` bundle requirement in T063 stands until
tested from launchd specifically.

**Rationale**: `btleplug` is the mature cross-platform choice but is explicitly host/central only;
its own documentation directs peripheral users elsewhere. `bluest` is likewise central-only.
Peripheral support therefore has to be platform-specific: `bluer` is the official BlueZ binding and
has a working GATT server and LE advertisement registration; on macOS, `ble-peripheral-rust` 0.2.0
wraps CoreBluetooth's peripheral manager. `blew` 0.5.0 claims cross-platform central *and*
peripheral and would collapse this into one dependency — it is worth evaluating in the spike, but
at 0.5.0 it is too young to commit to sight-unseen.

**Consequences**:

- Peripheral role (FR-018) ships on both platforms. The contingency of a Linux-first release with
  the capability query reporting macOS unavailable is no longer required.
- **macOS requires an application bundle for Bluetooth.** CoreBluetooth will not grant access to a
  bare executable: the binary must be inside a `.app` with `NSBluetoothAlwaysUsageDescription` in
  its `Info.plist`, and the user must grant permission. This directly drives FR-021 and FR-039f —
  the service installer must register the bundled executable, not a loose binary.

**BLE MIDI protocol details** (identical on both platforms, so they live in a shared codec crate):

- Service UUID `03B80E5A-EDE8-4B33-A751-6CE34EC4C700`, characteristic
  `7772E5DB-3868-4112-A1A9-F2669D106BF3` with Read, Write-Without-Response and Notify.
- The packet encoding carries a 13-bit millisecond timestamp split across a header byte and per-
  message timestamp bytes, with wraparound that must be tracked to reconstruct real time (FR-019).
- Running status may span packet boundaries, and system-exclusive messages may be split across
  packets — both are classic sources of bugs and need explicit round-trip tests.

---

## R-041: Two adapters close together could not hear each other

**Status**: **CLOSED** (2026-09-22). Superseded by R-058 and R-064, which carried MIDI both ways
between this Mac and the Linux desktop.

Bluetooth is the only seam in this project that needs two radios to test, and the two available
ones could not sustain a link. Both are in the same building. Neither ever saw the other advertise.

What was tried, and what each produced:

| Advertiser | Scanner | Result |
|---|---|---|
| macOS, bare executable | Linux `bluetoothctl`, `btmgmt find -l` | not seen |
| macOS, inside a signed `.app` with `NSBluetoothAlwaysUsageDescription` | same | not seen |
| Linux `bluetoothctl advertise` | macOS `CBCentralManager` | not seen in 50 s |
| Linux, this project's `bluer` backend | macOS `CBCentralManager` | not seen in 30 s |

The Linux side is confirmed to transmit: `btmon` shows `LE Set Extended Advertising Enable`
succeeding and the payload carrying `11 07 00 c7 c4 4e e3 6c 51 a7 33 4b e8 ed 5a 0e b8 03`,
which is the BLE MIDI service UUID little-endian, with the name in the scan response.

**The asymmetry is the finding.** In a 30-second discovery each adapter ran alone:

| Adapter | Distinct devices heard |
|---|---|
| macOS (BCM_4387) | 27 |
| Linux (`E8:48:B8:C8:20:00`, internal) | 4 |

The Linux adapter did hear the Mac's controller once, at **-90 dBm** — the noise floor, from a
machine the owner reports is beside it. Third-party devices are heard at inconsistent relative
strengths from the two adapters (`Meshtastic_7fc8` at -78 on Linux and -91 on the Mac,
`Mijia Scale` at -88 on Linux and -79 on the Mac), so the two are not co-located as far as the
radio is concerned. A seven-fold difference in devices heard, in the same room, points at the
Linux machine's antenna being unconnected or shielded inside its case rather than at either
software stack.

**Consequence for the tests.** Everything above the seam is pinned against the in-memory backend,
and the native backends are verified as far as one adapter can go: the adapter opens, the
capability query answers per role, a scan runs and correctly reports no MIDI peripherals among
the non-MIDI devices in range, and an advertisement carrying the right service reaches the air.
What no single adapter can show is a session — connect, subscribe, and MIDI in both directions.
That is what the ignored hardware tests in `crates/platform/tests/bluetooth.rs` are for, and it
needs a second radio that can actually be heard.

**Do not take silence for a software fault.** Every row above looks exactly like a broken
backend and none of them is one. The macOS rows in particular say nothing about whether macOS
transmitted: `isAdvertising` was true and CoreBluetooth reported the adapter powered on, and the
only observer was an adapter that hears a seventh of what its neighbour does. In particular this does **not** bear on T063.

---

## R-058: Bluetooth between the Mac and the Linux desktop

**Status**: partly **VERIFIED** (2026-09-21). A link from the Mac to the Linux desktop's
advertising carries MIDI both ways, and reconnects after the Linux side goes away and returns,
once three defects were fixed. The Mac's own advertising was not visible to any receiver. One
defect is in `btleplug` itself.

No Bluetooth MIDI device was needed: each machine runs a daemon on a scratch configuration and
plays the other role.

**Linux advertises, the Mac connects.** The Mac's scan found the Linux desktop by the MIDI
service at -81 dBm. After a connect, MIDI went both ways, checked by the counters at every hop:

- a note, a controller change and a note off played into a port on the Mac left through the
  link, 3 messages; the Linux side received 3; its route and synth port carried 3.
- the same played into a port on Linux reached a synth port on the Mac, 3 and 3.

**Defects found and fixed**:

- **A device that dropped never reconnected.** Hearing a device advertise is the only sign it is
  back, and the radio listened only during a scan the user asked for, which ends on its own. The
  log said "it will reconnect when the device returns", and it never did, against FR-020.
  - The daemon now listens whenever a remembered device is waiting, and stops once none is. This
    is decided each tick, alongside any scan the user asked for.
  - Verified for real: Linux stopped advertising for 10 s. The Mac logged "listening for bluetooth
    devices to come back", and reconnected by itself 28 s after advertising resumed. The wait was
    the retry backoff, which had grown while the link was down.
- **The central never forgot a device.** A device was reported only the first time it was seen,
  so one that dropped and came back was never reported again. It is now forgotten when its link
  drops.
- **The fake radio reported devices nobody was listening for.** It announced a device coming
  into range whether or not it was scanning, which a real radio never does. That is how the
  first defect passed its tests. It now reports only while scanning. Tests that found a device
  for the first time now scan for it, as a user does. A new test,
  `a_device_lost_after_the_scan_ended_still_comes_back`, drops a device after the user's scan has
  ended; it failed before the fix.

**A defect in `btleplug` 0.13.2, the latest release.** On macOS, connecting to a peripheral that
announces its services changed, which BlueZ's GATT server does, makes CoreBluetooth discover them
again. That second discovery reaches `check_discovered` with no request waiting, and it panics
("We should still have a future at this point!"). The panic kills the thread behind every Bluetooth
central operation until the daemon restarts. With that one line changed to ignore the second
discovery, in a build used only for these tests, the link connected and carried MIDI. The project
carries that change as a patched `btleplug` (the root `Cargo.toml`), submitted upstream as
deviceplug/btleplug#479.

**The Mac's advertising works, without its name.** This entry first said the Mac's
advertisement was seen by nothing, and that macOS keeps a background process off the air. Both
were wrong. The phone check looked for "Harbor Mac" by name, and the `bluetoothctl` scans that
found nothing were fed commands through a pipe, which races. A clean test from the Linux desktop
used `btmgmt find -l`, which asks the kernel for LE discovery with no filter and no agent,
alternating the Mac's advertising on and off:

- advertising on: the Mac appeared at its public address, -74 to -78 dBm;
- advertising off: it did not appear.

`btmon` decoded the 24 bytes of advertising data. They are the flags (`02 01 1a`), a transmit power of
12 dBm (`02 0a 0c`), and the complete list of 128-bit services (`11 07 ...`): exactly the BLE
MIDI service, `03b80e5a-ede8-4b33-a751-6ce34ec4c700`. The scan response was empty. So macOS puts
the service on the air and leaves the name out. Receivers show the Mac under its system name, or
no name at all, and a phone looking for "Harbor Mac" by name finds nothing.

**Why the name is missing.** A minimal Swift advertiser, decoded the same way, showed that macOS
puts the whole advertisement in the 31-byte packet and never uses the scan response:

| Advertised | Bytes on air | Name sent |
|---|---|---|
| MIDI service and "HM" | 28 | yes, `03 09 48 4d` |
| MIDI service and "Harbor Mac" | 24 | no, and the scan response is empty |
| "Harbor Mac" alone | 18 | yes |

The flags (3 bytes), the transmit power macOS always adds (3), and the 128-bit service (18) take
24. The 7 left hold a name of up to five characters. The service has to stay, since MIDI apps
search for it, and a 128-bit identifier has no shorter form. So on macOS a name of five
characters or fewer is sent, and a longer one is dropped without an error. Midi Harbor says so
instead. `SetPeripheralAdvertisingResponse` now carries the name and whether it is sent, as
added fields 2 and 3. The daemon logs a warning, and `bluetooth advertise` names the limit and
what devices will show. `a_name_that_will_not_be_sent_is_said_so` runs the binary with a long
name and a short one. It fails with the check removed.

Linux has no such limit: BlueZ sends a name that does not fit in the scan response, as `btmon`
showed ("Harbor Linux", 14 bytes). The Mac had still listed the Linux desktop with no name. The
cause was ours. The central reported a device only the first time it was heard, and the name
comes in the scan response, after that. A device is now reported again when its name arrives or
changes, and the Mac lists "Harbor Linux". `a_name_arriving_after_the_first_sighting_is_passed_on`
covers it, and fails with only first sightings reported.

The Linux desktop's attempts to connect to the Mac ran into BlueZ treating the Mac as a known
classic device, and then into a pairing held on one side only; R-064 gives the cause.

**Also seen, not yet fixed**:

- `bluetooth connect` reports "connected" when it has only asked, and exits 0 even when the
  connection then fails.
- The advertised endpoint reads "connected" while nothing is connected to it.
- The Linux desktop's advertisement reaches the Mac with no name.
- The Linux desktop found the Mac only intermittently, and could not connect to it. The Mac is a
  dual-mode device, and BlueZ may be connecting over classic Bluetooth, which the Mac does not
  offer for this service.

---

## R-064: Bluetooth between this Mac and the Linux desktop, both ways over one link

**Status**: **VERIFIED** (2026-09-22) for the Mac as central and Linux as peripheral. Linux as
central is refused, below. Scenario 5, on the macOS 15 laptop and the EndeavourOS desktop
(BlueZ 5.x, glibc 2.44), the Mac having forgotten the desktop first. The Linux binary was built
on Debian 12 and copied over, since the desktop is not for building on.

With btleplug taken from the fork carrying the service rediscovery fix (deviceplug/btleplug#479,
patched in by `6ebb3b5`), the Mac connected to "Harbor Linux" and MIDI
went both ways over the one link: note on and off, a controller, pitch bend, and system-exclusive
in each direction. `monitor` on the Mac showed all of it, in and out.

Four defects were found and fixed first:

- **A second link to the same device.** `bluetooth connect` on a device that had just reconnected
  by itself opened a second link and put it in place of the first. Both delivered, so every
  message arrived twice (6 counted in for 3 sent), and the first link could never be closed. A
  link already open or opening is now left alone, checked and recorded under one lock.
- **System-exclusive to the advertised port vanished.** The platform seam had no way to notify a
  dump, although both peripherals could, and the advertised port has no platform handle, so the
  daemon's loop fell through without counting it. Linux took in 4 messages and sent 3. There is
  now `notify_sysex` on the seam, and the daemon has a branch for the advertised port.
- **`monitor` refused every Bluetooth endpoint** as not running. Watching required a platform
  handle, which neither a link nor the advertised port has.
- **The Linux peripheral kept a departed central.** It found out a central had gone only when a
  write to it failed, so after the Mac left, the port said "connected" for as long as nothing
  was sent. Subscriptions are now checked once a second. After a `bluetoothctl disconnect`, the
  port went back to advertising within a second. The central is now named by its address.

Each fix has a test in `crates/daemon/tests/bluetooth.rs` that fails without it, apart from the
Linux sweep, which only a real BlueZ can exercise.

Known limitations:

- **Linux as central to the Mac fails** with BlueZ's `br-connection-canceled`. BlueZ knows the Mac
  as a dual-mode device and tries Classic, while BLE MIDI needs LE. btleplug's `Device.Connect`
  lets BlueZ choose. Newer BlueZ has a `PreferredBearer` device property. The Mac advertising, and
  Linux seeing it, both worked. Checked again on 2026-09-25: the desktop's BlueZ 5.87 does not
  expose `PreferredBearer` on `Device1`, because it is experimental there and `Experimental` is
  off in `/etc/bluetooth/main.conf`, so the daemon cannot ask for LE on that machine without a
  system setting changed. This affects a dual-mode peer such as another computer. A Bluetooth MIDI
  instrument is LE only, and BlueZ has no Classic link to try.
- **Both systems' own MIDI drivers take the link too.** The connection bonds the two machines.
  After that, macOS shows a "Linux Desktop Bluetooth" MIDI device and BlueZ's MIDI plugin shows "Studio Mac (2)
  Bluetooth", each of which the other side's daemon then lists as attached
  hardware. The system link also outlives our own `bluetooth disconnect`.
- **For four minutes the Mac ended every link 29 seconds after it connected.** btmon on Linux
  showed every ATT request in both directions answered, then "Remote User Terminated
  Connection". Cutting the system link from Linux ended it, and it did not recur in the next 90
  seconds with either build. Not explained.
- On Linux, other applications' sequencer clients, such as `aseqdump`, were remembered as
  hardware, and on macOS so were Apple's IAC buses and network session. The owner chose to keep
  such a port only while it is open or a route names it. That is now done: discovery marks a port
  as software (a sequencer client numbered 128 or above; a CoreMIDI endpoint with no driver, or
  Apple's IAC or network driver), and one that goes away unrouted leaves the configuration. On
  this Mac, a program's port was listed while open, marked `software: true`, and gone from the
  file once the program quit.

Fixed after this run:

- `bluetooth connect` to a device out of range now says so and suggests a scan, exit 5. It had
  said "does not exist", or "device was removed" when the radio had lost the device just before.
- The radio reporting a link closed after it had failed replaced the failure's reason with
  "device was removed", and after a user's disconnect it logged a reconnect. Such a report is
  now ignored when no link is held.
- The advertised port counted messages as sent when no device was subscribed. They are now
  counted as undelivered on the route.

---

## R-075: Applying Bluetooth MIDI timestamps

**Status**: **DONE** (2026-09-22), for T162 (FR-019).

The decoder rebuilt each message's time from the device's thirteen-bit millisecond clock, and
dispatch dropped it. A device collects what is played into packets sent once per connection
interval, 7.5 ms or more, so two notes played 10 ms apart could reach the synth together.

`midi_harbor_core::devicetime::DeviceTiming` relates the device's clock to this machine's by the
latest arrival seen: local time minus device time, at its largest. A message is held until its
device time plus that, so notes sharing a packet come out as far apart as they were played, and a
packet that arrives on time is not held at all.

Decisions:

- **Bounded.** Nothing is held longer than 10 ms, about one connection interval. Holding longer
  trades more latency than the spacing is worth.
- **Relaxing.** The anchor comes down by 1/64 of the device time that passes, so one late packet
  does not make every later message wait. The anchor is kept in microseconds so that relaxation
  too small to show in milliseconds still adds up.
- **Ordering.** One link's packets arrive in the order the device sent them, and the decoder emits
  their messages in that order, so arrival order is already timestamp order. Only the holding is
  new.
- **Bluetooth only.** Every other source's timestamp is when the message arrived, which says
  nothing arrival order does not.

Tests: `crates/core/src/devicetime.rs` for the rule, and a daemon test in which two notes stamped
10 ms apart, sent together, reach the synth at least 5 ms apart. Without the hold they arrived
292 ns apart.

---

## R-077: Refusing Bluetooth without advice

**Status**: **DONE** (2026-09-23), for T170. Decided by the owner.

A Bluetooth request refused because the radio cannot be used failed with `adapter_unavailable`,
whose guidance was "switch the adapter on". That was wrong for a machine with no adapter and for a
Linux machine where BlueZ was not running, and FR-028 asks for an actionable reason. Offered a new
`FailureReason` variant for the missing service, or rewording the guidance, the owner decided that
when Bluetooth is not available the product says so and nothing more.

Decisions:

- **No advice.** `adapter_unavailable` reads "bluetooth is not available" and carries no guidance.
  It is the one reason the user must act on that has none, a deliberate departure from FR-028's
  "actionable". The closed set is unchanged.
- **Still surfaced.** An endpoint refused this way still reports `Unavailable` rather than
  `Retrying`, and is still retried, so a radio switched on later reconnects it.
- **The cause lives in `capabilities`.** It names no adapter, an adapter switched off, a refused
  permission or a missing BlueZ, as FR-053 asks.
- **A device's own failure is not the adapter's.** A device that answers the connection and then
  fails to list its services or refuses notifications, on a radio that is still on, fails with
  `protocol_error` and the radio's own detail. It was `adapter_unavailable`.

Tests: `unavailable_bluetooth_is_surfaced_without_advice` in `crates/core/src/failure.rs`, and the
`step_failure` tests in `crates/platform/src/bluetooth/central.rs`.

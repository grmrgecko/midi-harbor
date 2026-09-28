# Tasks: Hardware Devices

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 6: User Story 4 — Routing and repeating between endpoints (Priority: P3)

### Tests for User Story 4

- [x] T104 [P] [US4] Tests in `tests/integration/physical_devices.rs` over the fake covering hot-plug arrival within 2 seconds, removal, fingerprint rebinding across a different port, and two identical devices kept distinct (FR-015a–g, SC-010a, SC-010c) — *in `crates/daemon/tests/routing.rs` alongside the other hardware tests rather than a separate file; arrival is now held to the two seconds SC-010a promises rather than four*

### Implementation for User Story 4

- [x] T107 [US4] Implement physical device enumeration in `crates/platform/src/midi/coremidi.rs` reading `kMIDIPropertyUniqueID`, manufacturer, model and name into a `DeviceFingerprint` (research R-013)
- [x] T108 [US4] Implement physical device enumeration in `crates/platform/src/midi/alsa.rs` reading ALSA client/port plus USB serial from sysfs, falling back to topology path when no serial is reported (research R-013, RISK-8) — *sysfs reading in `crates/platform/src/midi/usb.rs`, checked against three real USB sound cards (R-047)*
- [x] T109 [US4] Implement fingerprint matching with `MatchConfidence` in `crates/core/src/fingerprint.rs` — only `Exact` and `Probable` auto-rebind routes; `Ambiguous` surfaces a choice rather than guessing (FR-015e, FR-015g)
- [x] T110 [US4] Implement hot-plug handling in `crates/daemon/src/devices.rs` updating the endpoint list without user refresh, retaining route configuration while a device is absent (FR-015c, FR-015d, FR-015f)
- [x] T111 [US4] Implement the device-claimed-by-another-application path in `crates/platform/src/midi/mod.rs` reporting `DeviceClaimed` and claiming the device when it becomes free (edge case) — *ALSA only, where an exclusive subscription is a real claim: `EBUSY` becomes `DeviceClaimed` naming the holder, and the retry loop reopens it once released, verified with `aconnect -e`. CoreMIDI shares endpoints between applications and has no such claim to report*
- [x] T116 [US4] Implement partial-SysEx discard on mid-transfer device removal in `crates/daemon/src/dataplane.rs` so corrupt data is never forwarded (edge case)
- [x] T119 [P] [US4] Implement `device list|forget|resolve` commands in `crates/cli/src/device_cmd.rs` (contracts/cli-interface.md §3)

## Phase 11: Convergence

- [x] T158 CRITICAL: Report ALSA hot-plug announcements from `drain_input` in `crates/platform/src/midi/alsa.rs` without a mutex or allocation on the sequencer read path, per Constitution III (contradicts) — done: an atomic flag, with an ALSA test that another client's port is still announced

## Phase 12: The redesigned window

- [x] T188 Report each hardware device's maker, model, and MIDI In and MIDI Out counts over the contract, and show them in the window, per R-078, FR-044 — done: the contract and the window already carried maker and model in the device fingerprint, but CoreMIDI enumeration never read them, so no device on a Mac had either; it now reads `kMIDIPropertyManufacturer` and `kMIDIPropertyModel`, treating blank as unreported (ALSA already read them from the USB descriptors), and a stored device lacking either takes it from the hardware it matches without overwriting what is stored, so devices remembered before this are described too. The In and Out counts need no new field: each hardware endpoint is one port pair, so its counts are its direction, which the contract carries and the window shows; a multi-port interface is one endpoint per port, now told apart as the same device by its maker and model. Every check was mutation-tested; checked live on the Mac, where the IAC bus now reads Apple Inc., IAC Driver.
- [x] T190 Recognise a software port that returns under a new platform identifier, such as an IAC bus edited in Audio MIDI Setup, by its name when the remembered one is absent, instead of listing it again beside the old entry the routes still name, per FR-022, FR-032 — done: after matching by identifier, an absent remembered software port is taken back by an unclaimed software port of its name when that settles it, one entry and one port of the name, and takes the port's new identifier so the next pass matches it exactly; hardware is never taken back by name, nor software by hardware or the reverse, nor a port another entry already matched, and two of a name on either side are left for the user. Every check was mutation-tested, including an end-to-end test where a routed bus returns under a new identifier and the route carries again.

## Phase 13: Surviving the MIDI server

- [x] T193 Count an ALSA client as hardware only when it belongs to a sound card, and describe other programs' ports as provided rather than attached or unplugged in logs and history, per FR-015c, FR-046 — done: `Midi Through` reads "provided" on Linux, a routed provided port that closes is recorded as gone and back rather than unplugged, checked on a Linux machine

# Quickstart & Validation Guide: Midi Harbor

**Feature**: 001-service-and-clients, first written as 001-midi-connectivity-manager | **Date**: 2026-09-20

Runnable scenarios that prove the feature works end to end. Each maps to user stories and success
criteria in the spec it validates. Scenarios are ordered so that each is useful on its own — you can
stop after any one of them and have validated a real slice.

---

## Prerequisites

**Both platforms**: Rust 1.96+ (edition 2024).

**macOS**: Xcode command line tools. No further packages — CoreMIDI and CoreBluetooth ship with
the OS. Bluetooth requires the binary to be inside a `.app` bundle with
`NSBluetoothAlwaysUsageDescription`, and permission granted on first use (R-006).

**Linux**: a C toolchain, `pkg-config`, and the development files for ALSA, D-Bus and Avahi, plus
libclang for the Avahi bindings (Debian: `build-essential pkg-config libasound2-dev libdbus-1-dev
libavahi-client-dev libclang-dev`). At run time, `avahi-daemon` for advertising sessions, BlueZ
5.50+ for Bluetooth, and `systemd` for service installation; without it, run the daemon in the
foreground.

**For scenario 2 (network)**: a second machine on the same LAN running Midi Harbor, Apple's Network
MIDI (macOS), rtpMIDI (Windows), or rtpmidid (Linux).

**For scenario 5 (Bluetooth)**: a BLE MIDI peripheral, or a phone or tablet with a BLE MIDI app.

### Build

```bash
cargo build --release                       # full build, GUI included
cargo build --release --no-default-features # headless build — must not pull in libcosmic
```

The headless build succeeding on a machine with no graphics libraries is itself a test
(SC-014a). Verify the dependency tree is genuinely absent, not merely unused:

```bash
cargo tree --no-default-features | grep -i cosmic && echo "FAIL: GUI deps leaked" || echo "OK"
```

---

Scenarios 1 to 5 and 8 moved to the spec each validates: [1](../003-virtual-ports/quickstart.md),
[2](../005-network-ports/quickstart.md), [3](../007-routing/quickstart.md),
[4](../009-observability/quickstart.md), [5](../006-bluetooth-midi/quickstart.md) and
[8](../002-configuration/quickstart.md). They keep their numbers, which the research log cites.

---

## Scenario 6 — Headless parity (US2 · SC-014a, SC-014b)

On a machine with **no desktop environment**, using the `--no-default-features` build:

```bash
./midi-harbor                               # expect: help text explaining the GUI is not included
./midi-harbor gui                           # expect: exit 2 with a clear message
./midi-harbor service install --start
./midi-harbor port create "Headless Bus"
./midi-harbor route create "Headless Bus" "Studio"
# reboot
./midi-harbor status --json                 # expect: everything restored
```

**Pass criteria**: every command behaves identically to the full build (SC-014a), and the GUI was
never built or run.

**Coverage check (SC-014b)**: every IPC request kind reachable from the GUI is reachable from the
CLI. This is asserted by an automated test, not by inspection.

---

## Scenario 7 — Client independence (SC-014)

```bash
midi-harbor session connect Studio <peer>   # establish, confirm MIDI flowing
# open and close the GUI 50 times while watching:
midi-harbor events --follow
```

**Pass criteria**: zero connection state changes attributable to the GUI starting or stopping
(FR-037). Any `EndpointStateChanged` event correlating with a GUI launch is a failure.

---

## Automated equivalents

The scenarios above are manual acceptance tests. Their automated counterparts, which must run on a
CI machine with no MIDI hardware, no peer, and no radio (Principle VI):

| Scenario | Automated coverage |
|---|---|
| 1 | State machine tests over the in-memory platform fake; config round-trip |
| 2 | Loss-injecting RTP-MIDI harness; journal property tests; recorded packet captures from Apple/rtpMIDI replayed against our parser |
| 3 | Device arrival/removal events from the fake; route rebinding by fingerprint |
| 4 | Event history assertions; `FailureReason` exhaustiveness |
| 5 | BLE packet codec round-trip and timestamp-wraparound property tests |
| 6 | `cargo build --no-default-features` in CI; GUI/CLI request-kind subset assertion |
| 7 | Daemon state unchanged across simulated client connect/disconnect churn |
| 8 | YAML round-trip, schema migration, and corrupt-file recovery tests; import and reload over the fake in `crates/daemon/tests/reconcile.rs` |

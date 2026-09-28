# Tasks: Bluetooth MIDI

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 1: Setup (Shared Infrastructure)

### Stage 0 spikes (retire RISK-2 and RISK-3 before committing dependencies)

- [x] T008 [P] Spike in `spikes/ble-peripheral/` evaluating `ble-peripheral-rust` 0.2 on macOS and `bluer` 0.17 on Linux for BLE MIDI peripheral role, and `blew` 0.5 as a unified alternative — resolves RISK-2 (research R-006)

## Phase 2: Foundational (Blocking Prerequisites)

### Platform seam and fake

- [x] T037 [P] Define the `BluetoothPlatform` trait (scan, connect, disconnect, advertise) in `crates/platform/src/bluetooth/mod.rs`

## Phase 8: User Story 6 — Bluetooth LE MIDI both directions (Priority: P5)

**Goal**: Connect out to BLE MIDI peripherals, and advertise this computer as one.

**Independent Test**: Scan, connect a peripheral, route it; separately advertise and connect from a
phone or tablet (quickstart scenario 5).

> **Gate**: the peripheral half depends on spike T008. If it failed on macOS, ship Linux-first and
> record the declared limitation per the plan's Complexity Tracking note.

### Tests for User Story 6

- [x] T128 [P] [US6] Property tests in `crates/blemidi/tests/codec.rs` covering the 13-bit timestamp with wraparound, running status across packet boundaries, and SysEx split across packets (research R-006)
- [x] T129 [P] [US6] Fuzz target in `fuzz/fuzz_targets/blemidi_codec.rs` asserting no panic or unbounded allocation on hostile peripheral input (Principle V)
- [x] T130 [P] [US6] Test in `tests/integration/bluetooth_unavailable.rs` asserting missing adapter, adapter off and permission denied each report a specific reason and leave other features working (FR-021, FR-053)

### Implementation for User Story 6

- [x] T131 [US6] Implement the BLE MIDI packet codec in `crates/blemidi/src/codec.rs` — service `03B80E5A-EDE8-4B33-A751-6CE34EC4C700`, characteristic `7772E5DB-3868-4112-A1A9-F2669D106BF3`, timestamp reconstruction with wraparound tracking (FR-019)
- [x] T132 [US6] Implement the BLE central role over `btleplug` 0.13 in `crates/platform/src/bluetooth/central.rs` — scan, connect, subscribe to notifications, write without response (FR-016, FR-017)
- [x] T133 [P] [US6] Implement the macOS BLE peripheral backend in `crates/platform/src/bluetooth/peripheral_macos.rs` using the crate chosen by spike T008 (FR-018)
- [x] T134 [P] [US6] Implement the Linux BLE peripheral backend in `crates/platform/src/bluetooth/peripheral_linux.rs` using `bluer` 0.17 GATT server and LE advertisement registration (FR-018)
- [x] T135 [US6] Implement pairing memory and automatic reconnection on device return in `crates/daemon/src/bluetooth.rs` (FR-020)
- [x] T136 [US6] Implement Bluetooth request handlers (`StartBluetoothScan`, `StopBluetoothScan`, `ConnectBluetoothDevice`, `DisconnectBluetoothDevice`, `ForgetBluetoothDevice`, `SetPeripheralAdvertising`) in `crates/daemon/src/handlers/bluetooth.rs`
- [x] T137 [P] [US6] Implement `bluetooth scan|connect|disconnect|forget|advertise` commands in `crates/cli/src/bluetooth_cmd.rs`, exiting 4 with guidance when unavailable (contracts/cli-interface.md §3)

**Checkpoint**: All four endpoint kinds are complete and mutually routable.

## Phase 11: Convergence

- [x] T156 CRITICAL: Silence the destinations of every route from a Bluetooth source when its link is lost or a central leaves the advertised port, with a failure-injection test, in `crates/daemon/src/bluetooth.rs` per FR-026, Constitution I, US6/AC3 (contradicts) — done: both paths silence the routes from the source, with a test for each that failed before
- [x] T162 Map Bluetooth MIDI device timestamps to local time and deliver received messages by them rather than in arrival order, with a test, per FR-019, US6/AC4 (missing) — done: messages held to device time against the latest arrival, at most 10 ms (R-075)
- [x] T169 Follow the Bluetooth central's adapter state after startup and report the adapter as off when it is off or switched off, per FR-021, US6/AC6 (partial) — done: the central reads the radio's power at startup and follows btleplug's state updates, reporting the adapter off, or the refused permission on macOS, and forgetting what was in range; the daemon's scan plan already stops and restarts the scan from the capability; verified live on the Mac on 2026-09-23, where switching Bluetooth off in Control Center showed "the adapter is switched off" in `capabilities` within a second and switching it on showed it available again
- [x] T170 Give Bluetooth failures accurate reasons instead of `AdapterUnavailable` when there is no adapter, BlueZ is missing, or service discovery or subscription fails on a working radio, per SC-012 (partial) — done: service discovery and subscription failures on a radio that is still on are `protocol_error` with the radio's detail; for no adapter, one switched off or a missing BlueZ the owner chose to say only that Bluetooth is not available, so `adapter_unavailable` carries no advice and `capabilities` names the cause (R-077)
- [x] T171 Count malformed Bluetooth MIDI packets per endpoint and expose the count additively in the traffic counters, per Edge Case: malformed input (partial) — done: the central and both peripheral roles count each discarded packet on the endpoint's ring, and the dispatch task adds it to `packets_malformed` (field 9 of `TrafficCounters`), shown in `status --json`, the diagnostic report and the window's loss summary; ring overflow, which was only logged, now reaches `messages_dropped` the same way
- [x] T175 Make the GUI's Disconnect on a Bluetooth row close the Bluetooth link instead of calling the session-only disconnect, which fails, per FR-044 (contradicts) — done: a Bluetooth row's Disconnect calls the Bluetooth disconnect, with a test

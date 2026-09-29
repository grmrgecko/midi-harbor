# Implementation Plan: Midi Harbor — MIDI Connectivity Manager

**Date**: 2026-09-20 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-service-and-clients/spec.md`

## Summary

Midi Harbor is a single Rust binary that manages four kinds of MIDI endpoint — virtual ports,
physical hardware, RTP-MIDI network sessions, and Bluetooth LE MIDI links — from one place on both
macOS and Linux, and routes MIDI between any of them. Its defining requirement is connection
resilience: links must self-heal across network changes, sleep/wake, unplug, peer restart and
Wi-Fi roaming, and must recover MIDI state so that no note is left sounding.

The technical approach follows from four findings in [research.md](./research.md). First, Apple's
IAC driver and `MIDINetworkSession` are not programmable in any way that can meet these
requirements, so Midi Harbor creates and owns its own endpoints and sessions on both platforms.
Second, meeting the no-stuck-notes requirement means implementing the RFC 6295 recovery journal,
which no existing Rust crate provides — this is the largest body of work in the project. Third, a
headless daemon must own all state so healing continues when no window is open; the GUI and CLI
are clients over a versioned gRPC contract carried on a Unix domain socket. Fourth, libcosmic was empirically verified to
build *and run* on macOS, retiring the project's largest platform risk before any code was
committed.

## Technical Context

**Language/Version**: Rust 1.96.0, edition 2024

**Primary Dependencies**: `tokio` 1.53 (async, off the real-time path) · `clap` 4.6 (subcommands) ·
`libcosmic` (git-pinned, optional `gui` feature) · `coremidi` 0.9 / `alsa` 0.12 (platform MIDI) ·
`btleplug` 0.13 (BLE central) · `ble-peripheral-rust` 0.2 / `bluer` 0.17 (BLE peripheral) ·
`mdns-sd` 0.21 (discovery) · `rtrb` 0.4 (real-time ring buffers) · `service-manager` 0.11
(launchd/systemd) · `tonic` 0.14 + `prost` 0.14 (daemon gRPC over a Unix socket) · `serde` 1.0 +
`serde_yaml_ng` 0.10 (config) · `directories` 6.0 · `tracing` 0.1. RTP-MIDI is implemented in-project, not
taken from a crate.

**Storage**: a single YAML configuration file in each platform's standard per-user config
directory, holding endpoints and the MIDI connections between them. Written atomically and
schema-versioned. No database. Event history is bounded and in-memory.

**Testing**: `cargo test` with in-memory platform fakes; `proptest` for wire-format round trips;
`cargo-fuzz` targets on every network and Bluetooth parser; a packet-loss injection harness for
the recovery journal. All must pass on a CI runner with no MIDI hardware, no peer, and no radio.

**Target Platform**: macOS 11+ (Apple Silicon and x86_64) and Linux (x86_64 and aarch64). Windows
is explicitly out of scope.

**Project Type**: Desktop application — one binary operating as a background service, a CLI, and
an optional GUI.

**Performance Goals**: Virtual-port added latency under 1 ms mean and under 3 ms at p99 (SC-008).
Network added latency under 5 ms at p99 beyond raw round-trip (SC-009). Physical device repeated
over the network under 10 ms at p99 (SC-010b). Idle CPU under 1% of one core with 10 endpoints,
memory under 150 MB (SC-010).

**Constraints**: No allocation, locks, I/O or logging on the MIDI data path. No elevated
privileges. Headless build must carry no graphical dependencies. 24-hour session with zero
unrecovered disconnections (SC-007).

**Scale/Scope**: Single-user desktop. Tens of endpoints and routes, not thousands. 6 user stories,
70 functional requirements, 22 success criteria.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

Evaluated against [constitution.md](../../.specify/memory/constitution.md) v1.0.0.

| Principle | Design response | Status |
|---|---|---|
| **I. Resilience Is The Product** | One `ConnectionState` machine shared by all four endpoint kinds, with unbounded jittered backoff and no terminal failure state. `Unavailable` is re-evaluated, not terminal. Note-silencing on every exit from `Connected`; state restoration on every re-entry. | PASS |
| **II. Daemon Owns State** | `daemon` crate owns every handle; `cli` and `gui` reach it only through the generated gRPC client. Version lives in the proto package (`midiharbor.v1`), so a major mismatch surfaces as `UNIMPLEMENTED` rather than undefined behaviour. No client-lifetime-scoped resources exist in the protocol. | PASS |
| **III. Real-Time Safety** | `rtrb` SPSC ring buffers at every real-time boundary, carrying fixed-size `Copy` events; SysEx travels as handles into a pre-allocated pool. Counters are atomics; `tracing` runs on ordinary tasks. Overflow increments a counted drop. | PASS |
| **IV. Platform Parity** | Four platform traits (MIDI, Bluetooth, system events, service manager), each with macOS, Linux and fake implementations. `cfg(target_os)` confined to the `platform` crate. Runtime capability query (FR-053) reports genuine limitations. | PASS — with one watch item, below |
| **V. Protocol Correctness** | RFC 6295 recovery journal implemented, not skipped. Interoperability with Apple, rtpMIDI, rtpmidid and hardware is a tested requirement. Parsers treat peer input as hostile; fuzz targets on each. | PASS |
| **VI. Testable Without Hardware** | `core`, `rtpmidi` and `blemidi` carry no I/O and no platform code. Time is injected. Every resilience behaviour has a failure-injection test. | PASS |
| **VII. Observable By Default** | `ConnectionState` exposes phase, duration, last error and next retry. `TrafficCounters` separates network loss from our own backpressure. Bounded event history lives in the daemon, so it survives the GUI. | PASS |

**Watch item on Principle IV — RESOLVED 2026-09-20.** The concern was that BLE *peripheral*
support had no viable macOS crate, which would have forced a Linux-first release and a declared
limitation. Spike T008 proved `ble-peripheral-rust` 0.2.0 advertises a BLE MIDI GATT service on
macOS successfully. **Parity is preserved and no limitation needs declaring.**

**New constraint from spike T009 — CoreMIDI notifications require a CFRunLoop**, and deliver
nothing at all, silently, without one. This does not violate any principle, but it shapes the
platform crate: the `MidiPlatform` implementation owns a dedicated run-loop thread on macOS (and a
descriptor-polling thread on Linux) rather than being a set of free functions. Recorded as RISK-10.

**Gate result: PASS.** No unjustified violations. Proceeding.

### Post-Phase 1 re-check

Re-evaluated after the data model and contracts were written. No new violations. Three design
decisions strengthened compliance rather than eroding it:

- The loop-suppression `OriginTag` and session identifier (data-model §5) were added because
  cross-machine repeater loops cannot be caught locally — designed in from the start per R-013
  rather than retrofitted.
- The `MonitorEndpoint` and `WatchTraffic` gRPC streams are lossy by contract with explicit drop
  counts (ipc-protocol §4). HTTP/2 flow control would otherwise let a stalled GUI apply
  backpressure toward the MIDI data path, which Principle III forbids — the one place gRPC's
  defaults are wrong here, overridden deliberately.
- `FailureReason` is a closed enum rather than a string, so Principle I's "specific, actionable
  reason" is enforced by the type system and maps onto CLI exit codes.

## Project Structure

### Documentation (this feature)

```text
specs/001-service-and-clients/
├── plan.md              # This file, the architecture of the whole project
├── spec.md              # The service and its clients; the original input
├── research.md          # Architecture findings and the tracked risks
├── data-model.md        # Identity and the endpoint, shared by every spec
├── quickstart.md        # Prerequisites, scenarios 6 and 7, automated equivalents
├── contracts/
│   ├── ipc-protocol.md  # Daemon ↔ client gRPC contract
│   └── cli-interface.md # User-facing command contract
├── checklists/
│   └── requirements.md  # Spec quality validation
└── tasks.md             # Phase 2 — created by /speckit-tasks, not by this command
```

Planned as one feature, then split on 2026-09-27 into one spec per capability, 002-configuration
to 012-test-note; [the index](../README.md) lists them.

### Source Code (repository root)

```text
AGENTS.md                      # Code style and contributor guidance
Cargo.toml                     # Workspace root; `gui` feature gates libcosmic
proto/
└── midiharbor/v1/harbor.proto # The daemon contract; generated from, never hand-edited
src/
└── main.rs                    # Argument dispatch only — selects daemon | gui | cli role

crates/
├── core/                      # Domain. No I/O, no platform code, no cfg(target_os).
│   ├── endpoint.rs            #   Endpoint, EndpointKind, identity types
│   ├── state.rs               #   ConnectionState machine, backoff, FailureReason
│   ├── router.rs              #   Route graph, cycle detection, OriginTag suppression
│   ├── config.rs              #   TOML schema, atomic write, migration, corruption recovery
│   └── events.rs              #   Event, bounded history, TrafficCounters
├── rtpmidi/                   # AppleMIDI + RFC 6295. Pure; no sockets.
│   ├── session.rs             #   IN/OK/NO/BY/CK state machine
│   ├── clock.rs               #   CK exchange, offset/latency estimation, liveness
│   ├── packet.rs              #   RTP-MIDI payload encode/decode, delta times
│   └── journal/               #   Recovery journal: chapters N, C, P, W, T; trimming
├── blemidi/                   # BLE MIDI packet codec. Pure; no radio.
│   └── codec.rs               #   13-bit timestamps, wraparound, running status, split SysEx
├── platform/                  # The four seams. All cfg(target_os) lives here.
│   ├── midi/                  #   CoreMIDI | ALSA seq | fake
│   ├── bluetooth/             #   btleplug + ble-peripheral-rust | bluer | fake
│   ├── sysevents/             #   IOKit/SCNetwork | logind/netlink | fake
│   └── capability.rs          #   Runtime capability query (FR-053)
├── ipc/                       # Generated gRPC client and server from proto/; no hand-edited types
├── service/                   # launchd agent / systemd user unit install, stale detection
├── daemon/                    # The engine: owns endpoints, runs supervisors, serves IPC
│   ├── supervisor.rs          #   Per-endpoint reconnect loop driving core::state
│   ├── dataplane.rs           #   Ring buffers, routing hot path — Principle III territory
│   └── server.rs              #   Unix socket, subscriptions, event fan-out
├── cli/                       # clap subcommands; --json projection of ipc types
└── gui/                       # libcosmic views. Optional feature. Thin — no domain logic.

tests/
├── integration/               # Daemon + fake platform, end-to-end over real IPC
├── interop/                   # Recorded captures from Apple Network MIDI, rtpMIDI, rtpmidid
└── soak/                      # Long-running resilience and packet-loss harness

fuzz/                          # cargo-fuzz targets: rtpmidi packets, journal, BLE codec, IPC
```

**Structure Decision**: A cargo workspace of focused library crates behind one thin binary. Three
forces determined this shape rather than a single crate:

1. **FR-039b (headless build)** requires that disabling the `gui` feature removes the libcosmic
   dependency tree entirely. That is only enforceable if the GUI is its own crate that nothing
   else depends on — in a single crate, feature unification would keep dragging wgpu and winit in.
2. **Principle VI (testable without hardware)** requires that the protocol and state-machine logic
   be reachable without touching an FFI binding. Putting `core`, `rtpmidi` and `blemidi` in crates
   that cannot even depend on `platform` makes that a compile-time guarantee rather than a
   convention.
3. **Principle IV (platform parity)** requires `cfg(target_os)` to stay out of core logic.
   Confining it to one crate makes violations visible in review.

The split is deliberately by *dependency capability* — what a crate is permitted to touch — not by
architectural layer for its own sake, consistent with the code style rule in `AGENTS.md` that
favours simple structure over abstraction.

## Implementation Sequencing

Ordered so each stage is independently valuable and the riskiest work is de-risked early.

| Stage (tasks.md phase) | Content | Gates on |
|---|---|---|
| **0. Spikes** (Phase 1) | Direct CoreMIDI/ALSA virtual port + notifications (R-003); `mdns-sd` coexistence with `mDNSResponder`/`avahi-daemon` (R-005); BLE peripheral viability on macOS (R-006) | Resolve RISK-2, RISK-3 before committing to dependencies |
| **1. Skeleton** (Phase 2) | Workspace, `core` domain types, `ConnectionState` machine, config round-trip, IPC contract, `clap` surface, fake platform | US2 |
| **2. Virtual ports** (Phases 3–4) | `platform/midi` for both OSes, daemon supervisor, service install | US1, US2 — **first shippable slice** |
| **3. Physical devices** (Phase 6) | Device enumeration, fingerprinting, hot-plug, routing engine | US4 (local half) |
| **4. Network MIDI** (Phase 5) | Session control, clock sync, mDNS, then the recovery journal | US3 — largest stage |
| **5. Bluetooth** (Phase 8) | Central first, then peripheral subject to the R-006 spike | US6 |
| **6. GUI** (Phase 9) | libcosmic views over the existing IPC contract | All |

Observability (US5) is not a stage — per Principle VII it is built into each stage as it lands,
because a stage without it cannot be validated.

Stage 4 is sequenced with session control before the journal so that interoperability with Apple's
Network MIDI can be proven early, while the journal — the single largest and most intricate piece —
is developed against a working session with loss injection.

## Complexity Tracking

> No constitutional violations require justification.

The one anticipated violation — BLE peripheral support shipping Linux-first against Principle IV's
parity requirement — **did not materialise**. Stage 0 spike T008 proved the macOS peripheral role
works, so this table stays empty and the Platform Support Matrix needs no declared limitation.

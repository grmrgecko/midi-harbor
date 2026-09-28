---

description: "Task list for Midi Harbor — Service and Clients"
---

# Tasks: Midi Harbor — Service and Clients

**Input**: Design documents from `/specs/001-service-and-clients/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests**: Test tasks ARE included. Constitution Principle VI ("Testable Without Hardware") makes
them mandatory, not optional: every resilience behaviour claimed in Principle I must have a test
that induces the failure and asserts recovery.

**Organization**: First written for all of Midi Harbor, grouped by user story. Since the split on
2026-09-27 each capability's tasks are in its own spec under the phase they were done in; this list
keeps the service and command-line tasks, and the build order below for the whole project.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: Which user story this task belongs to (US1–US6)
- Exact file paths are given in every task

## Path Conventions

Cargo workspace per [plan.md](./plan.md) §Project Structure. Library crates under `crates/`,
binary entry at `src/main.rs`, cross-crate tests under `tests/`, fuzz targets under `fuzz/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Workspace, toolchain, and the Stage 0 spikes that retire the two highest risks before
any dependency is committed to.

- [x] T001 Create cargo workspace root `Cargo.toml` with members `crates/{core,rtpmidi,blemidi,platform,ipc,service,daemon,cli,gui}` and `src/main.rs` binary target, edition 2024, rust-version 1.96
- [x] T002 Add `rust-toolchain.toml` pinning stable 1.96 with `rustfmt` and `clippy` components
- [x] T003 [P] Configure `rustfmt.toml` and workspace `[lints]` denying `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `clippy::indexing_slicing` for library crates per AGENTS.md
- [x] T004 [P] Add CI workflow in `.github/workflows/ci.yml` running fmt, clippy, test and `cargo build --no-default-features` on both macOS and Linux runners
- [x] T005 [P] Define the `gui` feature in the root `Cargo.toml` as the only gate on `libcosmic`, with target-conditional features (macOS: `default-features = false, features = ["winit","wgpu","tokio","multi-window"]`; Linux: defaults plus `wayland`,`x11`) per research R-001
- [x] T006 [P] Add CI assertion in `.github/workflows/ci.yml` that `cargo tree --no-default-features` contains no `cosmic` entry, enforcing FR-039b / SC-014a

### Stage 0 spikes (retire RISK-2 and RISK-3 before committing dependencies)

- [x] T009 [P] Spike in `spikes/platform-midi/` proving direct `coremidi` 0.9 virtual endpoint creation with `kMIDIPropertyUniqueID` retrieval and `MIDINotifyProc` device notifications, and the `alsa` 0.12 sequencer equivalent — validates the R-003 decision to skip `midir`
- [x] T010 Record spike outcomes in `specs/001-midi-connectivity-manager/research.md`, updating R-003, R-005, R-006 status from ASSUMED to VERIFIED or switching to the documented fallback; if the BLE peripheral spike fails, add the Complexity Tracking row in `plan.md` and the declared limitation in `.specify/memory/constitution.md`

**Checkpoint**: Dependencies are proven, not assumed. Fallbacks are chosen explicitly.

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Domain types, the state machine every endpoint shares, config, and the platform fake.
Nothing below depends on hardware.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

### Core domain types

- [x] T011 [P] Implement `EndpointId` (opaque UUID, never reused, stable across rename per FR-004), `RouteId`, `PeerId`, `EventId` newtypes in `crates/core/src/ids.rs`
- [x] T012 [P] Implement `DeviceFingerprint` (fields `unique_id`, `usb_serial`, `manufacturer`, `model`, `name`, `topology_path`) and `MatchConfidence` (`Exact` | `Probable` | `Ambiguous`) in `crates/core/src/fingerprint.rs` per data-model §1
- [x] T013 [P] Implement `FailureReason` as a closed enum with variants `NetworkUnreachable`, `PeerTimeout`, `PeerRejected`, `DeviceRemoved`, `DeviceClaimed`, `PermissionDenied`, `AdapterUnavailable`, `NameConflict`, `ResourceLimit`, `ProtocolError`, `ConfigInvalid` in `crates/core/src/failure.rs` — closed so FR-028 is type-enforced
- [x] T014 [P] Implement `TrafficCounters` as atomics (`messages_sent`, `messages_received`, `bytes_sent`, `bytes_received`, `messages_lost`, `messages_recovered`, `messages_dropped`, `last_activity`) in `crates/core/src/counters.rs`, keeping `messages_lost` (network) distinct from `messages_dropped` (our backpressure) per data-model §7
- [x] T015 [P] Implement `Capability` and `CapabilityName` (`VirtualPorts`, `NetworkSessions`, `BluetoothCentral`, `BluetoothPeripheral`, `ServiceManager`, `MdnsResponder`) with `UnavailableReason` in `crates/core/src/capability.rs` per FR-053
- [x] T016 Implement `Endpoint` and `EndpointKind` (`VirtualPort` | `NetworkSession` | `PhysicalDevice` | `BluetoothDevice`) plus `Direction` in `crates/core/src/endpoint.rs`, validating `name` as 1–128 characters, no control characters, trimmed, unique within kind (FR-005)

### Events and observability

- [x] T028 [P] Initialise `tracing` subscriber configuration in `crates/core/src/logging.rs` following AGENTS.md levels — debug for operational detail, info for lifecycle, error for failures, no exclamation marks — *built in `src/main.rs` (`init_logging`) rather than a core module, since only the binary installs a subscriber*

### IPC contract

- [x] T029 Define the `Harbor` service and every message in `proto/midiharbor/v1/harbor.proto` per contracts/ipc-protocol.md §3, and wire `tonic-prost-build` into `crates/ipc/build.rs`
- [x] T030 Define the query, mutation and streaming RPCs in `proto/midiharbor/v1/harbor.proto` covering virtual ports, network sessions, physical devices, Bluetooth, routes, configuration and diagnostics
- [x] T031 Implement the `FailureReason` to gRPC status mapping in `crates/ipc/src/status.rs`, attaching the stable slug as the `harbor-reason` trailing metadata entry so clients never switch on message text (contracts/ipc-protocol.md §5)
- [x] T032 Implement the Unix-socket transport helpers in `crates/ipc/src/transport.rs` — a server binding a `UnixListener` at mode 0600 under a 0700 directory, and a client connecting through `connect_with_connector` (research R-009)
- [x] T033 Implement the `GetServerInfo` version check in `crates/ipc/src/version.rs`: a major mismatch, or `UNIMPLEMENTED` on the service, must name both versions and say which component to update, never a generic connection error (FR-041)
- [x] T034 [P] Tests in `crates/ipc/tests/protocol.rs` covering an end-to-end unary call and server stream over a real Unix socket, socket permissions, forward compatibility with unknown proto fields, major-version refusal, and the `FailureReason` to status mapping being total

### Platform seam and fake

- [x] T035 Define the `MidiPlatform` trait in `crates/platform/src/midi/mod.rs` — create/destroy virtual endpoints, enumerate physical devices, subscribe to arrival/removal notifications, open/close data paths, read unique identity
- [x] T038 [P] Define the `ServiceManager` trait (install, uninstall, start, stop, status, stale detection) in `crates/service/src/lib.rs`
- [x] T039 Implement the in-memory fake for all four seams in `crates/platform/src/fake.rs` with programmable failure injection — device arrival/removal, permission denial, claimed devices, sleep/wake — satisfying Principle VI

### Real-time data plane

- [x] T041 Define the fixed-size `Copy` MIDI event type crossing the real-time boundary, plus the pre-allocated SysEx pool with handle references, in `crates/core/src/rtevent.rs` per AGENTS.md §Real-time discipline
- [x] T042 Implement `rtrb` ring-buffer wrappers sized at link setup with documented capacity and counted overflow drop in `crates/daemon/src/dataplane.rs` — no allocation, locks, I/O or `tracing` on this path (Principle III)
- [x] T043 [P] Test in `crates/daemon/tests/dataplane.rs` asserting the hot path performs zero allocations, using a counting global allocator

**Checkpoint**: Domain, state machine, config, IPC and the platform fake are complete and tested
with no hardware. User story work can begin.

## Phase 3: User Story 2 — One binary: headless service, CLI, optional GUI (Priority: P1) 🎯 MVP

**Goal**: A single executable that runs as a service, installs itself at login, and exposes every
capability through the command line — the delivery vehicle every other story needs.

**Independent Test**: On a machine with no graphical environment, install the service from the
command line, reboot, and verify from the command line that it came back — without the GUI ever
being built or run (quickstart scenario 6).

### Tests for User Story 2

- [x] T044 [P] [US2] Integration test in `tests/integration/service_lifecycle.rs` asserting install → status → stop → start → uninstall, that re-running install updates in place rather than duplicating, and that uninstall leaves configuration intact (FR-039h) — done as `tests/service_lifecycle.rs`, beside the other binary tests: the real binary runs with `PATH` holding only a stand-in `launchctl` or `systemctl` that keeps state in files and starts the daemon, so nothing is registered with the machine; passes on macOS and Linux, and fails when uninstall touches the configuration or status misreads a running service
- [x] T045 [P] [US2] Test in `tests/integration/headless_parity.rs` asserting every IPC request kind reachable from the GUI is reachable from the CLI (FR-039c, SC-014b) — *at the repository root, `tests/headless_parity.rs`; each RPC takes its own request type, so the request types each crate names are the calls it can make*
- [x] T046 [P] [US2] Test in `tests/integration/daemon_independence.rs` asserting no endpoint state change occurs across 50 simulated client connect/disconnect cycles (FR-037, SC-014, quickstart scenario 7) — *in `crates/daemon/tests/clients.rs`, against a real server on a Unix socket; clients also abandon each kind of stream mid-flight*

### Implementation for User Story 2

- [x] T047 [US2] Implement argument dispatch in `src/main.rs` selecting daemon, gui, cli or service role — bare invocation launches the GUI on a full build and prints help on a headless build (FR-039, FR-039e)
- [x] T048 [US2] Implement the `clap` command tree in `crates/cli/src/commands.rs` matching contracts/cli-interface.md §2–6 exactly, with global flags `--json`, `--socket`, `-v/-vv`, `--quiet`, `--no-color`
- [x] T049 [US2] Implement the gRPC server in `crates/daemon/src/server.rs` on a Unix socket at mode 0600 under a 0700 directory, serving many concurrent clients over HTTP/2 with no client blocking another (FR-040)
- [x] T050 [US2] Implement the client connection in `crates/cli/src/client.rs` calling `GetServerInfo` first and reporting a major-version mismatch by naming both versions and which side to update (FR-041)
- [x] T051 [US2] Implement the streaming RPCs in `crates/daemon/src/server.rs`: `WatchState`, `WatchEvents` and `WatchInvitations` lossless with `RESOURCE_EXHAUSTED` for a client that falls behind, and `WatchTraffic` and `MonitorEndpoint` lossy with reported drop counts, overriding HTTP/2 flow control so a stalled client cannot reach the data path (contracts/ipc-protocol.md §4, Principle III) — *built in `crates/daemon/src/service.rs`; each stream runs on its own task behind a bounded channel, so a stalled client blocks only that task and never the real-time rings, which achieves the isolation without tuning HTTP/2 windows*
- [x] T052 [US2] Implement `midi-harbor daemon` foreground mode in `crates/daemon/src/lib.rs`, registering nothing with the OS so it can be externally supervised or debugged (FR-039a)
- [x] T053 [P] [US2] Implement the launchd user agent backend in `crates/service/src/launchd.rs` writing `~/Library/LaunchAgents/com.mrgeckosmedia.MidiHarbor.daemon.plist` with `RunAtLoad` and `KeepAlive` (FR-039g, research R-007)
- [x] T054 [P] [US2] Implement the systemd user unit backend in `crates/service/src/systemd.rs` writing `~/.config/systemd/user/midi-harbor.service` with `Restart=always` and `WantedBy=default.target`, enabled via `systemctl --user enable --now`
- [x] T055 [US2] Implement stale-registration detection in `crates/service/src/lib.rs` reporting a registration pointing at a moved or deleted executable and naming the missing path (edge case)
- [x] T056 [US2] Implement the unsupported-service-manager path in `crates/service/src/lib.rs` telling the user to run `midi-harbor daemon` under their own supervisor (edge case, research R-007)
- [x] T057 [US2] Implement `service install|uninstall|start|stop|status` commands in `crates/cli/src/service_cmd.rs` per contracts/cli-interface.md §2, never requiring elevation (FR-039f, FR-043)
- [x] T058 [US2] Implement the daemon-not-running path in `crates/cli/src/client.rs` exiting 3 with the offer to run `midi-harbor service install --start` or `midi-harbor daemon` (FR-042)
- [x] T059 [US2] Implement `--json` output in `crates/cli/src/output.rs` as a direct projection of the generated proto types, with diagnostics on stderr so piping to `jq` is always safe (FR-039d)
- [x] T060 [US2] Implement the exit-code mapping (0–8) in `crates/cli/src/exit.rs` derived from IPC `error.code` per contracts/cli-interface.md §7
- [x] T063 [US2] Create the macOS `.app` bundle build with `NSBluetoothAlwaysUsageDescription` in `Info.plist` in `packaging/macos/`, and register the bundled executable rather than a loose binary (research R-006) — `packaging/macos/build.sh` builds and signs the bundle; `service install` run from it registers the bundled binary, and launchd's daemon from it had both Bluetooth roles usable across a reboot (R-059); a refused permission is now told apart from a radio switched off (R-006)

**Checkpoint**: The executable installs itself, runs headless, and is fully driveable from the
command line.

## Phase 10: Polish & Cross-Cutting Concerns

- [x] T146 [P] Build the latency measurement harness in `tests/soak/latency.rs` verifying SC-008 (virtual port under 1 ms mean, 3 ms p99), SC-009 (network under 5 ms p99) and SC-010b (repeater under 10 ms p99) — *at the repository root, `tests/latency.rs`, covering SC-008 across a virtual port on the real backend of whichever machine runs it; SC-009 and SC-010b need a peer and are not covered. It found the three delays in R-048*
- [x] T147 [P] Build the resource measurement harness in `tests/soak/resources.rs` verifying SC-010 (under 1% of one core idle with 10 endpoints, under 150 MB memory) — *measured by hand rather than as a test, since process CPU time needs a platform call the workspace does not otherwise use; recorded in R-048 for both platforms*
- [X] T151 [P] Write user documentation in `docs/` covering installation, the CLI reference from contracts/cli-interface.md, and the configuration file format (FR-049) — written from what the binary does rather than from the contract, each command run against a scratch daemon; doing so found nine defects, fixed and recorded in R-052
- [X] T152 [P] Add packaging in `packaging/` — macOS `.app` and `.dmg`, Linux `.deb`, `.rpm` and a headless variant built with `--no-default-features` — all built and inspected, none installed; the full `.deb` needs a Debian machine with the GUI build dependencies; runtime-loaded GUI libraries recommended by name; R-054
- [X] T153 Verify platform parity per Principle IV: every feature present on one OS is present on the other, or declared in the constitution's Platform Support Matrix (SC-016) — audited by code, by diffing both test suites, and by building and running the GUI on Linux for the first time; ALSA backend tests added to match CoreMIDI's, a false capability claim fixed, and platform differences documented in `docs/platforms.md`; R-053
- [x] T154 Run the full quickstart.md validation suite on both macOS and Linux with real hardware and a real peer, recording results — this is the **only** coverage for SC-001 (under 30 s to a working port) and SC-014c (one install command plus one create command), which are deliberately manual acceptance criteria rather than automated tests — done: scenarios 2 (discovery, peer restart, network cut, 5% loss, a roam between two access points that keeps the address, R-069, and sleep and wake, R-070), an Apple peer each way (R-068), 7 and 8 run between this Mac and the Linux desktop, finding six defects (R-055 to R-057); scenario 1 passed on macOS across a real reboot (R-059); scenario 5 run Mac to Linux with no device (R-058), then with MIDI both ways over one link, Linux as central still refused (R-064); scenarios 3, 4 and 6 run on 2026-09-25 (R-081): the repeater Linux to Mac with the pad controller, a held pad silenced across the network and the device back 14 s after a replug into another USB port; the history, exit 6 and the report; the headless build restored after a reboot of the Arch VM. Replacement under launchd checked the same day (R-079).

## Phase 11: Convergence

- [x] T157 CRITICAL: Remove the `unsafe` `getuid` call from `crates/service/src/launchd.rs`, taking the uid without `unsafe` or from the platform crate with a `// SAFETY:` comment at the call site, per Constitution: unsafe code (contradicts) — done: `rustix::process::getuid`, checked against `id -u`
- [x] T166 Emit one JSON document for every CLI command under `--json`, including port, route, session, peer, Bluetooth and service commands, per FR-039d, US2/AC7 (partial) — done: sentences are held under `--json` and every invocation closes with one document, with a `result` for what was made or renamed; usage errors caught by the argument parser still write only to stderr
- [x] T167 Show the running daemon's version and uptime in `service status`, from `GetServerInfo`, per US2/AC2 (partial) — done: "daemon: version 0.1.0, up 4s", and `daemon_version`, `started_at` and `uptime_seconds` in JSON
- [x] T172 Offer and document enable and disable for every endpoint kind in the CLI, matching the GUI's toggle, per SC-014b, FR-039c (partial) — done: `session`, `device` and `bluetooth` each gain `enable` and `disable`, documented beside `port`'s

## Dependencies

### Phase order

```
Phase 1 (Setup + Spikes)
    ↓
Phase 2 (Foundational)  ⚠️ BLOCKS ALL USER STORIES
    ↓
Phase 3 (US2 — binary, CLI, service)  ──┐
    ↓                                    │  together form the MVP
Phase 4 (US1 — virtual ports)  ──────────┘
    ↓
Phase 5 (US3 — network MIDI)
    ↓
Phase 6 (US4 — routing + repeater)
    ↓
Phase 7 (US5 — diagnostics)      ← may start once US1 lands
    ↓
Phase 8 (US6 — Bluetooth)        ← gated on spike T008
    ↓
Phase 9 (GUI)                    ← needs only the IPC contract, could start after Phase 3
    ↓
Phase 10 (Polish)
```

### Story dependencies

- **US2** depends only on Phase 2. It is the delivery vehicle for everything else.
- **US1** depends on US2 for its reboot-persistence scenario (FR-003) — the service must exist.
- **US3** depends on Phase 2 and the socket layer from US2. Independent of US1.
- **US4** depends on US1 (endpoints to route between) and benefits from US3 (the repeater target).
- **US5** is genuinely independent once any endpoint exists; per Principle VII it is built
  incrementally into each earlier phase rather than deferred wholesale.
- **US6** depends on Phase 2 and spike T008. Independent of US3 and US4.
- **Phase 9 (GUI)** depends only on the IPC contract from Phase 3, so it can be developed in
  parallel with Phases 5–8 by a separate person.

### Critical path

T001 → T007/T008/T009 → T010 → T011–T043 → T047–T063 → T067–T076 → T083–T103 → T107–T119

The recovery journal (T086–T093) is the longest single stretch and should start as early in
Phase 5 as the session layer allows.

---

## Parallel Execution Examples

**Phase 1 — all three spikes at once** (different directories, no shared code):

```
T007 (mDNS coexistence)  ‖  T008 (BLE peripheral)  ‖  T009 (platform MIDI)
```

**Phase 2 — domain types** (each a separate file):

```
T011 (ids)  ‖  T012 (fingerprint)  ‖  T013 (failure)  ‖  T014 (counters)  ‖  T015 (capability)
```

**Phase 3 — the two service backends** (different files, different platforms):

```
T053 (launchd)  ‖  T054 (systemd)
```

**Phase 5 — journal chapters** (T087 first, then the rest in parallel):

```
T087 (Chapter N — do first, it prevents stuck notes)
    ↓
T088 (Chapter C)  ‖  T089 (Chapter P)  ‖  T090 (Chapter W)  ‖  T091 (Chapter T)
```

**Phase 8 — peripheral backends**:

```
T133 (macOS peripheral)  ‖  T134 (Linux peripheral)
```

**Phase 9 — GUI views** (each a separate file over a settled contract):

```
T139  ‖  T140  ‖  T141  ‖  T142  ‖  T143
```

---

## Implementation Strategy

### MVP scope

**Phase 1 + Phase 2 + Phase 3 (US2) + Phase 4 (US1)** — tasks T001–T076.

This delivers a single self-installing binary that creates named virtual MIDI ports which persist
across reboot and are fully driveable from the command line, on both macOS and Linux, with no GUI.
That is already a usable replacement for the platform's built-in inter-application MIDI, and it is
the foundation everything else builds on.

### Incremental delivery

1. **MVP** (T001–T076) — virtual ports, service, CLI. Ship it.
2. **+ US3** (T077–T103) — network MIDI with the recovery journal. The headline differentiator.
3. **+ US4** (T104–T119) — physical hardware and the repeater.
4. **+ US5** (T120–T127) — diagnostics made explicit. Much of this lands earlier per Principle VII.
5. **+ US6** (T128–T137) — Bluetooth.
6. **+ GUI** (T138–T145) — can be developed in parallel from step 2 onward.
7. **+ Polish** (T146–T154).

### Sequencing rationale

The spikes come first because RISK-2 (BLE peripheral) and RISK-3 (mDNS coexistence) could each
force a dependency change, and finding that out after building on the wrong assumption is the
expensive outcome.

Within Phase 5, session control and clock sync land before the journal so interoperability with
Apple's Network MIDI can be proven against a working session early, while the journal — the
largest and most intricate piece in the project — is developed against loss injection rather than
in isolation.

Chapter N is singled out ahead of the other journal chapters because it is the one that prevents
stuck notes, which is the single most user-visible resilience failure the product exists to
eliminate.

## After the split

- [x] T241 Say in the install guide and the README how to open the macOS app, which is signed ad hoc and not notarized, past Gatekeeper, and the unsigned Windows executable past SmartScreen, and list the xkbcommon and Wayland development files a Linux build of the graphical interface needs, which the sysroot installs and the build instructions left out (found 2026-09-27 preparing the first release; a Debian build of the window failed without xkbcommon) — done

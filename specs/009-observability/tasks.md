# Tasks: Observability

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 2: Foundational (Blocking Prerequisites)

### Events and observability

- [x] T027 [P] Implement `Event`, `Severity`, `EventKind` (stable machine-readable names) and a bounded in-memory ring of 10,000 entries in `crates/core/src/events.rs` per FR-046 and Principle VII

## Phase 7: User Story 5 — Connection health and diagnostics (Priority: P4)

**Goal**: Diagnosing a dropped connection without a log file or a restart.

**Independent Test**: Induce a failure, close every client, reopen, and confirm the failure and its
recovery are both visible with timestamps (quickstart scenario 4).

### Tests for User Story 5

- [x] T120 [P] [US5] Test in `tests/integration/event_history.rs` asserting failure and recovery events survive all clients disconnecting and reconnecting (FR-046, SC-013) — *in `crates/daemon/tests/clients.rs`; writing it showed that a failure to open was never recorded, only the recovery that followed it*
- [x] T121 [P] [US5] Test in `crates/core/tests/failure_reason.rs` asserting `FailureReason` is exhaustively mapped to actionable messages and CLI exit codes, so no case falls through to a generic failure (FR-028, SC-012) — *the exhaustive exit mapping and its test live in `crates/cli/src/exit.rs`, over `FailureReason::one_of_each()`, whose match stops compiling when a variant is added; guidance coverage is tested in `crates/core/src/failure.rs`*

### Implementation for User Story 5

- [x] T122 [US5] Implement live state exposure — phase, time in phase, last error, next retry attempt — in `crates/daemon/src/handlers/status.rs` (FR-044)
- [x] T123 [US5] Implement coalesced `CountersUpdated` events at most every 250 ms in `crates/daemon/src/server.rs`, never per-message (contracts/ipc-protocol.md §5)
- [x] T124 [US5] Implement the `monitor:<endpoint>` topic decoding MIDI to human-readable form in `crates/daemon/src/monitor.rs`, lossy under load with reported drop counts (FR-047)
- [x] T125 [US5] Implement `ExportDiagnostics` collecting configuration, connection history and counters in `crates/daemon/src/handlers/diagnostics.rs` (FR-048)
- [x] T126 [P] [US5] Implement `status [--watch]`, `monitor [--raw]`, `events [--follow]` and `diagnostics export` commands in `crates/cli/src/status_cmd.rs` (contracts/cli-interface.md §5)

**Checkpoint**: Every resilience claim made by the product is now visible and verifiable by users.

## Phase 11: Convergence

- [x] T163 Record an endpoint state change in the history for every Bluetooth connect, loss and failure and for hardware re-opening, per FR-046, US5/AC2, SC-013 (partial) — done: Bluetooth connects, losses, returns, first failures and the last central leaving, and hardware plugged back in, each against its endpoint
- [x] T164 Show when the next reconnection attempt is due for a retrying endpoint in the GUI and in CLI `status`, per FR-044, US5/AC3 (partial) — done: the window's detail line and `status` say "next in 4s", and `status --json` gives `next_retry`
- [x] T165 Make `WatchTraffic` and `MonitorEndpoint` never await capacity: use `try_send`, count updates dropped for each subscriber, and report that count in `dropped`, per AGENTS: lossy streams (contradicts) — done: both offer with `try_send` and count what a full channel refuses; no new test, since a waiting task's lagging broadcast counted drops too, and the client-churn tests cover streams left unread
- [x] T178 Include the full running configuration, preferences and per-kind settings included, in the diagnostic report, per FR-048 (partial) — done: the report carries the configuration as the file holds it, preferences, peers and every endpoint's settings

## Phase 13: Surviving the MIDI server

- [x] T194 Keep a device's traffic counters while it is unplugged and continue them when it returns, per FR-045 — done: the counts are held with the unplugged device and read from there while it is away; on hardware the pad controller went from 15 to 17 across a replug, matching its route

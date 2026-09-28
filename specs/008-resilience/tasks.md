# Tasks: Resilience

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 2: Foundational (Blocking Prerequisites)

### Connection state machine (the heart of Principle I)

- [x] T017 Implement `ConnectionPhase` (`Disabled`, `Disconnected`, `Connecting`, `Connected`, `Retrying`, `Unavailable`) and `ConnectionState` with `since`, `last_error`, `attempt`, `next_retry` in `crates/core/src/state.rs` per data-model §6
- [x] T018 Implement the transition function in `crates/core/src/state.rs` enforcing: `Unavailable` is re-evaluated and never terminal; backoff never gives up while enabled (FR-023); every exit from `Connected` signals note-silencing (FR-026); every re-entry signals state restoration (FR-027)
- [x] T019 [P] Implement exponential backoff in `crates/core/src/backoff.rs` with base delay 250 ms, factor 2.0, maximum delay 30 s, and full jitter (uniform over `[0, computed]`), taking an injected time source so no test sleeps (FR-023, Principle VI)
- [x] T020 [P] Unit tests in `crates/core/src/state.rs` asserting every transition, that no input drives an enabled endpoint to a terminal state, and that `Unavailable` re-evaluates — covers Principle I
- [x] T020a [P] Test in `crates/core/tests/isolation.rs` asserting that driving one endpoint through `Connecting` → failure → `Retrying` produces zero state transitions on every other endpoint — covers FR-029, which no other task verifies
- [x] T021 [P] Property test in `crates/core/tests/backoff.rs` asserting backoff is monotonic up to the cap, always jittered, and never returns zero or unbounded delay

### Platform seam and fake

- [x] T036 [P] Define the `SystemEvents` trait (sleep, wake, network change) in `crates/platform/src/sysevents/mod.rs` per research R-010

## Phase 4: User Story 1 — Persistent virtual MIDI ports (Priority: P1) 🎯 MVP

### Implementation for User Story 1

- [x] T071 [US1] Implement the per-endpoint supervisor loop in `crates/daemon/src/supervisor.rs` driving `core::state`, reconciling desired configuration against actual platform state on startup and on change (FR-022, FR-036, FR-038) — *startup and change reconciliation live in `crates/daemon/src/state.rs`; `supervisor.rs` delivers the retries the state machine schedules for virtual ports and attached hardware, which were computed and shown but never acted on. One loop serves every such endpoint, so per-endpoint isolation remains T071a*
- [X] T071a [US1] Isolate each supervisor in `crates/daemon/src/supervisor.rs` onto its own task with its own error boundary, so a failing or retrying endpoint cannot stall or fault any other (FR-029) — each retry now runs on its own task, opens off the lock and off the runtime's workers, and a result that arrives after the endpoint changed is released; `crates/daemon/tests/isolation.rs`, R-050

## Phase 5: User Story 3 — Self-healing network MIDI (Priority: P2)

### Implementation for User Story 3

- [X] T097 [US3] Implement the macOS `SystemEvents` backend in `crates/platform/src/sysevents/macos.rs` using IOKit `IORegisterForSystemPower` and network path monitoring (FR-024, FR-025) — IOKit sleep and wake, answering each suspend once held notes are released; network changes stay with the polled address watch, which finds them within a second; registration verified, a real sleep not yet; R-051
- [X] T098 [US3] Implement the Linux `SystemEvents` backend in `crates/platform/src/sysevents/linux.rs` using logind `PrepareForSleep` over D-Bus and netlink `RTMGRP_IPV4_IFADDR`/`RTMGRP_IPV6_IFADDR` — logind sleep and wake with a delay lock held until held notes are released; network changes stay with the polled address watch, as on macOS; verified on Debian through a forged signal, a real suspend not yet; R-051
- [x] T099 [US3] Implement the monotonic-versus-wall-clock gap detector in `crates/platform/src/sysevents.rs` as a platform-independent suspend fallback, because platform events are unreliable (research R-010)

## Phase 7: User Story 5 — Connection health and diagnostics (Priority: P4)

### Implementation for User Story 5

- [x] T127 [US5] Implement rapid connect/disconnect flap damping in `crates/daemon/src/supervisor.rs`, surfacing the link as unstable rather than consuming resources (edge case) — *in `crates/core/src/state.rs` rather than the supervisor, so every endpoint kind shares it; Bluetooth also needed its sightings held to the backoff, see R-044*

## Phase 10: Polish & Cross-Cutting Concerns

- [x] T148 [P] Implement the 24-hour soak test in `tests/soak/endurance.rs` asserting zero unrecovered disconnections (SC-007) — done as `crates/daemon/tests/endurance.rs`, ignored by default and run with `HARBOR_SOAK_SECS`; a full day on the Linux desktop recovered all 24278 faults, slowest 2.56 s, 11.5 million messages played (R-067)

## Phase 11: Convergence

- [x] T155 CRITICAL: Keep the last controller, program, pitch-bend and pressure values per channel for each outbound session and Bluetooth link, and send them to the peer when `Effect::RestoreState` fires on recovery, instead of ignoring it in `crates/daemon/src/session.rs` per FR-027, Constitution I (missing) — done: sessions resend what was routed into them, outage changes included, and Bluetooth links what they were last sent (R-074)

## Phase 13: Surviving the MIDI server

Found on real hardware (R-079): when Apple's `MIDIServer` dies, the daemon goes deaf for good,
and only a new process recovers.

- [x] T192 Recover from `MIDIServer` restarting on macOS: probe CoreMIDI from the backend's thread every few seconds with a private output port that no other application sees, report a lost server as a platform event, record it, and replace the daemon with a new process running the same executable and arguments, which restores ports with their pinned identifiers, hardware and routes from the configuration, per FR-032, Constitution Principle I. Test the probe and the restart path against the fake, then on a Mac by killing `MIDIServer`, and finish with a USB pad controller: unplug and plug in with a route carrying its pads, on macOS and on Linux. — done: a private output port is created and dropped every 2 s, and a failure replaces the daemon through `exec` with a warning event; checked by killing `MIDIServer` under the pad controller (noticed in 2.8 s, serving 0.4 s later, same port identifiers, pads through the route before and after a replug), and on Linux, where the kernel sequencer has no server to lose, with seven replugs; stopping with clients connected now ends in 2 s; one mutant survives, the `return` after asking for the restart, which has no observable effect because nothing after it runs before the daemon stops
- [x] T195 Silence held notes with a note-off each, and leave a channel's pedal and broad resets alone when another route still sounds notes on it at the same destination and MIDI Out, per FR-015f, FR-026 — done: sustain-off, a note-off per held note, then the two resets on a channel nobody else is playing; only note-offs on a shared one; checked on hardware with a pad held through an unplug

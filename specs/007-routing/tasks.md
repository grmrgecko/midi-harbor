# Tasks: Routing

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 6: User Story 4 — Routing and repeating between endpoints (Priority: P3)

**Goal**: Physical MIDI hardware as a first-class endpoint, and routes carrying any endpoint to any
other — including a physical device repeated over the network or Bluetooth.

**Independent Test**: Plug in a physical device, route it to another endpoint, play it, verify
arrival; unplug and replug and verify the route resumes on its own (quickstart scenario 3).

### Tests for User Story 4

- [x] T105 [P] [US4] Tests in `crates/core/tests/router.rs` covering one-to-many and many-to-one delivery, cycle detection, broken-route reporting, and delivery continuing to reachable destinations when others are unavailable (FR-030a–b, FR-033, FR-035)
- [x] T106 [P] [US4] Test in `tests/integration/repeater.rs` asserting a physical device routed to a network session survives unplug and replug with no stuck notes on the remote side (SC-010c) — *in `crates/daemon/tests/repeater.rs`, two daemons joined by a real session over loopback; it found that silencing never reached a network session, and running the same thing across two machines found R-046*

### Implementation for User Story 4

- [x] T111a [US4] Implement route suspension and resumption for every endpoint kind in `crates/core/src/router.rs` and `Daemon::router`, keeping a route's configuration intact while either endpoint is unavailable and resuming delivery when it returns (FR-032)
- [x] T112 [US4] Implement the route graph with validation — no self-route, source has an input side, destination has an output side, no duplicate pairs — in `crates/core/src/router.rs` (FR-015b, FR-030, FR-030b, data-model §5)
- [x] T113 [US4] Implement cycle detection on every route mutation in `crates/core/src/router.rs`, marking affected routes `LoopDetected` and warning without blocking creation (FR-033)
- [X] T114 [US4] Implement `OriginTag` per-message loop suppression and the network session identifier in `crates/core/src/router.rs` and `crates/rtpmidi/src/session.rs`, so cross-machine repeater loops are caught (research R-013, RISK-9) — caught without a wire marker, at the session-to-session route where every such loop closes, by recognising MIDI a session sent moments ago coming back; the route is switched off and the history says why; `crates/core/src/loops.rs`, R-062
- [x] T115 [US4] Implement the routing hot path in `crates/daemon/src/dataplane.rs` fanning messages from source ring buffers to destination ring buffers with no allocation, locks or logging (Principle III)
- [x] T117 [US4] Implement route request handlers (`CreateRoute`, `DeleteRoute`, `SetRouteEnabled`) and physical device handlers (`ListPhysicalDevices`, `ForgetPhysicalDevice`, `ResolveAmbiguousDevice`) in `crates/daemon/src/handlers/routes.rs` (FR-031, FR-034)
- [x] T118 [P] [US4] Implement `route list|create|delete|enable|disable` commands in `crates/cli/src/route_cmd.rs` (contracts/cli-interface.md §4)

**Checkpoint**: The repeater works — hardware in one room drives hardware in another.

## Phase 11: Convergence

- [x] T160 Make stored routes identify their endpoints unambiguously across kinds that share a name, and use that in `Router::build`, duplicate checks and rename rewriting, per FR-030, FR-004 (partial) — done: routes carry an optional kind per end, written when a name is shared or becomes shared; an unqualified shared name reads as broken
- [x] T177 Reword the GUI's loop note so it no longer says messages circulate, since delivery is direct-only, per FR-033 (contradicts) — done: worded as the command-line guide words it

## Phase 12: The redesigned window

- [x] T183 Let one route carry MIDI both ways: a `both_ways` route setting in the configuration and contract, the router delivering in both directions with loop detection covering both, an RPC to change a route's ends and `both_ways`, `route create --both-ways`, and the window's route dialog, per FR-034a, FR-033 — done: `both_ways` on a route in the configuration and contract (`Route.both_ways` 14, `CreateRouteRequest.both_ways` 5, new `UpdateRoute` RPC); the router delivers back from the destination's MIDI In to the source's MIDI Out of the same numbers under the same route, refuses a route that would carry what a two-way route already does, marks a two-way route broken when a connector of the way back is missing, and treats one as an edge each way for loops without calling A and back a loop; editing a route silences it first and keeps whether it is switched on; silencing a two-way route stops notes at both ends, including when its destination goes away; `route create --both-ways`, `route edit`, and one shared route dialog in the window with a Both ways switch and ↔ on the row. Every new check was mutation-tested.

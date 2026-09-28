# Tasks: Virtual Ports

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 4: User Story 1 — Persistent virtual MIDI ports (Priority: P1) 🎯 MVP

**Goal**: Named virtual MIDI ports that local applications can see and use, surviving reboot.

**Independent Test**: Create a named port, verify two applications exchange MIDI through it,
reboot, and verify the port reappears with its name and identity intact (quickstart scenario 1).

### Tests for User Story 1

- [x] T064 [P] [US1] Integration test in `tests/integration/virtual_ports.rs` over the platform fake covering create, rename with confirmation, delete, enable/disable, and name-conflict rejection (FR-001–FR-007)
- [x] T065 [P] [US1] Test in `tests/integration/persistence.rs` asserting all configured ports are recreated on daemon startup with identical `EndpointId` (FR-003, FR-004, SC-002)
- [x] T066 [P] [US1] Test in `crates/daemon/tests/silencing.rs` asserting sounding notes are silenced before a port is deleted or disabled (FR-006)

### Implementation for User Story 1

- [x] T067 [US1] Implement CoreMIDI virtual endpoint creation in `crates/platform/src/midi/coremidi.rs` using `MIDISourceCreateWithProtocol`/`MIDIDestinationCreateWithProtocol`, reading and persisting `kMIDIPropertyUniqueID` for stable identity (FR-002, FR-004, research R-003)
- [x] T068 [US1] Implement the `MIDINotifyProc` notification callback in `crates/platform/src/midi/coremidi.rs` for device arrival and removal, with `// SAFETY:` comments on every `unsafe` block per AGENTS.md
- [x] T069 [US1] Implement ALSA sequencer virtual port creation in `crates/platform/src/midi/alsa.rs` with client and port enumeration and subscription management, exposing each port as both input and output (FR-002)
- [x] T070 [US1] Implement ALSA sequencer announce-port subscription in `crates/platform/src/midi/alsa.rs` for device arrival and removal notifications
- [x] T072 [US1] Implement `CreateVirtualPort`, `RenameEndpoint`, `DeleteVirtualPort`, `SetEndpointEnabled` request handlers in `crates/daemon/src/handlers/ports.rs`, rejecting duplicate names within a kind with `name_conflict` (FR-005)
- [x] T073 [US1] Implement the rename confirmation gate in `crates/daemon/src/handlers/ports.rs` returning `confirmation_required` when `confirm` is absent (FR-001 scenario 4)
- [x] T074 [US1] Implement note-silencing (all-notes-off and sustain reset per active channel) in `crates/core/src/sounding.rs` and `Daemon::silence_endpoint`, invoked on disable, delete and daemon stop (FR-006, FR-026)
- [x] T075 [US1] Implement `port list|create|rename|delete|enable|disable` commands in `crates/cli/src/port_cmd.rs`, resolving endpoints by name or id and erroring with candidates on ambiguity (contracts/cli-interface.md §3)
- [x] T076 [US1] Implement the OS endpoint-limit path in `crates/platform/src/midi/mod.rs` returning `ResourceLimit` rather than failing silently (edge case) — *ALSA only: it reports the limit as `EINVAL`, told apart by counting the client's ports against `SNDRV_SEQ_MAX_PORTS`; verified by filling a real client. CoreMIDI's limit, if it has one, was not found and is not mapped*

**Checkpoint**: MVP complete. Ports persist across reboot, are driveable from the CLI, and the
service keeps them alive with no GUI.

## Phase 11: Convergence

- [x] T161 Add creating, renaming (with the reselect warning and confirmation) and deleting (with confirmation) virtual ports to the GUI over the existing RPCs, per SC-001, US1 (partial) — done: a New virtual port form, and Rename and Delete on each port's row, each confirmed in place; checked on screen against a scratch daemon

## Phase 12: The redesigned window

- [x] T182 Give virtual ports MIDI In and MIDI Out connector counts, one to sixteen of each, in place of `direction` (an in-only or out-only port read as one of each): configuration, contract (additive), CoreMIDI and ALSA creating numbered ports, routes naming a connector, `port create --inputs --outputs`, an edit RPC that removes routes on connectors that no longer exist, and the window's port dialog, per FR-002, FR-002a, Constitution III — done: `inputs`/`outputs` (1–16) with every connector's identifier kept, an out-of-range count clamped and an old in-only or out-only port read as one of each; one ring and dispatch task per MIDI In, routes naming a connector (`from_connector`/`to_connector`, ids unchanged for first connectors), numbered CoreMIDI endpoints and ALSA ports, `SetVirtualPortConnectors` removing routes on dropped connectors, `port create --inputs --outputs`, `port connectors`, `route create --from-connector --to-connector`, and the window's steppers and per-connector route choices

## Phase 13: Surviving the MIDI server

- [x] T197 Refuse a virtual port and a network port with one name, since other applications see a network port through its automatic port of the same name and could not tell the two apart, per FR-005, FR-015h, R-078 (owner decision, 2026-09-25) — done: virtual ports and network ports now share one namespace, compared as before (trimmed, case-sensitive), so creating or renaming either into the other's name fails with `NameConflict` naming the name, whether or not the network port has its automatic port switched on; renaming a port to its own name still works. Importing or reloading a document that gives the two one name is refused, including a merge that would add one beside the other. A hand-edited file that already holds such a pair is loaded with both kept, and each clash is logged at error and recorded in the history so it can be fixed by renaming one; dropping either would lose part of the setup. Two virtual ports of one name in the file were already kept at startup, without a word; they are now reported the same way. `config import-apple` leaves out a bus named like a network port and a session named like a port and says so, rather than failing partway. Every check was mutation-tested.

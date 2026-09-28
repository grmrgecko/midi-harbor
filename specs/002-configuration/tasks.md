# Tasks: Configuration

Tasks from the task list, under the phase each was done in and by their original numbers. Phases are
the order the project was built in, across every spec; [001's
tasks](../001-service-and-clients/tasks.md) give that order in full.

## Phase 2: Foundational (Blocking Prerequisites)

### Configuration

- [x] T022 Implement the `Configuration` root (`schema_version`, `preferences`, `endpoints`, `peers`, `routes`) as YAML with serde derives in `crates/core/src/config.rs` per data-model §9, flattening the endpoint kind and referencing route endpoints by name so the file is hand-editable
- [x] T023 Implement atomic config write (temp file in the same directory, flushed, then renamed) and schema migration in `crates/core/src/config.rs` using `serde_yaml_ng` per research R-011 — guards FR-051
- [x] T024 Implement corrupt-config recovery in `crates/core/src/config.rs`: preserve the unreadable file with a timestamped rename, start from defaults, surface the problem (FR-051)
- [x] T025 [P] Resolve per-user config and socket paths via `directories::BaseDirs` in `crates/core/src/paths.rs` — config at `~/Library/Application Support/Midi Harbor/` on macOS and `$XDG_CONFIG_HOME/midi-harbor/` on Linux; socket at `$TMPDIR/midi-harbor/` and `$XDG_RUNTIME_DIR/midi-harbor/` respectively (research R-009, R-011)
- [x] T026 [P] Tests covering YAML round-trip, a hand-written file with no identifiers, route resolution and rename rewriting, schema refusal, atomic write leaving no temporaries, and corrupt-file preservation (quickstart scenario 8)

### Platform seam and fake

- [x] T040 [P] Implement `capability::query()` in `crates/platform/src/capability.rs` returning runtime availability per platform, so unavailable is reported as unavailable rather than broken (FR-053, Principle IV)

## Phase 3: User Story 2 — One binary: headless service, CLI, optional GUI (Priority: P1) 🎯 MVP

### Implementation for User Story 2

- [x] T061 [US2] Implement config reload and diffing in `crates/daemon/src/reconcile.rs` so changes disturb only the connections whose configuration actually changed (FR-050) — *built as described; `Applied` names what was added, removed and reopened, and an unreadable file is refused with everything left running rather than set aside as at startup*
- [x] T062 [P] [US2] Implement `config path|show|export|import|reload` commands in `crates/cli/src/config_cmd.rs` (FR-052) — *ticked before `export`, `import` and `reload` existed, when the CLI had only `path` and `show` and the three RPCs were stubs; they were built with T061, in `crates/cli/src/run.rs`*

## Phase 10: Polish & Cross-Cutting Concerns

- [x] T149 [P] Implement config export/import cross-platform portability tests in `tests/integration/portability.rs` (FR-052, SC-015) — *the automated half is `crates/daemon/tests/reconcile.rs` (merge, replace, identity kept, attached hardware kept); the cross-platform half cannot be automated on one machine and was run by hand in R-043: a setup exported from macOS imported on Linux with the same endpoints and routes*
- [X] T150 [P] Implement the one-time Apple configuration import in `crates/daemon/src/migrate.rs` reading existing IAC bus names and Network MIDI sessions to pre-populate config, never writing to Apple's files (research R-002) — as `config import-apple`, an explicit command that previews and asks for `--yes`, per the owner; reads through CoreMIDI in `crates/platform/src/midi/apple_setup.rs` and creates through the daemon's existing requests; R-061

## Phase 11: Convergence

- [x] T173 Move the Apple setup read behind a daemon RPC so the CLI no longer queries CoreMIDI, or record the exception in research.md, per Constitution II (contradicts) — done: the daemon reads the setup behind a new ReadAppleSetup RPC, and `config import-apple` asks it
- [x] T174 Resolve the disagreement between AGENTS.md and plan.md over configuration file I/O in `crates/core/src/config.rs`, moving the I/O into the daemon or recording the decision, per AGENTS: layout (contradicts) — done: the owner chose to amend AGENTS.md, which now allows `core/` its own configuration file I/O, as plan.md places it

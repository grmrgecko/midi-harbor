---

description: "Task list for Windows support"
---

# Tasks: Windows support

**Input**: Design documents from `/specs/013-windows-support/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md)

**Tests**: Mandatory, as in specs 001 to 010 (Constitution Principle VI).

## Format: `[ID] [P?] [Story] Description`

Task IDs continue after the original specification's, so every T number in the project names one task.

---

## Phase 1: Setup

- [x] T201 Cross-compile for `x86_64-pc-windows-gnu` with mingw-w64 from macOS, configured in
  `.cargo/config.toml` — done: the whole workspace builds, the interface included (R-089).
- [x] T202 Move advertising behind `crates/platform/src/responder/` so `zeroconf`, which cannot
  build for Windows, is left out there — done.
- [x] T203 Add run-time DLL loading in `crates/platform/src/dll.rs` for interfaces a machine may
  lack — done.
- [x] T204 Add `scripts/windows-test.sh` to build test binaries here and run them on a Windows
  machine, mirroring the tree for tests that read compiled-in paths — done (R-089).

## Phase 2: Foundational

- [x] T205 Carry the contract over a named pipe with a recorded random name in
  `crates/ipc/src/transport/windows.rs`, with tests for binding, a live pipe, a stale record, a
  missing daemon and a planted record — done (R-087).
- [x] T206 Stop the daemon through a named event, and on Ctrl-C, console close, logoff and
  shutdown, in `crates/platform/src/stop.rs` and `crates/daemon/src/server.rs` — done: stops
  gracefully from another session once the event moved to the global namespace (R-087).
- [x] T207 Clear `IPV6_V6ONLY` on session sockets — done: the IPv4-peer test fails on Windows
  without it (R-085).
- [x] T208 Claim session ports with `SO_EXCLUSIVEADDRUSE` on Windows — done, with a test that
  fails without it (R-088).

## Phase 3: User Story 1 — Virtual ports (P1)

- [x] T209 [US1] teVirtualMIDI ports in `crates/platform/src/midi/tevirtual.rs`, loaded at run
  time, with the capability reported unavailable without the driver — done.
- [x] T210 [US1] Retire destroyed ports until the next port exists, and keep closed ports' names
  out of the device list, in `crates/platform/src/midi/retire.rs` — done: the eight-rename test
  failed five of five runs without it and passed five of five with it (R-086).
- [x] T211 [US1] Windows integration tests in `crates/platform/tests/windows_midi.rs` — done:
  seven tests, MIDI and a 6 KB dump both ways through another backend.

## Phase 4: User Story 2 — Devices (P1)

- [x] T212 [US2] WinMM inputs and outputs in `crates/platform/src/midi/winmm.rs`, the input
  callback limited to scanning into the ring — done.
- [x] T213 [US2] Identity, pairing and message packing in
  `crates/platform/src/midi/winmm_identity.rs`, tested on every platform — done; every test was
  mutation-checked.
- [x] T214 [US2] The backend in `crates/platform/src/midi/windows.rs`: a watcher comparing lists
  once a second, stable enumeration, opens checked against the handle's own position — done.
- [x] T215 [US2] Replug USB MIDI hardware on Windows and confirm the route resumes

## Phase 5: User Story 3 — Network sessions (P1)

- [x] T216 [US3] Advertise through the DNS Client service, with dots sent as hyphens — done:
  Avahi on the same subnet resolved the Windows port (R-084).
- [x] T217 [US3] Join a session between Windows and Linux — done: joined at 1.0 ms after T207;
  the Windows daemon discovered the Linux port.
- [x] T218 [US3] A session started by another machine inviting the Windows one — done once the
  owner allowed Midi Harbor through Windows Defender Firewall: a Linux daemon invited the Windows
  port by address and joined at 0.9 ms.

## Phase 6: User Story 4 — Service (P2)

- [x] T219 [US4] Task Scheduler backend in `crates/service/src/taskscheduler.rs` — done: installs
  as a standard task, no window, starts at logon.
- [x] T220 [US4] `daemon --supervise` in `crates/service/src/supervisor.rs` — done: a killed
  daemon was running again 2.4 s later and answering 7.3 s after the kill (R-083).
- [x] T221 [US4] A daemon that will not stop is ended with its supervisor — done, with a test that
  fails when the supervisor is left alone.

## Phase 7: User Story 5 — Sleep and wake (P2)

- [x] T222 [US5] Power manager notifications in `crates/platform/src/sysevents/windows.rs` — done;
  registers on the test machine.
- [x] T223 [US5] Detect a suspend from Windows' unbiased interrupt time rather than `Instant` —
  done, with a test that fails on a wrong unit.
- [x] T224 [US5] Sleep and wake a Windows machine with sessions up

## Phase 8: User Story 6 — Bluetooth and the interface (P3)

- [x] T225 [US6] Build and run the interface on Windows, releasing a console only it uses —
  done: the Endpoints page listed the daemon's ports, with no console window beside it.
- [x] T226 [US6] The Bluetooth central role on Windows with a radio, and the peripheral role —
  the central role is built and reports no adapter correctly; the peripheral role is reported not
  built on Windows, as the constitution records.

## Phase 9: Found on the way

- [x] T229 Close every port and device before the daemon exits — done after a daemon stopped
  cleanly and never finished exiting (R-090); the cause is not confirmed, and fifteen cycles since
  exited.

- [x] T230 Refuse moving a network port to a taken UDP pair before stopping it — done after the
  Linux gate failed twice on the undo path (R-091); the test fails without the change.

## Phase 10: Polish

- [x] T227 Constitution amendment 1.1.0, AGENTS.md, and the user documentation — done.
- [x] T228 All gates on macOS, Linux and Windows — done at 7389ff2: fmt, clippy, the full test
  suite and the headless build pass on macOS (357 s) and on the Arch VM (238 s); on Windows,
  clippy passes for the full and headless builds, all 46 test binaries pass, and the seven
  teVirtualMIDI integration tests pass.

## Phase 11: Windows MIDI Services

- [x] T231 [US1] Virtual ports through Windows MIDI Services in `crates/platform/src/midi/wms/`,
  replacing teVirtualMIDI and the retirement T210 added for it: the in-box API when Windows
  registers it, the App SDK otherwise, with UMP translation in `crates/platform/src/midi/ump.rs`
  — done: through the App SDK RC4, six of the seven integration tests pass, each alone with the
  service restarted before it (R-093).
- [x] T232 [US1] Wait on no call into the service for more than a bounded time, and refuse
  virtual ports while one is stalled — done: with all seven tests in one process, the first passed
  and the other six were refused within 7.4 s in total, where before the second waited for good.
- [x] T233 [US1] Recognise this process's ports under the group names the service gives them —
  done: the multi-connector and own-port tests pass, and the naming tests fail when the suffix or
  the cut is changed.
- [x] T234 [US1] Refuse a name one of this process's ports shows before asking the service — done:
  the conflict test failed without it.
- [x] T235 [US1] With Windows' late-2026 update on the test machine, confirm the in-box API is
  chosen with the App SDK runtime still installed, all seven integration tests pass in one run,
  and which names WinMM shows

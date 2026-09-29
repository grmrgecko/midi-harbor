<!--
SYNC IMPACT REPORT (temporary scratch material — remove before committing the amendment)

Version change: (none) → 1.0.0
Rationale: Initial ratification. No prior version existed; the template was unfilled.

Modified principles: none (initial adoption)

Added sections:
  - Core Principles I–VII
  - Platform Support Matrix
  - Development Workflow & Quality Gates
  - Governance

Removed sections: none

Deferred items / TODOs: none. RATIFICATION_DATE set to the date of initial adoption
(2026-09-20) per project start.
-->

# Midi Harbor Constitution

## Core Principles

### I. Resilience Is The Product (NON-NEGOTIABLE)

Midi Harbor exists because the platform-native alternatives drop connections. Any change that
trades away recovery behaviour for convenience is rejected by default.

- Every transport (virtual port, physical device, RTP-MIDI session, BLE MIDI link) MUST be
  modelled as an
  explicit state machine with a defined reconnect path from every failure state.
- Reconnection MUST be automatic, unbounded in attempts, and use exponential backoff with
  jitter, capped at a bounded maximum interval. A link MUST never enter a terminal
  "give up" state as a result of a transient error.
- A link MUST only stay down when the user has explicitly disabled it, or when the failure is
  provably permanent (for example a configuration conflict that requires a user decision).
  Permanent failures MUST be surfaced to the user with a specific, actionable reason.
- MIDI state MUST be repaired across reconnects: on link recovery, the system MUST resolve
  hanging notes and resynchronise controller state rather than leaving sounding notes stuck.
- Rationale: users adopt this tool specifically to stop babysitting MIDI connections. A
  connection that silently stays dead is a total product failure, not a degraded mode.

### II. Daemon Owns State, GUI Is A Client

All MIDI, network, and Bluetooth state lives in a headless daemon. The libcosmic GUI is a
replaceable view over that state.

- The daemon MUST own every virtual port, network session, and BLE link, and MUST run
  independently of any GUI process (launchd user agent on macOS, systemd user unit on Linux, a
  Task Scheduler logon task running the daemon under its own supervisor on Windows, and in the
  sandboxed Mac App Store build a bundled helper process the app starts and supervises).
- Closing, crashing, restarting, or never launching the GUI MUST NOT disturb any active MIDI
  connection.
- The GUI and CLI MUST communicate with the daemon only through the published IPC contract.
  Neither MUST open MIDI, network, or Bluetooth handles directly.
- The IPC contract MUST be versioned, and the daemon MUST reject clients whose major version
  it does not support with a clear error rather than degrading silently.
- Rationale: self-healing that only works while a window is open is not self-healing. This
  split also makes the engine testable and scriptable without a display server.

### III. Real-Time Safety On The MIDI Data Path (NON-NEGOTIABLE)

The path that carries MIDI bytes is a real-time context and is governed by real-time rules.

- Code executing in a MIDI read callback, audio thread, or packet-dispatch hot path MUST NOT
  allocate, free, lock a mutex, perform I/O, log to disk, or block on any channel.
- Communication between real-time and non-real-time contexts MUST use pre-allocated wait-free
  structures (ring buffers, lock-free queues, atomics).
- All buffers used on the data path MUST be pre-allocated at link setup with a documented
  capacity, and overflow MUST be handled by an explicit, counted drop policy — never by
  growing a buffer in the hot path.
- Any violation MUST be treated as a correctness bug, not a performance issue.
- Rationale: MIDI timing defects are audible. A priority inversion or an allocation stall
  produces jitter and dropped events that users perceive directly as the product failing.

### IV. Platform Parity Through Explicit Abstraction

macOS, Linux and Windows are equal first-class targets. None is a port of another.

- Platform-specific code MUST sit behind a trait boundary in a dedicated platform module.
  Core logic MUST NOT contain `cfg(target_os)` branches.
- Every platform trait MUST have an implementation for macOS (CoreMIDI, CoreBluetooth), Linux
  (ALSA sequencer, BlueZ) and Windows (WinMM with Windows MIDI Services, WinRT Bluetooth), plus an
  in-memory fake for tests.
- A feature MUST NOT ship enabled on one platform and silently missing on another. Where a
  capability genuinely cannot exist on a platform, or is not yet built there, it MUST be declared
  in the Platform Support Matrix and reported through a capability query the UI reads at
  runtime.
- Every supported platform MUST build and pass tests before merge.
- Rationale: divergence compounds. Enforcing parity at the type level keeps the two targets
  from drifting into two different products.

### V. Protocol Correctness Over Convenience

Midi Harbor implements published protocols. Interoperability with other implementations is a
hard requirement, not a goal.

- The RTP-MIDI implementation MUST implement the recovery journal (RFC 6295) so that MIDI
  state survives UDP packet loss. Shipping a journal-less session is prohibited.
- The implementation MUST interoperate with Apple's Network MIDI, rtpMIDI on Windows,
  rtpmidid on Linux, and hardware RTP-MIDI endpoints. Interoperability MUST be verified, not
  assumed.
- BLE MIDI MUST follow the standard GATT service and the specified packet encoding, including
  timestamp handling and running-status rules across packet boundaries.
- Protocol parsers MUST treat all input from the network or from a peer device as hostile:
  no panics, no unbounded allocation, no out-of-bounds access on malformed input.
- Rationale: a MIDI tool that only talks to itself is useless. Protocol shortcuts surface as
  field failures against peers we cannot debug.

### VI. Testable Without Hardware

The correctness of this system MUST be demonstrable on a CI runner with no MIDI interface, no
network peer, and no Bluetooth radio.

- Protocol encoders/decoders, the recovery journal, timing/clock sync, and every connection
  state machine MUST be pure and deterministic, so they can be tested in isolation.
- Time MUST be injected as a dependency. Tests MUST NOT rely on wall-clock sleeps to advance
  a state machine.
- Tests come in two tiers. Integration tests are the primary safety net: they drive the daemon,
  the CLI and the protocols as a whole over real files and real sockets, with fakes only at the
  platform seams. Unit tests are few and cover serialization boundaries, external formats and
  facts, and non-obvious algorithms; tests of simple logic, or of what an integration test
  already proves, MUST NOT be added.
- Every resilience behaviour claimed in Principle I MUST have an integration test that induces
  the failure (dropped packets, peer timeout, device removal, daemon restart) and asserts
  recovery.
- Wire-format code MUST be covered by round-trip property tests and by fuzzing on parsers.
- Rationale: resilience claims that are only tested by hand are untested. Failure injection is
  the only way to know the healing paths actually run, and a suite padded with tests that restate
  the code slows every change without catching anything.

### VII. Observable By Default

Diagnosing a dropped connection MUST NOT require a debugger or a rebuild.

- Every connection MUST expose structured, queryable state: current phase, last error, uptime,
  attempt count, and counters for packets/messages sent, received, lost, and recovered.
- Every state transition MUST emit a structured, timestamped log event with a stable event
  name and the transport identity.
- The daemon MUST retain a bounded in-memory event history that the GUI and CLI can read, so
  users can see what happened while they were not watching.
- Logging MUST be off the real-time path, per Principle III.
- Rationale: the product's core promise is about behaviour over time. Users and maintainers
  need evidence of what the connection did, especially for failures that already recovered.

## Platform Support Matrix

Supported targets are macOS 11+ (Apple Silicon and x86_64), Linux (x86_64 and aarch64), and
Windows 10 and 11 (x86_64).

| Capability | macOS | Linux | Windows |
|---|---|---|---|
| Virtual MIDI ports | CoreMIDI virtual endpoints | ALSA sequencer ports | Windows MIDI Services virtual devices (in Windows from its late-2026 update; before it, Microsoft's App SDK runtime) |
| Physical MIDI devices | CoreMIDI endpoints | ALSA sequencer / rawmidi | WinMM |
| RTP-MIDI sessions | Midi Harbor implementation | Midi Harbor implementation | Midi Harbor implementation |
| Service discovery | mDNS (`_apple-midi._udp`) | mDNS (`_apple-midi._udp`) | mDNS (`_apple-midi._udp`) |
| BLE MIDI central | CoreBluetooth | BlueZ | WinRT, through btleplug |
| BLE MIDI peripheral | CoreBluetooth | BlueZ | Not yet built; reported unavailable |
| Service supervision | launchd user agent; in the App Store build, a helper the app supervises | systemd user unit | Task Scheduler logon task and the daemon's own supervisor |

Binding constraints:

- Midi Harbor MUST NOT depend on Apple's IAC driver or `MIDINetworkSession`. It creates and
  owns its own endpoints and sessions. Apple's configuration MAY be read once for migration
  purposes, but MUST NOT be written to.
- Midi Harbor MUST NOT require root or elevated privileges for any normal operation. On Windows
  before its late-2026 update, installing Microsoft's Windows MIDI Services App SDK runtime once
  is a prerequisite for virtual ports and not part of normal operation; without it the
  capability is reported unavailable.
- Any capability that is unavailable at runtime (for example, no Bluetooth adapter present)
  MUST be reported through the capability query and rendered as unavailable in the UI, never
  as a failure.

## Development Workflow & Quality Gates

The following gates apply to every change and MUST pass before merge:

- `cargo fmt --check` and `cargo clippy -- -D warnings` MUST pass.
- `make test` and `make test-integration` MUST pass on macOS, Linux and Windows.
- New protocol or state-machine code MUST arrive with the tests the two-tier rule calls for, in
  the same change.
- Changes touching the real-time data path MUST state in the change description how
  Principle III is upheld.
- Public IPC contract changes MUST include a version bump and a compatibility note.
- Unsafe code MUST be confined to platform FFI bindings, MUST carry a `// SAFETY:` comment
  stating the invariant upheld, and MUST NOT appear in core logic.
- Dependencies MUST be justified. Prefer a small, audited dependency set given the
  privileged, always-running nature of the daemon.

## Governance

This constitution supersedes other development practices for this project. Where a plan, spec,
or review comment conflicts with a principle here, the principle wins.

- **Amendment procedure**: amendments MUST be proposed as a change to this file, MUST state
  the motivating problem, and MUST record the resulting version bump and date below.
- **Versioning policy**: MAJOR for removing or redefining a principle in a backward-
  incompatible way; MINOR for adding a principle or materially expanding guidance; PATCH for
  clarifications and wording that do not change meaning.
- **Compliance review**: every change is reviewed against these principles. Complexity that
  violates a principle MUST be recorded in the Complexity Tracking table of the relevant plan
  with the simpler rejected alternative named. Unjustified violations block the merge.
- **Runtime guidance**: agent-facing and contributor-facing implementation guidance, including
  the project's code style, lives in `AGENTS.md` at the repository root and MUST stay consistent
  with this document.

**Version**: 1.4.1 | **Ratified**: 2026-09-20 | **Last Amended**: 2026-09-27

Amendment 1.1.0 (2026-09-26): Windows added as a first-class target (feature
013-windows-support). Principles II and IV and the Platform Support Matrix name its
implementations, and the test gate covers it. The BLE MIDI peripheral role is declared not yet
built on Windows: no Windows machine with a Bluetooth radio was available to verify it on.

Amendment 1.2.0 (2026-09-26): Windows virtual ports move from the teVirtualMIDI driver, which may
not be distributed without its author's clearance, to Windows MIDI Services. Principle IV and the
Platform Support Matrix name it, and the privilege constraint names the App SDK runtime as the
one-time prerequisite until Windows carries the API.

Amendment 1.3.0 (2026-09-26): Principle VI and the quality gates adopt two test tiers. The suite
had grown to about 900 tests, many restating the code they covered. Integration tests become the
primary safety net and carry the resilience proofs; unit tests are limited to serialization,
external formats and facts, and non-obvious algorithms. AGENTS.md holds the detailed rules.

Amendment 1.4.0 (2026-09-27): the sandboxed Mac App Store build cannot register a launchd agent,
so Principle II and the Platform Support Matrix name a third way of running the daemon apart from
the GUI: a helper executable in the app bundle, which the app starts, restarts after a failure and
stops when the user quits entirely (feature 014-mac-app-store-mode, research R-095). The daemon
stays a separate process, so a crash in the window still disturbs no connection.

Amendment 1.4.1 (2026-09-27): wording only. The specifications were split into one per
capability and renumbered by topic, and the features named above cite the current specs.

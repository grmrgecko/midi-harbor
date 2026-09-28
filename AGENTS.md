# Midi Harbor — Agent & Contributor Guide

Midi Harbor manages virtual MIDI ports, physical MIDI hardware, RTP-MIDI network sessions, and
Bluetooth LE MIDI links on macOS, Linux and Windows, with connection resilience as its defining
value.

Governance lives in `.specify/memory/constitution.md` and takes precedence over this file where
the two disagree. Specifications live in `specs/`.

---

## Code style

- Write pragmatic, systems-oriented Rust focused on correctness and clarity.
- Favor explicit control flow and simple data structures over abstraction. Traits exist only where
  a real seam is needed (instrument drivers, transfer functions), not for hypothetical testability.
- Short receiver names (`self` for most types). Internal helpers are private and idiomatic
  snake_case; exported names are clear and descriptive.
- Return `Result` when the caller can act on the failure. Otherwise log and continue safely. Errors
  are lowercase, no trailing punctuation, and carry context about the attempted operation.
- Library code never panics: no `unwrap`, `expect`, `panic!`, or indexing that can go out of
  bounds. Return errors or saturate deliberately. Tests may use `unwrap` and `assert_eq`.
- Log operational detail at debug, lifecycle events (start/stop of sessions, driver open/close) at
  info, and failures at error. No exclamation marks in log lines.
- Comments are functional and intent-focused, written for someone maintaining or debugging the
  system. The name of the element being commented is the first word, the comment is a complete
  sentence ending in a period.
- Exported types and functions have short `///` doc comments describing responsibility.
- Functions with multiple logical steps use short section comments to label each phase ("Validate
  input.", "Fit the response curve.").
- No conversational language, jokes, or speculative notes. The code reads without comments;
  comments add clarity where reasoning is not obvious.
- Spell out arithmetic constants so numbers are not magic (for example `16.0 + 219.0 * v` for
  video-range encoding).

---

## Where traits are warranted in this project

The style rule above says traits exist only for real seams. In Midi Harbor there are exactly four,
and they are genuine — each has two or more independent implementations that must coexist in
shipped code, not merely in tests:

| Seam | Implementations |
|---|---|
| Platform MIDI | CoreMIDI (macOS), ALSA sequencer (Linux), WinMM with Windows MIDI Services (Windows) |
| Platform Bluetooth | CoreBluetooth (macOS), BlueZ (Linux), WinRT through btleplug, central only (Windows) |
| System events | IOKit power + `SCNetworkReachability` (macOS), logind D-Bus + netlink (Linux), power manager callbacks (Windows) |
| Service manager | launchd (macOS), systemd user units (Linux), Task Scheduler with the daemon's own supervisor (Windows); the App Store build has none, its app supervising a bundled helper instead |

In-memory fakes for these seams exist because the seam already exists, not the other way round.
Do not introduce a trait anywhere else to make something mockable — if a type is hard to test,
restructure it into pure functions over plain data instead.

---

## Real-time discipline

Constitution Principle III is non-negotiable and is the rule most easily broken by accident.

Inside a CoreMIDI read callback, an ALSA sequencer read path, or any packet-dispatch hot path:

- No allocation or deallocation. No `Vec::push`, no `String`, no `Box`, no `format!`.
- No mutex, no `RwLock`, no channel that can block.
- No I/O and no `tracing` macro. Increment an atomic counter; a normal task reads it and logs.
- No `unwrap`, per the style rule above, and no arithmetic that can panic — saturate deliberately.

Cross the boundary with `rtrb` single-producer/single-consumer ring buffers carrying fixed-size
`Copy` events. System-exclusive payloads travel as handles into a pre-allocated pool, never
inline. Buffers are sized at link setup; overflow increments `messages_dropped` and is never
handled by growing a buffer.

A change touching the data path states in its description how this section is upheld.

---

## Unsafe code

Confined to the platform FFI crates. Every `unsafe` block carries a `// SAFETY:` comment stating
the invariant being upheld and why it holds here. No `unsafe` in core logic, protocol crates, the
CLI, or the GUI.

---

## Errors and logging

Error enums are closed and machine-readable — `FailureReason` in particular maps onto IPC error
codes and CLI exit codes, so adding a variant is a contract change. Never widen an error into a
`String` at a boundary a client switches on.

```rust
// Log lifecycle at info.
info!(endpoint = %id, "network session connected");
// Log failure with the attempted operation as context.
error!(endpoint = %id, error = %err, "failed to bind control port");
```

---

## Project layout

Single binary `midi-harbor`, one cargo workspace. The GUI is an optional feature; disabling it
must drop the entire libcosmic dependency tree, so no non-GUI crate may depend on it even
transitively.

```
proto/
  midiharbor/v1/harbor.proto   the daemon contract; generated from, never hand-edited
crates/
  core/        domain types, state machines, routing, config    (no platform; its own file only)
  rtpmidi/     AppleMIDI session control + RFC 6295 journal      (pure, no I/O)
  blemidi/     BLE MIDI packet codec                             (pure, no I/O)
  platform/    the four seams above, plus in-memory fakes
  ipc/         generated gRPC client and server, plus status mapping
  service/     launchd / systemd / Task Scheduler installation, and the supervisor
  daemon/      the engine that owns all state
  cli/         clap subcommands over the gRPC contract
  gui/         libcosmic views — optional feature, thin
src/main.rs    argument dispatch only
```

### The daemon contract

Clients reach the daemon over **gRPC on a Unix domain socket** — never TCP, so the control plane is
not reachable from the network. Windows uses a named pipe that refuses remote clients instead, with
a random name the daemon records in a file where the socket would be, so a file permission still
decides who can reach it (R-087). `proto/midiharbor/v1/harbor.proto` is the contract; changing it is
a contract change whether or not a Rust signature moves. Version lives in the package name, so a
breaking change means `midiharbor.v2`, never a reinterpreted field. Never reuse a field number;
reserve it.

Two streams — `MonitorEndpoint` and `WatchTraffic` — are **lossy by contract**. HTTP/2 flow control
would otherwise let a stalled GUI apply backpressure toward the MIDI data path. Drop the update,
count it, report the count on the next message. Never await capacity on those two.

### Discovery uses two stacks on purpose

Browsing goes through `mdns-sd`; advertising goes through the platform responder: through
`zeroconf` on macOS and Linux, and through the DNS Client service's DNS-SD functions on Windows.
That is not an oversight. `mdns-sd`'s responder announces once at registration and then stops
answering queries from other machines, so a service registered through it is invisible to every
peer — including Apple's own browser — while looking perfectly healthy locally. `zeroconf`'s
browser, meanwhile, returned `0.0.0.0` for most entries. Each is used where it works.

Do not consolidate onto one stack without re-running the two-machine test in R-022. A single
machine cannot tell the difference.

### The configuration file

One YAML document per user, in the platform's standard config directory. Use `serde_yaml_ng`;
`serde_yaml` and `serde_yml` are both deprecated.

People edit this file by hand, which constrains its shape: the endpoint kind is flattened rather
than nested, routes name their endpoints instead of referencing UUIDs, and identifiers are
optional on read so a config can be written from scratch with none. Renaming an endpoint must
rewrite every route that names it, in the same operation — that is what keeps connections intact
across a rename. Writes are atomic (temp file, flush, rename); an unparseable file is moved aside
with a timestamp and never deleted.

Keep `core/`, `rtpmidi/` and `blemidi/` free of platform code and of MIDI, network and radio I/O.
They are where the correctness lives and they must be testable on a machine with no MIDI, no
network, and no radio. The one exception is the configuration file: `core/` reads it, writes it
atomically and moves an unreadable one aside with `std::fs`, and resolves where it lives, as
plan.md places it. Any machine can test that. Nothing else in these crates touches the
filesystem.

---

## Testing

Few tests, each worth its place, in two tiers. More tests are not better: never pad coverage, and
prune tests that break these rules when touching a suite.

### The two tiers

- **Unit tests** live in `#[cfg(test)] mod tests` beside the code and run with `make test`
  (`cargo test --workspace --lib --bins`). They are small and fast, and mainly cover
  serialization boundaries.
- **Integration tests** are the primary safety net. They live in each crate's `tests/` directory
  and the root `tests/`, and run with `make test-integration`
  (`cargo test --workspace --test '*'`). They drive the project as a whole: a real daemon over real files, real sockets on loopback, a
  real gRPC client, real RTP-MIDI and BLE MIDI bytes. Every resilience behaviour in Constitution
  Principle I is proved here, by inducing the failure and asserting recovery.
- **Live tests** are integration tests that need something a CI runner lacks: CoreMIDI, the ALSA
  sequencer, WinMM and Windows MIDI Services, a Bluetooth radio, rtpmidid or Apple's session
  peer. They are `#[ignore = "needs ..."]`, naming what they need, and run with `make test-live`
  on a machine that has it.

### What deserves a unit test

- Serialization and parsing: configuration YAML, the proto mapping, RTP-MIDI packets and the
  recovery journal, BLE MIDI and UMP codecs. Encode, decode and compare, with the invalid input
  in the same test. Wire formats keep their proptest round-trips; parsers keep their fuzz targets.
- Invariants of external formats: what launchd, systemd, Task Scheduler, ALSA, WinMM or Windows
  MIDI Services write or report, against fixtures shaped exactly as they produce them.
- Contracts with an external specification: RFC 6295, Apple's session protocol, the BLE MIDI
  specification, the gRPC contract's error codes. The specification's own bytes or numbers may be
  the assertion. Truncated or older input must not panic or misalign.
- Facts about the outside world the code depends on: an exact spelling a tool prints, a
  protocol quirk, a platform limit. These stay even when the test is four lines.
- Real algorithmic logic with a non-obvious result: clock synchronisation, routing resolution,
  backoff, note tracking. One representative test per axis, never an exhaustive fan-out.

### What must not be unit tested

- Simple logic: trivial predicates, a helper's body restated, a `Display` string nobody parses.
- Anything an integration test already proves through the daemon, the CLI or a real socket.
- A lone rejection. A validation test earns its place only by pinning the accepted boundary case
  beside the rejected one.
- This codebase's own collaborators through a fake. Fakes belong only at the four platform seams,
  where the real thing is hardware or the operating system.

For a borderline case, ask whether the test encodes a fact from outside this codebase. If it
pins a real spelling, format or quirk, keep it. If it only restates this codebase's own code,
delete it: a bug there fails loudly downstream. A throwaway test to check behaviour while
working is fine, but it is deleted before the work is done.

### Mechanics

- Plain `assert!`, `assert_eq!` and `assert_ne!`, each with a message that is a sentence giving
  the reason: `assert_eq!(sent, 1, "a ready peer must not be invited twice")`. Setup that
  invalidates the rest of the test uses `expect("...")` with the same kind of message.
- Table-driven wherever more than one input shape exists: an array of structs or tuples with a
  `name`, the inputs, the wanted result and, where it helps, a `why`. The assert message names
  the case.
- Every test has a `///` comment stating the invariant it locks and why, naming the real-world
  source where there is one: the RFC section, the tool and version, the regression it pins.
  Arithmetic behind an expected number is spelled out in the comment.
- Helpers are small, local to the file, and build fixtures laid out exactly as the real system
  writes them. Real captured output goes in a `testdata/` directory beside the test, with its
  provenance noted; synthetic samples are labelled synthetic.
- Re-read from the source of truth before asserting: reload the configuration from disk after a
  write rather than trusting the daemon's memory of it.
- Time is injected, never read from the clock in logic under test. No `sleep` to advance a state
  machine.
- Parsers treat peer and network input as hostile: malformed input produces an error, never a
  panic, an unbounded allocation, or an out-of-bounds access.

### Real dependencies, never substitutes

Use the real thing, then the real thing isolated, and a fake only as the last resort. Files are
real files in a temporary directory; sockets are real sockets on loopback; a protocol peer is a
real in-process peer speaking the real wire format. The in-memory platform fakes exist because
the platform seams exist: they stand in for MIDI hardware, the Bluetooth radio, power events and
the service manager, which no CI runner has. No mocking library, and no trait added only to make
something testable.

---

## Quality gates

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
make test                            # unit tests
make test-integration                # integration tests
cargo build --no-default-features    # the headless build
! cargo tree --no-default-features | grep -qi cosmic    # must not pull in libcosmic
```

All six pass on macOS, Linux and Windows before merge.

Windows is cross-compiled rather than built on the Windows machine, and its tests run there:

```bash
cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets -- -D warnings
cargo build --target x86_64-pc-windows-gnu --no-default-features
scripts/windows-test.sh --workspace --exclude midi-harbor-gui    # builds test binaries, runs them on Windows
```

The script takes the machine from `MH_WINDOWS_HOST` (`user@host`), which needs only OpenSSH
server. Research R-089 describes what it does, including why the tree is mirrored on the Windows
machine.

The virtual port tests in `crates/platform/tests/windows_midi.rs` cannot run through the script:
Windows MIDI Services never answers a virtual device created from an SSH session (R-093). Copy the
test binary over and start it from the desktop session, as a scheduled task with an interactive
logon. Before Windows' late-2026 update the service also stops answering once a port closes, so
there each test runs alone, with the Windows MIDI Service restarted before it.

The fuzz targets under `fuzz/` are their own workspace, because libFuzzer needs a nightly
compiler, so none of the gates above builds them. Run them after changing a parser, and at
least build them after changing any API they call:

```bash
cd fuzz
cargo +nightly fuzz build
cargo +nightly fuzz run blemidi_codec -- -max_total_time=300    # likewise rtpmidi_packet, rtpmidi_journal
```

Leave `-rss_limit_mb` at its default: libFuzzer itself settles near 480 MB on these targets, so a
lower limit reports an out-of-memory that no input caused.

`--workspace` is not decoration. Without it, cargo lints only the root package and the other
crates as plain libraries, so no test module in any crate is ever linted — a dead import in a
test passes unnoticed.

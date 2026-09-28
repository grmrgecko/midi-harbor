# Research: Service and Clients

**Feature**: 001-service-and-clients, first written as 001-midi-connectivity-manager | **Date**: 2026-09-20

All crate versions below were resolved against crates.io on 2026-09-20. Findings marked
**VERIFIED** were proven empirically in this repository's environment (macOS 15.6, Apple Silicon,
Rust 1.96.0); findings marked **ASSUMED** are literature-based and carry a spike task.

---

## R-003: Direct platform MIDI bindings, or the `midir` abstraction?

**Decision**: Bind the platform APIs directly — `coremidi` 0.9.2 on macOS, `alsa` 0.12.1 on Linux —
behind our own platform trait. Do not use `midir`.

**Status**: **VERIFIED** on macOS (2026-09-20, spike T009). ALSA side remains ASSUMED.

**Spike results (macOS)**:

- `client.virtual_source(..)` and `client.virtual_destination(..)` produce endpoints that appear
  in the system-wide source and destination lists **alongside the real IAC Driver buses**, exactly
  as R-002 predicted. Observed next to the machine's existing `IAC Driver Bus 1`, `IAC Driver
  Test2` and `IAC Driver Test3`.
- `unique_id()` reads `kMIDIPropertyUniqueID`, and it is **settable** via
  `set_property_integer("uniqueID", n)` — confirmed changing an endpoint's uid from a random
  system-assigned value to a pinned one. **FR-004's stable identity across restarts is achievable**,
  and this is the capability `midir` does not expose. The R-003 decision stands.
- `Client::new_with_notifications` delivers `ObjectAdded`, `ObjectRemoved` and `SetupChanged` for
  endpoints created and destroyed by *other* MIDI clients — the hot-plug signal FR-015c needs.

**Rationale**: `midir` 0.11 is a good general-purpose library and does support virtual ports on
both CoreMIDI and ALSA, but it abstracts away three things the spec explicitly requires:

1. **Stable endpoint identity across restarts (FR-004)** needs CoreMIDI's
   `kMIDIPropertyUniqueID`, which `midir` does not expose. Without it, routes cannot reliably
   re-bind to the same port after a reboot or a rename.
2. **Device arrival and removal notifications (FR-022)** need the `MIDINotifyProc` registered at
   `MIDIClientCreate` on macOS, and ALSA sequencer announce-port subscriptions on Linux. `midir`
   offers polling of port lists, not notifications — polling is too slow and too coarse to drive
   self-healing.
3. **Endpoint introspection and connection management** on the ALSA sequencer (client/port
   enumeration, subscription management) is needed for routing and is outside `midir`'s surface.

Binding directly costs more code but is the only way to satisfy the resilience and identity
requirements. The platform trait keeps this contained.

**CRITICAL CONSTRAINT DISCOVERED — CoreMIDI notifications require a CFRunLoop.**

With no run loop on the client's thread, **zero notifications are delivered**, silently. Running
`CFRunLoopRunInMode` on that thread produced all four expected notifications immediately. This was
not anticipated in the original design and it constrains the macOS platform backend:

- The CoreMIDI client MUST be created on a dedicated thread that owns and runs a `CFRunLoop` for
  the process lifetime. It cannot live on a tokio worker.
- That thread forwards notifications to the async world over a channel; it must do nothing else
  that could block the run loop.
- A test that creates the client without a run loop will appear to pass while hot-plug detection
  is entirely dead — so the platform test MUST assert that a notification actually arrives, not
  merely that the callback was registered.

Equivalent care is needed on Linux: ALSA sequencer announce events need a thread polling the
sequencer descriptor. The `MidiPlatform` trait therefore owns a backend thread on both platforms
rather than being a set of free functions.

**Alternatives considered**: `midir` — rejected for the reasons above; it remains a viable
fallback for the virtual-port data path alone if direct binding proves unexpectedly costly.

---

## R-007: Service installation (launchd / systemd user units)

**Decision**: `service-manager` 0.11.0, wrapped behind our own thin trait.

**Status**: **ASSUMED**; low risk.

**Rationale**: `service-manager` already abstracts launchd and systemd (among others) and supports
user-level as well as system-level installation, which is exactly the FR-039f..h surface. Wrapping
it keeps the door open to hand-writing the plist and unit file if its user-level handling proves
too restrictive — generating these two files directly is not difficult and is a credible fallback.

**Platform detail**:

- macOS: a launchd user agent at
  `~/Library/LaunchAgents/com.mrgeckosmedia.MidiHarbor.daemon.plist`, with `RunAtLoad` and
  `KeepAlive` for automatic restart (FR-038).
- Linux: a systemd user unit at `~/.config/systemd/user/midi-harbor.service`, with `Restart=always`
  and `WantedBy=default.target`, enabled via `systemctl --user enable --now`.
- Both are per-user and need no elevation (FR-043).
- The status command must detect a **stale registration** — one pointing at a moved or deleted
  executable — which `service-manager` does not do for us and we must check explicitly.
- Linux systems without systemd are supported only by running `midi-harbor daemon` in the
  foreground, as recorded in the spec's assumptions.

---

## R-008: Real-time safety on the MIDI data path

**Decision**: `rtrb` 0.4.0 single-producer/single-consumer ring buffers between every real-time
producer or consumer and the async world. No allocation, locks, or I/O inside CoreMIDI read
callbacks or ALSA sequencer read paths.

**Status**: **ASSUMED**; enforced by review and test rather than by the compiler.

**Rationale**: Constitution Principle III is non-negotiable and `rtrb` is purpose-built for exactly
this boundary: wait-free, pre-allocated, no allocation on push or pop. Buffers are sized at link
setup with a documented capacity and overflow increments a counted drop rather than growing.

**Design consequences**:

- MIDI events crossing the boundary must be a fixed-size `Copy` type. Variable-length
  system-exclusive data cannot travel inline; it needs a separate pre-allocated pool with the
  ring buffer carrying handles into it.
- `tokio` never touches a real-time callback. The callback's only job is to timestamp and push.
- Logging on the data path is limited to incrementing atomic counters; the `tracing` emission
  happens on a normal task that reads those counters.

**Alternatives considered**: `crossbeam-queue`'s `ArrayQueue` — viable and lock-free, but MPMC
where we need SPSC, so `rtrb` is the tighter fit. A plain `Mutex<VecDeque>` — rejected outright,
priority inversion on the audio path is the exact failure Principle III exists to prevent.

---

## R-009: IPC between the service and its clients

**Decision**: **gRPC (`tonic` 0.14.6) over a Unix domain socket**, with the service defined in a
versioned `.proto` file.

**Status**: **VERIFIED** on macOS (2026-09-20). Changed from the original decision at the product
owner's direction.

**Superseded decision**: the first design was length-delimited JSON over a Unix socket with a
hand-rolled `Hello`/`Welcome` version handshake and a topic-subscription mechanism. No code had
been written against it, so the change cost nothing but documentation.

**Spike results**: a `tonic` server bound to a `UnixListener` and a client connected through
`connect_with_connector` exchanged a unary call and a server-streaming call successfully. The
socket was created at mode `0600`. `tonic-prost-build` **vendors `protoc`**, so the build needs no
system protobuf package — which was the main practical objection to a schema-compiled protocol.

**What gRPC replaces, and what it improves**:

| Concern | Hand-rolled JSON | gRPC |
|---|---|---|
| Contract | Rust types, documented by prose | `.proto` file, compiled — cannot drift |
| Versioning | Custom `Hello`/`Welcome` handshake | Package name `midiharbor.v1`; unknown fields ignored natively |
| Streaming | Topic subscribe/unsubscribe, hand-written fan-out | Server-streaming RPCs with real flow control |
| Framing | Hand-written length prefix, size limits, error paths | Handled by HTTP/2 |
| Other languages | Would need a hand-written client | Generated from the same `.proto` |

**Transport stays a Unix socket, deliberately.** gRPC's usual transport is TCP, and using it here
would put Midi Harbor's entire control plane on the network. A local socket keeps per-user
isolation as a file permission rather than an authentication scheme we would have to design.

**CRITICAL: flow control must be overridden on high-rate streams.** HTTP/2 backpressure would let
a stalled GUI slow the producer — on `MonitorEndpoint` and `WatchTraffic`, that producer path is
adjacent to the MIDI data path, and Principle III forbids it. Those two streams are therefore
**lossy by contract**: a full per-subscriber channel drops the update and increments a counter
reported on the next message, rather than awaiting capacity. `WatchState`, `WatchEvents` and
`WatchInvitations` stay lossless, and a client that falls behind on them is disconnected with
`RESOURCE_EXHAUSTED`. This is the one place gRPC's defaults are wrong for this product.

**Costs accepted**: a build-time codegen step; a heavier dependency tree (`tonic`, `prost`,
`hyper`, `tower`, `axum`) in the headless build; and a wire that is no longer human-readable,
mitigated by `grpcurl` and the `--json` CLI output.

**Error mapping**: gRPC status codes carry the class of failure, and a `harbor-reason` trailing
metadata entry carries the stable slug from `FailureReason::code()`. Clients switch on the slug,
never on the message text, so the closed enum in `midi-harbor-core` remains the single source of
truth from domain reason through to CLI exit code.

**Alternatives considered**: length-delimited JSON — the superseded design; simpler dependency
tree, but every guarantee above would have been ours to build and maintain. D-Bus — Linux
idiomatic, absent on macOS, violates Principle IV. gRPC over TCP — rejected, needlessly exposes the
control plane to the network.

**The `grpc` crate was evaluated and rejected (2026-09-20).** Version 0.9.0 is the gRPC
organisation's new high-level API, and at first glance looks like the more official choice. Three
findings decided against it:

1. **It is a layer over tonic, not an alternative to it.** `grpc` 0.9.0 lists `tonic ^0.14.6`
   among its dependencies, so choosing it means running the same engine with an extra API on top.
2. **Its own documentation states it is a preview**: "not recommended for any production use. All
   APIs are unstable." For a daemon contract meant to stay compatible across releases, an
   explicitly unstable API is the wrong foundation.
3. **It pulls in `rustls`, `hickory-resolver` and `socket2`** — a TLS stack and a DNS resolver,
   both meaningless for a local Unix socket, and both working against the lean headless build
   FR-039b requires.

Worth revisiting once `grpc` reaches a stable release, since the migration would be confined to
`crates/ipc`.

---

## R-023: What a clean Linux machine needs, and what macOS was hiding

**Status**: **VERIFIED** on Debian 12 (2026-09-20). The first time this project had ever been
compiled or run on Linux.

All 307 tests pass on Linux. Getting there corrected three things that a developer machine cannot
reveal, because a developer machine already has what it needs.

**`protoc` is not vendored, contrary to what R-009 recorded.** The codegen crates do not bundle a
protobuf compiler; the macOS build worked because Homebrew had installed one years earlier and it
was on `PATH`. A clean machine fails outright. Fixed by supplying a compiler from
`protoc-bin-vendored` in the build script unless `PROTOC` is already set, so no contributor on any
platform needs a system package. Verified by building with Homebrew off the `PATH`.

**The Avahi bindings need `libclang`.** `zeroconf` depends on `avahi-sys`, which generates its
bindings with `bindgen` at build time. Not discoverable from macOS, where Bonjour needs no
bindings generated.

**The headless test was never headless.** `cargo test --workspace --no-default-features` still
compiles the graphical crate, because it is a workspace member and the flag applies to feature
selection rather than membership. On macOS this passed silently; on Linux it failed on Wayland
headers. The guarantee that matters — that the *binary* carries no graphical dependencies — was
always correct, and is checked separately with `cargo tree`. CI now excludes the crate explicitly.

**Linux build dependencies**: `libasound2-dev`, `libavahi-client-dev`, `libclang-dev`,
`libdbus-1-dev`, `pkg-config`. Adding `libxkbcommon-dev`, `libwayland-dev` and `libudev-dev`
builds the graphical interface as well.

---

## R-028: Platform parity is a claim that has to be tested twice

**Status**: **VERIFIED** (2026-09-20).

The physical-device fix in R-024 was written and verified on macOS. The Linux backend still had
`open_device` returning `NotFound` — so Linux had the identical defect, hidden behind the same
passing test suite, because every routing test uses virtual ports which both platforms do open.

The daemon-level fixes found while building the window (device not reopening after being switched
off, monitor waiting on a switched-off endpoint) were each re-verified on Linux after being fixed
on macOS. Both held, because they live in the daemon above the seam. The seam-level fix did not,
because it does not.

**The rule this suggests**: a fix below the platform seam is only half done when it passes on one
machine, and the tests cannot tell you which half you are on.

---

## R-042: The fuzzers' first minutes found four defects, and none were in the network parsers

**Status**: **FIXED** (2026-09-21), each with a regression test that fails when the fix is
reverted.

T079 and T129 added `cargo-fuzz` targets for the BLE codec, the RTP-MIDI data and control parsers,
and the recovery journal. Each target checks more than "no panic". The BLE decoder must allocate
nothing at all. The RTP parsers must hold no more than a fixed multiple of their input. Timestamps
must never run backwards. A completed dump must be framed and seven-bit clean. Whatever a parser
accepts must survive being encoded and read again. Each target first checks that the allocation
counter is really installed, since without it every allocation check passes by default.

Nothing was wrong in the RTP-MIDI or journal parsers themselves. Everything found was in code all
transports share, or in the BLE framing:

| Found by | Defect | Reach |
|---|---|---|
| RTP round trip, in seconds | `MidiMessage::parse` never checked data bytes. `EE 17 FF` became a pitch bend of 32663, beyond fourteen bits | Every transport, including CoreMIDI and ALSA through `Scanner`. A note of `0xF8` would be forwarded as a status byte in the middle of the next device's stream |
| Reading the parser for the above | `System { status }` had nowhere to keep data. Song position, MTC quarter frame and song select were sent as `[status, 0, 0]` | Every route. Time code sync could not survive a hop through the daemon. Round-tripping could not show this, since both directions dropped the data the same way |
| BLE framing check, after the first fix | `sysex_run` decided it was opening a dump because its first byte was `0xF0`. A continuation's first byte is a timestamp, and a timestamp of 112 ms is `0xF0` | One in 128 split dumps, our own encoder's included. The timestamp went into the payload and the terminator was reported as truncation, so the dump never completed |
| Reading `Scanner` for the same inference | A second `0xF0` while a dump was open started a new dump without marking the old one abandoned | A consumer appending runs would glue two dumps into one containing a status byte |

The fixes: data bytes are validated, and a message that fails is refused. `Scanner` resumes at
the status byte that interrupted a message rather than dropping the rest of the read. A new
`SystemCommon { status, data }` variant carries `F1` to `F3`. `sysex_run` is told whether it is
opening a dump rather than guessing from the byte. The first regression test for the BLE fix
passed against the old code too, because it put a data byte before the timestamp; the bug needs
the timestamp to be the continuation's first byte. It was rewritten, and it now fails against the
old code with exactly the predicted `Truncated`.

Afterwards, 4.4 million BLE runs, 14.5 million RTP packet runs and 7.3 million journal runs found
nothing more. An early "out of memory" on the journal target was blamed on an empty input that
does not reproduce it. libFuzzer's own footprint settles near 480 MB, just under the 512 MB limit
that run used, so the report was the limit and no input caused it.

**Not covered**: `Scanner` still drops a channel message that has a real-time byte between its
data bytes (`90 F8 3C 40`), which MIDI 1.0 permits. The clock is emitted and the note is lost
rather than corrupted. That is the right side to fail on, but it is still a loss.

---

## R-052: What writing the documentation found

**Status**: **FIXED** (2026-09-21), found while writing T151's documentation.

The reference was written from what the binary does, not from the contract: each command was
run against a daemon in a scratch home directory, and its output and exit code recorded. Three
commands did not do what they said, and writing the configuration reference the same way found
four more gaps between the file and what reads it.

- **`port delete` ignored `--yes`.** Its help said the flag confirmed the deletion "without
  prompting", but nothing prompted and nothing checked: the flag was parsed and discarded, and a
  delete went ahead unasked. Deleting a port takes it away from every application using it and
  leaves any route naming it broken. It is now refused without `--yes`, exit 7, as a rename is.
  The refusal comes before the daemon is reached. So did a replacing `config import`'s, which
  had come after connecting: with the daemon down, a script got "not running" and learned it
  needed `--yes` only on the second try.
- **One session had two states.** `status` called an idle session "listening", which is
  what the GUI calls it, and `session list` called the same session "disconnected", which reads
  as a fault. Both now use the same description, in text and in JSON.
- **`--socket` in a shared directory could not work.** Binding made the socket's directory
  private, whether or not the daemon had made it. On macOS, `daemon --socket $TMPDIR/x.sock`
  failed to start ("could not secure …: Operation not permitted"). On Linux, a daemon run as
  root with `--socket /tmp/x.sock` would have set `/tmp` to 0700 and closed it to every other
  user; run as anyone else, it failed like macOS. Only a directory the daemon creates is made
  private now. The socket's own 0600 mode is what keeps other users out, as it always was.

- **A second daemon took the first one's socket.** Binding removed whatever socket file was
  there, so `midi-harbor daemon` run beside the service started a second copy of every port and
  session, on fallback ports, and left the first running and unreachable. Both then wrote the
  same configuration file. The daemon now refuses to start while one answers on its socket. It
  checks before opening anything, and binding refuses a socket that answers as a backstop.
- **A hand-written file could not say less than the daemon writes.** A session given only a
  name and a kind, or a peer given no identifier, made the whole file unreadable. The file was
  set aside, and the daemon started with nothing. Each now takes the default `session create`
  would give it: the endpoint's name to advertise, a port the system chooses (then kept), and a
  generated identifier.
- **`default_invitation_policy` did nothing.** It was written to every file and read by nothing:
  `session create` always sent `prompt`. A create that does not name a policy now takes it.
- **Every start recorded that the Bluetooth adapter was switched off**, then that it was
  available, on a Mac whose adapter was on throughout. The macOS advertising backend reported the
  placeholder it holds until CoreBluetooth answers. It now records the answer only when the radio
  is unusable.

Tests: `tests/confirmation.rs` runs the binary with no daemon: refused is 7, and reaching for the
daemon is 3. `tests/cli_views.rs` compares the two JSON views of one session. The transport's
`a_directory_that_already_exists_is_left_as_it_was` covers the socket directory, and
`a_socket_a_daemon_answers_on_is_not_taken_over` the takeover. `cli_views.rs` also starts the
binary as a second daemon and requires the early refusal, and creates a session through the
binary under a configured default policy. Unit tests cover the bare session, the peer without an
identifier, and the adapter's first answer. Each fails with its fix reverted.

---

## R-053: Platform parity, checked rather than assumed

**Status**: **VERIFIED** (2026-09-21), for T153 (SC-016). The differences that remain are
platform facts, documented for users in `docs/platforms.md`.

The audit went three ways:

- **Every `cfg(target_os)` in the code.** Each seam has one backend per platform, and every trait
  method is implemented by both MIDI backends; none is left at a default.
- **The test suites themselves, by name.** The full workspace was listed on both platforms and
  diffed. Beyond each platform's own paths test, macOS had backend tests Linux lacked:
  - a created port visible to other applications;
  - identity kept across recreation;
  - MIDI sent to hardware reaching it, through the IAC bus.

  The ALSA backend now has the same three. It is tested against the real sequencer, with a
  second client standing in for another application and the kernel's Midi Through port for the
  IAC bus. All pass on the Linux desktop.
- **Building and running what the gates skipped.** The Linux gates had always run with
  `--exclude midi-harbor-gui --no-default-features`, so the graphical interface had never been
  compiled on Linux. On the Linux desktop it builds with no extra packages, and the whole
  workspace passes its tests, GUI included: 557 tests, against 564 on macOS, the difference being
  exactly the platform-specific tests above. Run under Xvfb against a real ALSA daemon, it drew
  the endpoint list correctly: a virtual port, the session listening, and the kernel's Midi
  Through port.

**One claim was false on Linux.** `capabilities` reported network discovery and service
installation as available on every machine. Advertising on Linux needs avahi-daemon, and
installing the service needs systemd. A machine without them was told yes, and each session went
unannounced, or `service install` failed. Now:

- network discovery reports avahi-daemon missing when its socket is absent;
- the daemon reports service installation unavailable when no service manager is found;
- network discovery also reports unavailable when discovery never started, which a comment had
  claimed all along.

Verified on Debian with avahi hidden from one process by a private mount:
"network discovery: no, avahi-daemon is not available on this system". The decision is a pure
function, unit tested; it fails with its overrides removed.

**Differences that are the platforms', not ours**:

| | macOS | Linux |
|---|---|---|
| Port identity across restarts | CoreMIDI identifier, pinned | client and port name; the client number changes |
| Hardware identity | CoreMIDI identifier | USB serial, then socket |
| A device held by another application | cannot happen: CoreMIDI shares devices | reported as claimed, with the holder |
| Advertising sessions | system Bonjour | needs avahi-daemon |
| Service without a service manager | n/a | not installable; run the daemon directly |
| Sleep notice | IOKit | logind, silent on return from hibernation |

Bluetooth between the two platforms was verified in R-064, with the Mac as central, which also
records why a Linux central cannot connect to the Mac; a real suspend on each was verified in
R-070 and R-072.

---

## R-054: Packages, and what a dependency scanner cannot see

**Status**: **BUILT** (2026-09-21), for T152 and the build half of T063. Nothing was installed on
any machine; each package was inspected and its binary run from an extracted copy.

| Package | Built on | Checked |
|---|---|---|
| `Midi Harbor.app` and `Midi-Harbor-0.1.0.dmg` | this Mac | signature verifies, ad hoc with the hardened runtime; identifier `com.mrgeckosmedia.MidiHarbor`; Bluetooth usage text present; the image mounts with the app and an Applications link |
| `midi-harbor-headless_0.1.0_amd64.deb` | Debian 12 | dependencies from `dpkg-shlibdeps`; extracted binary runs; `gui` explains its absence and exits 2 |
| `midi-harbor-0.1.0-1.x86_64.rpm` and `midi-harbor-headless` | the Linux desktop | dependencies from rpmbuild; conflicts with each other; recommends avahi and bluez |

The full `.deb` was not built: Debian has no GUI build dependencies and the desktop has no dpkg.
The script is the same one, with the variant as its argument.

**The graphical interface loads what it draws with, rather than linking it.** rpmbuild found only
`libxkbcommon` among its display libraries. Tracing the interface under a virtual display showed
it opening the Wayland and X11 client libraries, `libxkbcommon-x11`, `libXi`, and the EGL and
Vulkan loaders. None of these is linked, so every dependency scanner misses them. On a minimal
system the package would install, and the interface would fail to open a window. The full packages
now recommend them: by Debian package name in the `.deb`, and by library name in the `.rpm`,
which any RPM distribution resolves. They are recommended rather than required, since one window
system and one renderer are enough.

**Found on the way**: `midi-harbor gui` in a headless build said it had no graphical interface
twice, once on each stream, in two wordings.

**What T063 still needs**: `service install` registers the executable it runs from. Run from
inside the app, it therefore registers the bundled binary, as T063 asks, and the unit tests
already cover the plist written for that path. Whether launchd's daemon then gets Bluetooth
permission as Midi Harbor is the question in R-006.

---

## R-059: Surviving a reboot on macOS

**Status**: **VERIFIED** (2026-09-21), T154 scenario 1 on the Mac, with the owner rebooting it.

The app was built from the current code and placed in `~/Applications`. Its own binary then ran
`service install --start`, so the launch agent names
`~/Applications/Midi Harbor.app/Contents/MacOS/midi-harbor daemon`. A port, "Reboot Check",
was added to an existing configuration with two ports, a switched-off port, a session and three
routes, one of them broken. The service's status and all endpoints, sessions, routes and
capabilities were saved, along with whether another application could see the port.

After the reboot and login, the saved state and a fresh reading are identical apart from the
date:

- the service was running from inside the app;
- every endpoint came back in the same state;
- the session was listening on the port the system had chosen before, 62228, which is now
  pinned;
- the broken route was still shown as broken;
- another application could see "Reboot Check".

The history recorded the addresses changing as the network came up after the daemon had
started. The daemon had started before the network.

`capabilities` reported both Bluetooth roles usable for the daemon launchd started from the app,
which is the permission question R-006 left open for T063.

**Found on the way, and fixed**: `service install --start` said "started" as soon as launchd had
launched the process, before the daemon was listening. The quickstart's next command,
`port create`, failed with "the daemon is not running". SC-014c's "one install command plus one
create command" failed on its second command. Starting now waits, up to ten seconds, for the
daemon to answer. Stopping the service, then running `service start` and `port create` back to
back, worked first time. From install to a port another application could see took about 1 s,
against SC-001's 30 s.

---

## R-081: The repeater, headless parity, and diagnosing, checked by hand

**Status**: **VERIFIED** (2026-09-25), T154 scenarios 3, 4 and 6.

**Scenario 3, the repeater.** A USB pad controller stayed on the Linux desktop, under a scratch
daemon built on Debian, routed into a network port. A scratch daemon on the development Mac
connected to it; a virtual port there, watched with `monitor`, stood in for the sound module.
The Linux side held the invitation until it was answered, as its default policy says. Latency
read 3.2 ms. Five pads arrived as fifteen messages, with both routes counting fifteen. The owner
then held a pad, pulled the cable, and plugged it into a different USB port. The Mac received
sustain-off, note-off, all-notes-off and all-sound-off for the held pad, across the network.
The device was open again 14 s after it went away, the route resumed without being recreated,
and later pads arrived. Adding the reverse route on each machine warned on both, naming the
routes in the cycle, and three taps then added six messages to each forward route and none to
the reverse ones. SC-010c asks for 100 replugs; the owner caps replugs at about ten, and eight
were used across R-079 and this entry.

**Scenario 4, diagnosing.** Every CLI command is a new client that exits, so the unplug and
replug above, read back with `events` afterwards with their local times, are the history
outliving the clients that caused them. Creating "Sequencer Bus" twice exited 6 with "the name
'Sequencer Bus' is already in use", and so did a network port of that name (T197).
`diagnostics export` wrote capabilities, configuration, counters, daemon, endpoints, events and
routes. The quickstart asked for `events --since "10 minutes ago"`, which the CLI never accepted:
`--since` takes an event identifier, as the CLI reference says. The quickstart now shows plain
`events`.

**Scenario 6, headless parity.** On the Arch VM, with the headless build: the bare command's help
says the graphical interface is not included, `gui` exits 2 saying the same, and `service
install --start`, `port create "Headless Bus"`, `network create "Studio"` and the route between
them all worked. After a reboot and an ssh login, the service was running and `status --json`
matched the saved copy apart from times and counters, with both ports visible to ALSA. The VM has
a desktop installed, though nothing graphical ran, and a systemd user service starts at login, not
at boot. The reboot itself hung in shutdown on the desktop's Hyprland portal, which survived
SIGKILL; the daemon had stopped in under a second. The owner reset the VM.

---

## R-012: Single binary, optional GUI

**Decision**: One `midi-harbor` binary using `clap` 4.6.7 derive subcommands. The GUI is a `gui`
cargo feature, on by default, that can be disabled for a headless build.

**Status**: **VERIFIED** as feasible (this is standard cargo feature work); **ASSUMED** for the
specific dependency hygiene below.

**Rationale**: FR-039 and FR-039b. The critical constraint is that disabling the feature must
remove the libcosmic dependency *tree* entirely, not just the GUI code — otherwise the headless
build still drags in wgpu and winit and fails to build on a server with no graphics libraries
(SC-014a). That means libcosmic must be an `optional = true` dependency gated by the feature, and
no non-GUI crate in the workspace may depend on it even transitively.

**Command surface**:

```
midi-harbor                        # launches GUI, or prints help on a headless build
midi-harbor daemon                 # runs the service in the foreground
midi-harbor gui                    # launches GUI explicitly
midi-harbor service install|uninstall|start|stop|status
midi-harbor port list|create|rename|delete|enable|disable
midi-harbor session list|connect|disconnect|add|remove|discover
midi-harbor bluetooth scan|connect|disconnect|list|advertise
midi-harbor route list|create|delete|enable|disable
midi-harbor monitor <endpoint>
midi-harbor status [--json]
midi-harbor diagnostics export
```

A global `--json` flag makes every command emit machine-readable output (FR-039d).

**Consequence**: the GUI must be a thin crate containing only view and message-handling code. All
state, validation, and vocabulary live in shared crates that the CLI uses too. This is the
structural expression of FR-039c — every GUI action has a CLI equivalent because both drive the
identical IPC contract.

---

## Open risks carried into planning

| ID | Risk | Severity | Mitigation |
|---|---|---|---|
| RISK-1 | RTP-MIDI recovery journal is substantial, intricate, and has no reference Rust implementation to lean on | High | Sequence it early; property-test against a packet-loss simulator; verify interoperability against Apple and rtpMIDI continuously rather than at the end |
| ~~RISK-2~~ | ~~macOS BLE peripheral support rests on a 0.2.0 crate~~ | **RETIRED** | Spike T008 proved advertising works on macOS with the correct GATT UUIDs. Parity preserved. |
| ~~RISK-3~~ | ~~Linux: `mdns-sd` coexistence with `avahi-daemon` still untested~~ | **RETIRED** | Spike T007 proved macOS coexistence. On 2026-09-25, on the Linux desktop with `avahi-daemon` running and holding UDP 5353, a scratch daemon's `network discover` found a record the Mac registered with `dns-sd -R`, at `192.0.2.10:5004`, and the Mac's `dns-sd -B` saw the network port the daemon advertised through Avahi. |
| RISK-4 | libcosmic is pinned to a git revision with no macOS CI coverage upstream | Medium | Pin an exact revision; treat updates as deliberate changes with a manual macOS smoke test |
| RISK-5 | Sleep/wake platform events are unreliable | Medium | Already mitigated by design — liveness timeouts are authoritative, platform events only accelerate recovery; tests run with platform events disabled |
| RISK-6 | Direct CoreMIDI and ALSA bindings mean `unsafe` FFI | Medium | Confine to the platform crate per the constitution; every `unsafe` block carries a `// SAFETY:` comment; in-memory fakes keep core logic testable without FFI |
| RISK-7 | Latency budgets (SC-008, SC-009) may not survive the ring-buffer plus async hop design | Medium | Build the measurement harness alongside the data path, not after it |
| RISK-8 | Physical device identity on Linux is unreliable for hardware reporting no USB serial number | Medium | Composite key with a confidence level; surface the limitation to the user rather than mis-binding routes |
| ~~RISK-9~~ | ~~Cross-machine repeater loops cannot be detected from local configuration~~ | **RETIRED** | Detected where they close, on the session-to-session route, by what a session sent coming straight back (R-062) |
| ~~RISK-11~~ | ~~Two Midi Harbor machines cannot discover each other~~ | **RETIRED** | Root cause was the pure-Rust responder not answering cross-machine queries. Advertising moved to the platform responder; mutual discovery verified on two machines (R-022) || RISK-10 | CoreMIDI delivers no notifications without a CFRunLoop, and fails silently when one is absent | **High** | Discovered in spike T009. The macOS backend owns a dedicated run-loop thread; the platform test asserts a notification actually arrives rather than that a callback was registered |

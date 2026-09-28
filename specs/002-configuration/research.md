# Research: Configuration

Entries from the research log, under their original numbers. Findings marked **VERIFIED** were
proven in this repository; **ASSUMED** ones rest on the literature.

---

## R-043: Applying configuration while running, and what building it uncovered

**Status**: **VERIFIED** (2026-09-21). Reload and import were run through the binary on macOS, and
a setup exported from macOS was imported on Linux.

T061 applies a changed configuration to a running daemon, and the export, import and reload
commands use it. Each endpoint in the new document is compared with the running one, ignoring
what is observed and not stored (whether hardware is attached, radio signal strength). What
differs decides how much is done:

| Change | Done |
|---|---|
| None | Nothing |
| A label, or a session's invitation policy | Changed in place |
| Only switched on or off | Switched |
| Anything it was opened with, including a virtual port's name, which is its name on the platform | Closed, silenced both ways, and opened again |

A platform handle that is the same before and after is the test's evidence that an endpoint was
left alone. Treating every change as a restart fails six of the eleven tests.

**Reload refuses a file it cannot read.** At startup an unreadable file is set aside and the daemon
starts from defaults, because it has nothing to lose. A reload doing the same would tear down a
working setup over a typo in a file the user is still editing. So the file stays where it is,
everything keeps running, and the command exits 2 with the parser's message. The contract's
`repaired_from` field is therefore never set; it stays in the contract, documented as unused.

**Import matches by kind and name.** An imported endpoint whose identifier is unknown here takes
the identifier of a local one with the same kind and name. That way a setup moved from another
machine lines up with what is already running instead of restarting it. This machine also keeps
its own name, since two machines advertising under one name cannot be told apart. A replacing
import keeps hardware attached here, because a file from elsewhere cannot mention it. The first
cross-platform run showed why: it reported this machine's `Midi Through Port-0` removed, and
discovery then added it straight back under a new identity. A virtual port's platform identifier
is dropped on import, since it was assigned by another machine's MIDI system.

**SC-015, by hand.** Two ports, a network session and two routes were exported from macOS and
imported on Linux with `--mode replace --yes`. The same endpoints and routes appeared. ALSA listed
both ports under the daemon's client, and no CoreMIDI identifier was stored. The Mac's IAC buses
appear as absent hardware, which is how routes to hardware survive its absence.

Building this required stopping and starting endpoints on demand, which turned up five defects
in paths that already existed:

| Defect | Effect |
|---|---|
| Disabling a network session changed only the configuration | It went on listening, advertising and carrying MIDI, and status showed its live phase. The GUI's toggle reached this. Enabling a session that started disabled did nothing until a restart |
| Disabling or deleting a source port silenced what it was playing, not what it was sending | A note held when the keyboard's port was switched off rang on the synth indefinitely (FR-026) |
| `daemon --socket` was accepted and ignored | The daemon listened on the standard socket, where it collided with, or answered for, the real one. The GUI ignored the flag too |
| A fingerprint carrying only a name did not match itself | Every enumeration added the device again under the same name and opened it again. Real backends always supply more than a name, but the duplicate name was a defect regardless |
| Renaming a port changed only the configuration | Other applications saw the old name until the daemon restarted. The port is now recreated under the new name with the same platform identifier. On macOS CoreMIDI reported the new name with the unique ID unchanged; on Linux the port kept its ALSA address |

Each has a test that fails when its fix is reverted. The session fix was also
checked from outside the daemon: its UDP ports were released, and a fresh `dns-sd` browse no longer
listed it.

**Still open**: a machine's name change applies from the next start, since discovery was started
under the old one and the reply says so. Position-less identical devices, and Linux hardware whose
ALSA client number changes on replug, can still gain an entry per replug. The stable topology in
T108 is the fix for both.

---

## R-061: Importing Apple's MIDI setup

**Status**: **DONE** (2026-09-21), for T150. The owner chose an explicit command that shows what
it would do, over an import that runs by itself on first start.

`midi-harbor config import-apple` reads Apple's setup through CoreMIDI's queries. The IAC
Driver's buses are the entities of its device, and each network session is an entity of the
network driver's device. It then lists the virtual ports and sessions it would create, leaving
alone any name already in use. It changes nothing without `--yes`, and exits 7 to say so. With
`--yes` it creates them through the daemon's ordinary requests, so the contract is unchanged.
Sessions get ports of their own, since Apple's session keeps the one it uses. When the IAC
Driver is switched on, the preview warns that applications will see both its buses and the new
ports.

It never writes Apple's configuration (R-002). Reading happens through CoreMIDI, never the
setup file, and no call made sets a property. Apple's MIDI server rewrites `Default.mcfg` by
itself whenever its devices change. That includes a Bluetooth device it recorded after R-058's
test paired the two machines. So a changed modification time is not a sign anything here wrote
to it.

**Measured on this Mac**:

- It found the three IAC buses (Bus 1, Test2, Test3), the driver switched on, and no sessions.
- The preview listed the three ports and changed nothing.
- Against a daemon on a scratch configuration, `--yes` created them, and other applications saw
  "Bus 1".
- A second run found nothing to import and named all three as already here.

On Linux the command reports that there is no Apple setup, exit 4. The planning is a pure
function, unit tested:

- names already here are skipped;
- a port and a session may share a name;
- a name Apple lists twice is created once.

The reader has a smoke test on macOS.

---

## R-011: Configuration storage

**Decision**: **YAML** via `serde_yaml_ng` 0.10 and `serde` 1.0, in each platform's standard
per-user configuration directory.

**Status**: **VERIFIED** (2026-09-20). Changed from TOML at the product owner's direction.

**Superseded decision**: TOML. The change is confined to the encoder and the file extension; the
schema, atomic-write and corruption-recovery behaviour are unaffected.

**Crate choice matters here.** `serde_yaml` is published as `0.9.34+deprecated` and is
unmaintained. `serde_yaml_ng` 0.10 is the maintained continuation and is what this project uses.
`serde_yml` is also deprecated and must not be used.

**Resolved paths** — `directories::ProjectDirs` was rejected after testing, because it produced
`~/Library/Application Support/com.mrgeckosmedia.Midi-Harbor`, which is awkward for a document the
user is expected to open. `BaseDirs` plus an explicit per-platform directory name gives:

| | macOS | Linux |
|---|---|---|
| Config | `~/Library/Application Support/Midi Harbor/config.yaml` | `$XDG_CONFIG_HOME/midi-harbor/config.yaml`, falling back to `~/.config` |
| Socket | `$TMPDIR/midi-harbor/daemon.sock` | `$XDG_RUNTIME_DIR/midi-harbor/daemon.sock`, falling back to `~/.cache` |

macOS has no `XDG_RUNTIME_DIR`, and the socket is transient state that must not outlive a boot,
so the per-user temporary directory is the closest correct equivalent.

**The file holds the whole setup**, endpoints and the MIDI connections between them, which is what
makes export and import (FR-052) a file copy.

**Two decisions taken to make the file genuinely hand-editable**:

1. **The endpoint kind is flattened.** A nested tagged enum serialises as `kind: {kind: ..., ...}`,
   which reads badly. Flattening gives one block per endpoint with `kind: virtual_port` as a plain
   field.
2. **Routes reference endpoints by name, not by identifier.** Persisting UUIDs would have put
   values in the file that a person can neither read nor safely edit. Identity is not lost:
   renaming an endpoint through the daemon rewrites every route naming it, in the same operation,
   which is what FR-004 actually requires. A name matching nothing leaves the route visible and
   marked broken (FR-035) rather than silently dropped. Route identifiers are derived from the
   endpoint pair with UUIDv5, so they stay stable across restarts without being written down.

Endpoint identifiers are optional on read and generated when absent, so a configuration can be
written from scratch by hand with no identifiers at all. The daemon fills them in next time it
writes the file.

**Unchanged from the original decision**: writes are atomic — a temporary file in the same
directory, flushed, then renamed — so an interrupted write cannot corrupt the setup. A file that
cannot be parsed is renamed aside with a timestamp and never deleted, the daemon starts from
defaults, and the problem is surfaced (FR-051). The document carries a schema version, and a
schema newer than the build understands is refused loudly rather than silently downgraded.

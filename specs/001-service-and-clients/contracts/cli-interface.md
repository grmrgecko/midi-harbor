# Contract: Command-Line Interface

**Feature**: 001-service-and-clients, first written as 001-midi-connectivity-manager | **Version**: 1.0 | **Date**: 2026-09-20

The user-facing contract for the `midi-harbor` executable. Governed by FR-039 through FR-039h and
FR-042. Every command here is a thin projection of the [IPC protocol](./ipc-protocol.md); the CLI
holds no state of its own.

---

## 1. Invocation model

One executable selects its role from its arguments (FR-039). There is no separate daemon binary.

```
midi-harbor                    # full build: launches the GUI
                               # headless build: prints help and exits 0 (FR-039e)
midi-harbor gui                # launches the GUI explicitly; exits 2 on a headless build
midi-harbor daemon             # runs the engine in the foreground (FR-039a)
```

Running `daemon` registers nothing with the operating system, so it can be supervised externally
or attached to a debugger.

### Global flags

| Flag | Effect |
|---|---|
| `--json` | Machine-readable output on stdout (FR-039d) |
| `--socket <path>` | Override the daemon socket path |
| `-v`, `-vv` | Raise log verbosity to debug, then trace |
| `--quiet` | Suppress non-error output |
| `--no-color` | Disable styling; also honours `NO_COLOR` |

`--json` emits a single JSON document per invocation on stdout. Diagnostics go to stderr, so
`midi-harbor --json status | jq` is always safe.

---

## 2. Service management (FR-039f, FR-039g, FR-039h)

```
midi-harbor service install [--start] [--name <label>]
midi-harbor service uninstall [--purge-config]
midi-harbor service start
midi-harbor service stop
midi-harbor service status
```

`install` writes a per-user service definition — a launchd agent on macOS, a systemd user unit on
Linux — enables it for login, and with `--start` starts it immediately. It never requires
elevation (FR-043) and never asks the user to author a plist or unit file by hand.

Re-running `install` when a registration already exists **updates it in place** and says so,
rather than duplicating it (edge case).

`uninstall` stops and deregisters the service and leaves configuration intact (FR-039h).
`--purge-config` additionally removes configuration, and prompts unless `--yes` is given.

`status` reports installed, running, version, uptime, and socket path. It explicitly detects a
**stale registration** — one pointing at a moved or deleted executable — and names the missing
path (edge case).

Where no supported service manager exists (a container, or Linux without systemd), `install`
fails with a clear message and tells the user to run `midi-harbor daemon` under their own
supervisor (edge case, R-007).

---

## 3. Endpoint commands

### Virtual ports (FR-001..007)

```
midi-harbor port list [--kind <kind>]
midi-harbor port create <name> [--direction in|out|both]
midi-harbor port rename <name-or-id> <new-name> [--yes]
midi-harbor port delete <name-or-id> [--yes]
midi-harbor port enable <name-or-id>
midi-harbor port disable <name-or-id>
```

`rename` warns that connected applications may need to reselect the port and requires confirmation
(FR-001 scenario 4); `--yes` supplies it non-interactively. `delete` likewise requires `--yes`,
since every application using the port loses it, and exits `7` without it. It silences sounding
notes before removing the port (FR-006).

Endpoints are addressable by name or by id everywhere. An ambiguous name is an error listing the
candidates with their ids — never a silent pick.

### Physical devices (FR-015a..g)

```
midi-harbor device list [--all]
midi-harbor device forget <name-or-id>
midi-harbor device resolve <name-or-id>
```

`--all` includes remembered devices that are currently unplugged (`present: false`). `resolve`
disambiguates two identical devices whose fingerprints matched with `Ambiguous` confidence.

### Network sessions (FR-008..015)

```
midi-harbor session list
midi-harbor session create <name> [--port <n>] [--policy prompt|known|all|reject]
midi-harbor session discover [--timeout <secs>]
midi-harbor session connect <session> <peer>
midi-harbor session disconnect <session>
midi-harbor session peer add <address>[:<port>] [--name <name>]
midi-harbor session peer remove <peer>
midi-harbor session policy <session> <prompt|known|all|reject>
```

`discover` browses for `_apple-midi._udp` peers and prints them; it never lists this machine's own
advertised session (edge case). `connect` returns as soon as the session enters `Connecting` —
progress is observable through `status` and `monitor`.

### Bluetooth (FR-016..021)

```
midi-harbor bluetooth scan [--timeout <secs>]
midi-harbor bluetooth connect <address-or-name>
midi-harbor bluetooth disconnect <name-or-id>
midi-harbor bluetooth forget <name-or-id>
midi-harbor bluetooth advertise <on|off> [--name <name>]
```

When Bluetooth is unavailable — no adapter, adapter off, or permission not granted — these
commands exit `4` with the specific reason and guidance on granting permission (FR-021).

---

## 4. Routing (FR-030..035)

```
midi-harbor route list [--broken]
midi-harbor route create <source> <destination>
midi-harbor route delete <id>
midi-harbor route enable <id>
midi-harbor route disable <id>
```

`create` succeeds even when it forms a cycle, printing a warning naming the routes in the cycle
(FR-033). The repeater case is just a route whose source is a physical device and whose
destination is a network session (FR-030a) — no special command exists, deliberately.

---

## 5. Observation and diagnostics

```
midi-harbor status [--watch]
midi-harbor monitor <endpoint> [--raw]
midi-harbor dismiss-warning
midi-harbor send-note <endpoint> [--note <0-127>] [--channel <1-16>] [--velocity <1-127>] [--length <ms>]
midi-harbor events [--since <time>] [--endpoint <id>] [--follow]
midi-harbor diagnostics export [--output <path>] [--include-messages]
```

`status` prints every endpoint with its phase, time in phase, last error, next retry, and traffic
counters (FR-044), with when a message was last received and last sent. A network port's
automatic port has a row of its own beneath it, since the network port's counts say what the
network carried and the automatic port's say what reached the applications on this computer.
`--watch` redraws on events.

When the daemon replaced one that lost the platform's MIDI service, `status` warns above its
table, and `--json` carries the time the loss was found as `midi_server_replaced_at`: the daemon
recovered, but other applications may have lost their MIDI connection with the service and need
relaunching. The warning stands until `dismiss-warning`, or Dismiss in any window, clears it in the
daemon for every client.

`monitor` prints decoded MIDI in human-readable form (FR-047); `--raw` prints hex bytes. Both are
explicitly lossy under load and report the number of dropped updates rather than applying
backpressure to the data path.

`send-note` sends one note out of an endpoint for testing: the note-on at once, and the note-off
after `--length` milliseconds (500 by default, at most 10000), sent by the daemon so a command
stopped mid-note cannot leave it sounding. Numbers outside MIDI's ranges exit with a usage error;
an endpoint nothing can be sent out of, or one switched off, is refused as a failed precondition.

`diagnostics export` writes configuration, connection history, and counters to a file for bug
reports (FR-048).

---

## 6. Configuration (FR-049..052)

```
midi-harbor config path
midi-harbor config show
midi-harbor config export [--output <path>]
midi-harbor config import <path> [--mode merge|replace] [--yes]
midi-harbor config reload
```

`import` and `reload` diff against running state and disturb only the connections whose
configuration actually changed (FR-050).

---

## 7. Exit codes

Stable and scriptable. Derived from the IPC `error.code`.

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Generic failure |
| `2` | Usage error, or GUI requested on a headless build |
| `3` | Daemon not running or not reachable |
| `4` | Requested capability unavailable on this system |
| `5` | Not found — no such endpoint, route, peer, or device |
| `6` | Conflict — name already in use, duplicate route |
| `7` | Confirmation required and not supplied |
| `8` | Protocol version mismatch between client and daemon |

---

## 8. Behavioural requirements

1. **Every GUI action has a CLI equivalent** (FR-039c). This is verified as a test: the set of IPC
   request kinds reachable from the GUI must be a subset of those reachable from the CLI.
2. **The daemon not running is a recognised, recoverable state** (FR-042). Any command needing the
   daemon exits `3` with an offer:
   `daemon is not running — run 'midi-harbor service install --start' to install and start it at
   login, or 'midi-harbor daemon' to run it in the foreground`.
3. **A headless build behaves identically for every non-GUI command** (FR-039b scenario 4). The
   only differences are bare invocation printing help, and `gui` exiting `2`.
4. **No command blocks indefinitely.** Operations that take time return once the state machine has
   accepted the request; progress is observed through `status`, `events`, or `monitor`.
5. **`--json` output is contract-stable.** Field names match the IPC wire types. New fields may be
   added in a minor version; removal or re-typing is a major version.

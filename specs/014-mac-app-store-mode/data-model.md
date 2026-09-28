# Data model: Mac App Store mode

The daemon's configuration, the contract and the domain types are unchanged. Two small things are
new, both held by the app rather than the daemon.

## Window state at quit

Whether the window was open when Midi Harbor last quit entirely (FR-A10).

- **Where**: `window.yaml` in the configuration directory, which the sandbox puts in the
  container: `~/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/Library/Application Support/midi-harbor/window.yaml`.
  A separate file, because the daemon owns `config.yaml` and rewrites it from what it holds.
- **Shape**:

  ```yaml
  open: true
  ```

- **Written**: when Midi Harbor quits entirely, atomically as the configuration is (temporary
  file, flush, rename). Hiding the window writes nothing; only the state at quit matters.
- **Read**: at launch. A missing or unreadable file reads as open, so a first launch at login, or
  one after a crash, shows the window rather than hiding the app.
- **Used**: only for a launch at login. Launched any other way, the window opens.

## Login item

The app's registration to start at login. macOS owns it; the app never stores it.

| `SMAppService.Status` | Switch shows | Turning it on |
|---|---|---|
| `enabled` | on | |
| `requiresApproval` | waiting, with where to approve it | |
| `notRegistered` | off | `register()` |
| `notFound` | off | `register()`, which decides (R-099) |

A `register()` or `unregister()` that fails shows its error beside the switch, and the status is
read again. The first launch in App Store mode offers it once; whether it has been offered is
kept in `window.yaml` as `login_offered: true`.

## Daemon lifecycle, as the app sees it

```text
app starts ──► socket answers? ──yes──► attached (did not start it)
                     │no
                     ▼
               start helper ──► running ◄──┐
                     │ exits with failure  │ after the supervisor's delay
                     └─────────────────────┘
running ──quit entirely──► SIGTERM, wait for exit ──► app exits
attached ──quit entirely──► StopDaemon, wait for the socket to close ──► app exits
running or attached ──window crashes──► daemon keeps running; next launch attaches
```

An attached daemon is one the app did not start, left by a window that crashed. The app stops it
on Quit all the same, since in App Store mode nothing else would. The sandbox forbids signalling a
process the app did not start, so it asks through the contract's `StopDaemon` (R-095).

# Contract: the command line in App Store mode

The gRPC contract gains one call, `StopDaemon`, which stops the daemon through its graceful
shutdown as SIGTERM does. It is additive: the protocol minor version rises to 1.1, and a 1.0 daemon
answers it with `UNIMPLEMENTED`. The App Store app uses it to stop a daemon an earlier instance of
itself started, which its sandbox forbids signalling (research R-095). `FailureReason` and the exit
codes are unchanged. What differs is what the
command line says when run from the App Store app's executable, which runs in the same sandbox as
the app and so reaches the same daemon.

## `service`

`service stop` asks the running daemon to stop through `StopDaemon`, exiting 0, or 3 when no
daemon answers, since the window's Quit can stop it and the command line must be able to do what
the window does (FR-039c). Every other `service` subcommand exits with code 4, "unavailable", and
prints:

```text
the App Store build is started by Midi Harbor itself; turn on "Start at login" in its settings
```

## `capabilities`

```text
service installation   no   this build does not include it
```

and in `--json` output, `"available": false` with `"reason": "this build does not include it"`,
the existing `NotBuilt` reason.

## `config path`

Prints the file inside the container:

```text
~/Library/Containers/com.mrgeckosmedia.MidiHarbor/Data/Library/Application Support/midi-harbor/config.yaml
```

## `daemon`

Unchanged, but when sandboxed it listens on `$TMPDIR/daemon.sock`, the container's own `tmp`
(research R-096). `--socket` still overrides it.

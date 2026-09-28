# Data Model: Resilience

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## ConnectionState

The lifecycle shared by every endpoint (FR-044). This is the heart of Principle I.

```
ConnectionState {
    phase:       ConnectionPhase
    since:       Timestamp
    last_error:  Option<FailureReason>
    attempt:     u32
    next_retry:  Option<Timestamp>
}
```

### State machine

```
                  ┌────────────────┐
                  │   Disabled     │ ◄──── user disables (any state)
                  └───────┬────────┘
                          │ user enables
                          ▼
   ┌──────────────► Disconnected ──────────────┐
   │                      │                    │
   │                      │ start              │
   │                      ▼                    │
   │                 Connecting ───────────────┤ permanent failure
   │                      │                    │
   │           success    │      failure       ▼
   │                      ▼              Unavailable
   │                 Connected                 │
   │                      │                    │ condition clears
   │      loss detected   │                    │
   │                      ▼                    │
   └───────────────── Retrying ◄───────────────┘
                     (backoff + jitter)
```

**Transition rules**:

- `Retrying → Connecting` fires on backoff expiry, on a platform wake/network-change hint, or on
  user request. Backoff is exponential with jitter, capped at a bounded maximum, and **never
  terminates** while the endpoint is enabled (FR-023, Principle I).
- `Unavailable` is the *only* non-transient non-user stop, and requires a `FailureReason` that is
  actionable (FR-028): no Bluetooth adapter, permission denied, port name conflict, device claimed
  by another application. It is re-evaluated when the underlying condition may have changed — it is
  not terminal.
- Every transition out of `Connected` triggers note-silencing on that endpoint's destinations
  (FR-026, FR-015f).
- Every transition into `Connected` after a loss triggers state restoration (FR-027).
- Every transition emits an `Event` (Principle VII).

`FailureReason` is a closed enum, not a string, so the UI can render specific guidance and the CLI
can exit with meaningful codes. Variants include `NetworkUnreachable`, `PeerTimeout`,
`PeerRejected`, `DeviceRemoved`, `DeviceClaimed`, `PermissionDenied`, `AdapterUnavailable`,
`NameConflict`, `ResourceLimit`, `ProtocolError`, and `ConfigInvalid`.

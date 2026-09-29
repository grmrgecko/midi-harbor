# Contract: Daemon gRPC Protocol

**Feature**: 001-service-and-clients | **Protocol**: `midiharbor.v1` | **Date**: 2026-09-20

The contract between `midi-harbor daemon` and its clients (the GUI and the CLI). Governed by
FR-036, FR-037, FR-039c, FR-040, FR-041 and Constitution Principle II.

**Revised 2026-09-20**: gRPC replaces the original length-delimited JSON framing. See
[research.md](../research.md) R-009 for the decision and what it changes.

---

## 1. Transport

- **Protocol**: gRPC over HTTP/2, using `tonic`.
- **Channel**: a **Unix domain socket**, not a TCP port. Nothing about Midi Harbor's control plane
  should be reachable from the network, and a local socket gets per-user isolation from file
  permissions with no authentication layer to design.
- **Path**:
  - Linux: `$XDG_RUNTIME_DIR/midi-harbor/daemon.sock`, falling back to
    `$HOME/.cache/midi-harbor/daemon.sock` when `XDG_RUNTIME_DIR` is unset.
  - macOS: `$HOME/Library/Application Support/com.mrgeckosmedia.MidiHarbor/daemon.sock`.
- **Permissions**: socket file mode `0600`, parent directory `0700`.
- **Encoding**: protocol buffers, `proto3`.
- **Schema location**: `proto/midiharbor/v1/harbor.proto`, compiled by `tonic-prost-build` into
  the `midi-harbor-ipc` crate. The `.proto` file is the contract; the Rust types are generated
  from it and are never hand-edited.
- **Concurrency**: HTTP/2 multiplexes many concurrent calls over one connection, and many clients
  may connect at once (FR-040). No call blocks another.

---

## 2. Versioning (FR-041)

Version lives in the **proto package name**: `midiharbor.v1`.

- **Major version = new package.** A breaking change means `midiharbor.v2`, a new service path,
  and a daemon that may serve both during a migration. A client built against `v1` calling a
  `v2`-only daemon gets gRPC `UNIMPLEMENTED` for the whole service, which is unambiguous.
- **Minor version = additive only.** New RPCs, new message fields, new enum values. Protobuf
  ignores unknown fields on the wire, so an older client talking to a newer daemon keeps working
  without any negotiation step.
- **Clients still call `GetServerInfo` first**, because `UNIMPLEMENTED` alone does not tell a user
  *which* side is old:

```protobuf
message ServerInfo {
  string daemon_version  = 1;  // "0.1.0"
  uint32 protocol_major  = 2;  // 1
  uint32 protocol_minor  = 3;  // 0
  google.protobuf.Timestamp started_at = 4;
  string config_path     = 5;
}
```

A client whose major version differs from the daemon's MUST refuse to proceed and say which
component to update, naming both versions. This is the one place the product must not present a
generic connection error.

**Never reuse a field number.** Removing a field means reserving its number, so a future field
cannot silently inherit an old meaning.

---

## 3. Service surface

One service, `Harbor`. Every RPC operates on live daemon state, so the GUI and CLI are
interchangeable (FR-039c).

### Queries

| RPC | Request → Response |
|---|---|
| `GetServerInfo` | `GetServerInfoRequest` → `ServerInfo` |
| `ListEndpoints` | `ListEndpointsRequest` (optional kind filter) → `ListEndpointsResponse` |
| `GetEndpoint` | `GetEndpointRequest` → `Endpoint` |
| `ListRoutes` | `ListRoutesRequest` → `ListRoutesResponse` |
| `ListPeers` | `ListPeersRequest` → `ListPeersResponse` |
| `ListEvents` | `ListEventsRequest` (since, limit, endpoint) → `ListEventsResponse` |
| `GetCapabilities` | `GetCapabilitiesRequest` → `GetCapabilitiesResponse` |
| `GetStatus` | `GetStatusRequest` → `Status` |

### Mutations

| Group | RPCs |
|---|---|
| Virtual ports | `CreateVirtualPort`, `RenameEndpoint`, `DeleteVirtualPort`, `SetEndpointEnabled` |
| Network sessions | `CreateNetworkSession`, `ConnectPeer`, `AddManualPeer`, `DisconnectPeer`, `RemovePeer`, `RespondToInvitation`, `SetInvitationPolicy` |
| Physical devices | `ForgetPhysicalDevice`, `ResolveAmbiguousDevice` |
| Bluetooth | `StartBluetoothScan`, `StopBluetoothScan`, `ConnectBluetoothDevice`, `DisconnectBluetoothDevice`, `ForgetBluetoothDevice`, `SetPeripheralAdvertising` |
| Routes | `CreateRoute`, `DeleteRoute`, `SetRouteEnabled` |
| Configuration | `ExportConfiguration`, `ImportConfiguration`, `ReloadConfiguration` |
| Diagnostics | `ExportDiagnostics` |

Physical devices are discovered, never created, so no `CreatePhysicalDevice` exists.

`RenameEndpoint` without `confirm = true` fails `FAILED_PRECONDITION` carrying the applications
that may need to reselect the port (FR-001 scenario 4).

`CreateRoute` succeeds even when it forms a cycle, returning the affected routes marked
`LOOP_DETECTED` so the client can warn (FR-033).

### Server-streaming RPCs

Streams replace the original topic-subscription mechanism. A client opens the streams it needs.

| RPC | Yields | Delivery |
|---|---|---|
| `WatchState` | `StateEvent` — endpoint added/removed/changed, route validity, capabilities | **Lossless** |
| `WatchEvents` | `Event` — mirrors the bounded history | **Lossless** |
| `WatchInvitations` | `Invitation` — requires a `RespondToInvitation` (FR-014) | **Lossless** |
| `WatchTraffic` | `TrafficUpdate` — coalesced counters | **Lossy**, see below |
| `MonitorEndpoint` | `MidiMessage` — decoded messages on one endpoint (FR-047) | **Lossy**, see below |

---

## 4. Backpressure (Principle III)

gRPC has HTTP/2 flow control, which means a slow client can, by default, apply backpressure all
the way back to the producer. **On the two high-rate streams that is exactly the wrong behaviour**:
a stalled GUI must never slow down MIDI delivery.

So the daemon applies an explicit policy per stream:

- **`WatchTraffic` and `MonitorEndpoint` are lossy by contract.** Each subscriber gets a bounded
  channel. When it is full the daemon **drops the update and increments a counter** rather than
  awaiting capacity. The next message delivered carries `dropped: uint64` so the client can show
  that it fell behind. The daemon never blocks and never grows a buffer.
- **`WatchState`, `WatchEvents` and `WatchInvitations` are lossless.** These are low-rate and
  correctness-relevant. A client that cannot keep up with them is disconnected with
  `RESOURCE_EXHAUSTED` rather than having its view silently diverge.
- **`WatchTraffic` coalesces** to at most one update per endpoint per 250 ms. Counter updates are
  never emitted per-message.

Serialisation for every stream happens on ordinary async tasks reading atomics and ring buffers.
No gRPC code runs on the MIDI data path.

---

## 5. Errors

gRPC status codes carry the class of failure; the domain reason rides along in the details.

| Condition | Status code |
|---|---|
| Endpoint, route, peer or device not found | `NOT_FOUND` |
| Name already in use, duplicate route | `ALREADY_EXISTS` |
| Confirmation required and not supplied | `FAILED_PRECONDITION` |
| Capability unavailable on this system | `UNAVAILABLE` |
| Malformed request, invalid name or port | `INVALID_ARGUMENT` |
| Client speaks a major version the daemon does not serve | `UNIMPLEMENTED` |
| Lossless-stream client fell too far behind | `RESOURCE_EXHAUSTED` |

Every error additionally carries a trailing metadata entry `harbor-reason` holding the stable slug
from `FailureReason::code()` — `name_conflict`, `permission_denied`, `adapter_unavailable`, and so
on. **Clients switch on that slug, never on the message text**, and the CLI derives its exit codes
from it. The status message itself is human-readable and may change between releases.

Capabilities work the same way: `Capability.id` (such as `bluetooth_central`) is stable and is
what a client decides from, and `Capability.name` is for people.

When the user can do something about the failure, the error also carries `harbor-guidance-bin`:
UTF-8 text saying what, from `FailureReason::guidance()`. It is binary metadata because guidance
names things the user chose, which need not be ASCII. Like the message, it is for people and may
change; clients show it and never parse it.

This keeps one mapping — `FailureReason` → slug → gRPC status → CLI exit code — with the closed
enum in `midi-harbor-core` as its single source of truth.

---

## 6. Behavioural guarantees

1. **The daemon never blocks the MIDI data path on IPC.** Serialisation reads atomics and ring
   buffers from ordinary tasks (Principle III).
2. **State is authoritative in the daemon.** Clients hold a cache fed by `WatchState` and must
   tolerate re-syncing at any time via `ListEndpoints`.
3. **Connecting or disconnecting a client never disturbs a connection** (FR-037). No resource in
   this protocol is scoped to a client's lifetime.
4. **Requests are idempotent where they can be.** `SetEndpointEnabled` to the current value
   succeeds as a no-op; `DeleteRoute` on an absent route returns `NOT_FOUND`, not a broken state.
5. **No RPC blocks indefinitely.** Scanning and connecting return as soon as the state machine
   accepts the request, with the endpoint in `CONNECTING`; progress arrives on `WatchState`.
6. **The `.proto` file is the contract.** Changing it is a contract change and requires a version
   decision, whether or not any Rust signature changes.

---

## 7. Why gRPC, and what it costs

The upsides that decided it: a schema-first contract that cannot drift from the implementation,
generated clients in any language, server streaming with real flow control instead of hand-rolled
subscriptions, and no bespoke framing code to get wrong.

The costs, accepted deliberately:

- **A build-time protobuf compilation step**, which `tonic-prost-build` handles with a vendored
  `protoc`, so contributors need no system package.
- **A heavier dependency tree** — `tonic`, `prost`, `hyper`, `tower` — in the headless build,
  which was a point in favour of the original hand-rolled JSON. Measured against the alternative
  of maintaining our own framing, versioning and streaming semantics, this is the better trade.
- **The wire is no longer human-readable.** `grpcurl` against the socket, plus the `--json` CLI
  output, cover the debugging need that plain JSON would have given for free.
- **Flow control has to be actively overridden** on the two lossy streams, as §4 sets out. This is
  the one place gRPC's defaults are wrong for this product, and it is a deliberate override rather
  than an oversight.

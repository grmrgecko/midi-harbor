# Data Model: Midi Harbor

**Feature**: 001-service-and-clients | **Date**: 2026-09-20

This document defines the domain entities, their fields, relationships, validation rules, and
state transitions. It is the vocabulary shared by the daemon, the IPC contract, the CLI, and the
GUI. Types live in `midi-harbor-core` unless noted.

---

Each endpoint kind, peers, routes, connection state, counters, events, the configuration and
capabilities are described in the spec they belong to.

---

## 1. Identity

Three different notions of identity must not be confused.

| Identity | Scope | Stability | Purpose |
|---|---|---|---|
| `EndpointId` | Midi Harbor's config | Permanent once assigned | What routes reference |
| `PlatformHandle` | One daemon run | Lost on restart or replug | What the OS API needs |
| `DeviceFingerprint` | Physical hardware | Survives unplug/replug | Re-binds hardware to its `EndpointId` |

**`EndpointId`**: an opaque UUID assigned at creation and never reused. Renaming an endpoint does
not change it (FR-004). This is the only identifier persisted in routes.

**`DeviceFingerprint`**: a composite key for physical hardware (FR-015e, R-013), carrying a
confidence level:

```
DeviceFingerprint {
    unique_id:     Option<i32>      // CoreMIDI kMIDIPropertyUniqueID
    usb_serial:    Option<String>   // Linux, from sysfs, when reported
    manufacturer:  Option<String>
    model:         Option<String>
    name:          String           // always present, never sufficient alone
    topology_path: Option<String>   // stable per physical socket, not across sockets
}
```

`MatchConfidence` is `Exact` (unique id or USB serial matched), `Probable` (manufacturer + model +
name matched), or `Ambiguous` (name only, or several candidates matched). Only `Exact` and
`Probable` auto-rebind routes; `Ambiguous` surfaces a choice to the user rather than guessing
(R-013, FR-015g).

---

## 2. Endpoint

The central entity. Everything MIDI flows to or from is an `Endpoint` (FR-015b).

```
Endpoint {
    id:            EndpointId
    name:          String           // user-facing, editable
    kind:          EndpointKind
    enabled:       bool             // user intent, persisted
    state:         ConnectionState  // runtime, not persisted
    direction:     Direction        // Input | Output | Bidirectional
    counters:      TrafficCounters  // runtime
    created_at:    Timestamp
}
```

`EndpointKind` is a tagged union carrying kind-specific data:

- `VirtualPort(VirtualPort)`
- `NetworkSession(NetworkSession)`
- `PhysicalDevice(PhysicalDevice)`
- `BluetoothDevice(BluetoothDevice)`

**Validation**:

- `name` is 1–128 characters, no control characters, trimmed of surrounding whitespace.
- `name` must be unique among endpoints of the same kind (FR-005). Virtual ports and network ports
  share one namespace, because other applications see a network port as a port of its name
  (FR-015h). Names may otherwise collide *across* kinds, since the platform namespaces them
  separately.
- `enabled` is user intent and is independent of `state`. A disabled endpoint is never connected;
  an enabled endpoint may still be `Disconnected` or `Retrying`.

---

## 3. Endpoint kinds

---

## Entity relationships

```
Configuration 1───* Endpoint
Endpoint      1───1 EndpointKind  (VirtualPort | PhysicalDevice | NetworkSession | BluetoothDevice)
Endpoint      1───1 ConnectionState
Endpoint      1───1 TrafficCounters
Endpoint      1───* Event
Route         *───1 Endpoint  (as source)
Route         *───1 Endpoint  (as destination)
NetworkSession 0───1 Peer
PhysicalDevice 1───1 DeviceFingerprint
```

**At runtime**, routes reference endpoints by `EndpointId`, never by platform handle — which is why
replugging and rebooting leave routes intact.

**In the stored file**, routes reference endpoints by *name*, so the document can be edited by
hand (R-011). Renaming an endpoint through the daemon rewrites every route naming it in the same
operation, so connections survive renames as FR-004 requires. A name matching nothing leaves the
route visible and marked broken (FR-035) rather than silently dropped.

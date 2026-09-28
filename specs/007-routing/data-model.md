# Data Model: Routing

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## Route (FR-030..035, FR-030a, FR-030b)

```
Route {
    id:          RouteId
    source:      EndpointId
    destination: EndpointId
    enabled:     bool
    validity:    RouteValidity
    counters:    TrafficCounters
}
```

`RouteValidity` is `Valid`, `Broken { missing: Vec<EndpointId> }` (FR-035), or
`LoopDetected { cycle: Vec<RouteId> }` (FR-033).

**Validation**:

- `source` must not equal `destination`.
- `source` must have an input side; `destination` must have an output side.
- Duplicate (source, destination) pairs are rejected — enable/disable the existing route instead.
- The route graph is checked for cycles on every mutation. A cycle does not block creation but
  marks affected routes `LoopDetected` and warns (FR-033).

**Loop suppression** carries a per-message `OriginTag` through the router, and network sessions
carry a session identifier so a receiver recognises MIDI it originally sent — the only way to
catch loops formed across two machines (R-013, edge case).

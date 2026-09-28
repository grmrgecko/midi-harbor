# Data Model: Observability

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## TrafficCounters (FR-045)

```
TrafficCounters {
    messages_sent:      u64
    messages_received:  u64
    bytes_sent:         u64
    bytes_received:     u64
    messages_lost:      u64    // detected via RTP sequence gaps
    messages_recovered: u64    // restored from the recovery journal
    messages_dropped:   u64    // ring buffer overflow — our own backpressure
    last_activity:      Option<Timestamp>
}
```

All fields are atomics incremented on the real-time path with no logging or allocation
(Principle III). `messages_dropped` is deliberately distinct from `messages_lost`: one is our
backpressure, the other is the network's.

---

## Event (FR-046, Principle VII)

```
Event {
    id:        EventId        // monotonic
    at:        Timestamp
    endpoint:  Option<EndpointId>
    route:     Option<RouteId>
    severity:  Severity       // Info | Warning | Error
    kind:      EventKind       // stable, machine-readable name
    detail:    String          // human-readable
}
```

Retained in a bounded in-memory ring (default 10,000 events) that survives GUI restarts because it
lives in the daemon (FR-046). Not persisted across daemon restarts — deliberately, to bound disk
use; the diagnostic export (FR-048) is the durable artifact.

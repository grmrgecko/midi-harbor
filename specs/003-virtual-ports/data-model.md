# Data Model: Virtual Ports

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## VirtualPort (FR-001..007)

```
VirtualPort {
    platform_unique_id: Option<i64>   // assigned by the OS, re-requested on recreate
}
```

Fully user-defined and fully persisted. Created on daemon startup, destroyed on shutdown.
Deleting one requires silencing sounding notes first (FR-006).

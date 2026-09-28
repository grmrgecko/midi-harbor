# Data Model: Hardware Devices

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## PhysicalDevice (FR-015a..g)

```
PhysicalDevice {
    fingerprint: DeviceFingerprint
    present:     bool
    confidence:  MatchConfidence
    claimed_by:  Option<String>   // another application holds it exclusively
}
```

Discovered, never created by the user. A `PhysicalDevice` entry persists in config while the
hardware is absent so its routes survive (FR-015d); `present: false` drives the UI. Devices never
seen before appear at runtime without a config entry until a route references them.

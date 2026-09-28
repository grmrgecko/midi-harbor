# Data Model: Configuration

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## Configuration (FR-049..053)

The persisted root. YAML, atomically written, schema-versioned (R-011).

```
Configuration {
    schema_version:  u32
    preferences:     Preferences
    endpoints:       Vec<Endpoint>      // every kind, tagged by EndpointKind
    peers:           Vec<PeerConfig>
    routes:          Vec<RouteConfig>
}
```

**Revised during implementation (2026-09-20).** The original sketch split endpoints into four
parallel lists — `virtual_ports`, `network_sessions`, `physical_devices`, `bluetooth`. They were
collapsed into one `endpoints` list because `Endpoint` already carries a kind tag and all four
share every other field. Four lists would have meant four parallel code paths for validation,
lookup, renaming and conflict detection, with no gain: the TOML stays readable because each entry
carries `kind = "virtual_port"` explicitly.

**What is persisted vs. runtime**: `enabled`, names, ids, fingerprints, policies and routes are
persisted. `ConnectionState`, `TrafficCounters`, `Event`s, discovered `Peer`s and `rssi` are
runtime-only. This split is what lets FR-050 apply config changes by diffing persisted state and
touching only the affected connections.

---

## Capability (FR-053)

```
Capability {
    name:      CapabilityName
    available: bool
    reason:    Option<UnavailableReason>
}
```

Queried at runtime so the UI renders unavailable features as unavailable rather than broken
(Principle IV). Names cover `VirtualPorts`, `NetworkSessions`, `BluetoothCentral`,
`BluetoothPeripheral`, `ServiceManager`, and `MdnsResponder`. This is the mechanism by which a
documented platform limitation — such as the Bluetooth peripheral role on Windows — is
communicated honestly instead of failing at use.

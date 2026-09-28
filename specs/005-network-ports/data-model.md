# Data Model: Network Ports

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## NetworkSession (FR-008..015)

```
NetworkSession {
    local_name:         String          // what we advertise (FR-009)
    control_port:       u16             // data port is control_port + 1
    peer:               Option<PeerRef>
    invitation_policy:  InvitationPolicy
    sync:               Option<ClockSync>
    journal_stats:      JournalStats
    direction_policy:   SessionDirection
}
```

`InvitationPolicy` is `Prompt` (default), `AcceptKnown`, `AcceptAll`, or `RejectAll` (FR-014).
`ClockSync` carries the estimated offset, round-trip latency, and time of last successful exchange
— the last of which is the authoritative liveness signal (R-010).

`JournalStats` carries `messages_recovered`, `journal_bytes`, and `highest_seq_acked`, exposing
FR-045's "lost" and "recovered" counters.

---

## Peer

A discovered or manually added remote party (FR-008, FR-010).

```
Peer {
    id:            PeerId
    advertised_name: String
    addresses:     Vec<SocketAddr>   // may be several; may change over time
    source:        PeerSource        // Discovered | Manual
    trusted:       bool              // "always accept from this peer"
    last_seen:     Option<Timestamp>
    is_self:       bool              // never offered to the user (edge case)
}
```

**Validation**: a peer whose `addresses` is empty and whose `source` is `Manual` is invalid.
Discovered peers may briefly have no address while mDNS resolution is pending. Two peers
advertising the same name are kept distinct by `PeerId` and disambiguated in the UI by address
(edge case: duplicate peer names).

# Quickstart: Network Ports

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 2 — Network session self-heals (US3 · SC-003, SC-004, SC-005, SC-011)

The headline capability. Requires a second machine.

```bash
midi-harbor session create "Studio"
midi-harbor session discover                # expect: the peer machine listed by advertised name
midi-harbor session connect Studio <peer>
midi-harbor status                          # expect: Connected
```

**Interoperability check (SC-011)**: with the peer running Apple's Network MIDI rather than Midi
Harbor, confirm discovery and connection work with no manual configuration on either side.

Now induce each failure in turn, and after each one run `midi-harbor status` and confirm MIDI
resumes flowing **with no user action**:

| Failure | How to induce | Expected recovery |
|---|---|---|
| Network interruption | Unplug the cable or disable Wi-Fi for 30 s | Reconnects within 10 s of restoration (SC-003) |
| Sleep / wake | Sleep the machine, wake it | Reconnects within 15 s of wake (SC-004) |
| Peer restart | Reboot the other machine | Keeps retrying, reconnects when the peer returns |
| Wi-Fi roam | Move between access points so the address changes | Re-establishes at the new address |

Time each recovery from the history, not a stopwatch. Leave `midi-harbor events --follow` running
on both machines while inducing each failure: the TIME column gives the moment the session was
lost and the moment it connected again. For sleep, measure from the `system_resumed` event.

**Stuck-note check (SC-005)**: hold a chord down on the sending side, cut the network mid-chord,
restore it. **No note may be left sounding** on the receiving side after recovery.

**Packet-loss check (SC-006)** — this is what the recovery journal is for:

```bash
# Linux, on the sending machine:
sudo tc qdisc add dev <iface> root netem loss 5%
# ...play notes and move controllers for 60 seconds...
sudo tc qdisc del dev <iface> root netem
```

**Pass criteria**: no note left sounding, and controller values at the receiver match the sender
within 1 second of the loss ending. Confirm the journal did the work:

```bash
midi-harbor status --json | jq '.detail[] | select(.kind == "network") | {name, lost, recovered}'
```

`recovered` must be non-zero. If it is zero while `lost` is non-zero, the journal is not
functioning regardless of what the audible result seemed to be.

**Soak test (SC-007)**: leave the session carrying traffic for 24 hours. Expect zero unrecovered
disconnections in `midi-harbor events`.

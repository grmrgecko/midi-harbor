# Quickstart: Bluetooth MIDI

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 5 — Bluetooth in both directions (US6)

**Central** — connect out to a device:

```bash
midi-harbor bluetooth scan
midi-harbor bluetooth connect "<device>"
midi-harbor route create "<device>" "Sequencer Bus"
```

**Range check**: walk the device out of range and back. Expect automatic reconnection with no
stuck notes (FR-020).

**Peripheral** — advertise this computer:

```bash
midi-harbor bluetooth advertise on --name "Midi Harbor"
```

**Expected**: a phone or tablet scanning for BLE MIDI finds "Midi Harbor" and can connect and send.

**Unavailable path**: turn Bluetooth off entirely.

```bash
midi-harbor bluetooth scan                  # expect exit 4 with a specific reason
midi-harbor port list                       # expect: still works — unrelated features unaffected
```

**Pass criteria**: unavailability is reported as unavailable, not as a failure, and does not
affect anything else (FR-021, FR-053).

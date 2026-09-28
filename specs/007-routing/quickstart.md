# Quickstart: Routing

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 3 — Physical device repeated over the network (US4 · SC-010a, SC-010b, SC-010c)

The repeater. Requires a physical MIDI device and the session from scenario 2.

```bash
# Plug the device in — no command needed.
midi-harbor device list                     # expect: appears within 2 seconds (SC-010a)
midi-harbor route create "<device>" "Studio"
```

On the **remote** machine, complete the repeater in the other direction:

```bash
midi-harbor route create "Studio" "<local hardware output>"
```

**Expected**: playing the device drives the sound module attached to the other machine.

**Hot-plug check (SC-010c)**: unplug the device, wait, plug it back in — including into a
*different* USB port.

```bash
midi-harbor route list                      # expect: route Valid again, not Broken
```

**Pass criteria**: the route resumes automatically without being recreated, and no note is left
sounding on the remote machine (FR-015d, FR-015f). Repeat 100 times for SC-010c.

**Loop check**: create the reverse route on both machines so MIDI would circulate. Expect a warning
naming the cycle and no unbounded message multiplication (FR-033).

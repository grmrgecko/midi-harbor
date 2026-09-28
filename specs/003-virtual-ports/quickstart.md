# Quickstart: Virtual Ports

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 1 — Virtual ports survive a reboot (US1, US2 · SC-001, SC-002, SC-014c)

Proves the P1 slice: the IAC replacement, the service, and persistence.

```bash
midi-harbor service install --start
midi-harbor service status                  # expect: installed, running, version, uptime
midi-harbor port create "Sequencer Bus"
midi-harbor port list
```

**Expected**: "Sequencer Bus" appears within seconds in the MIDI device list of any other
application — Audio MIDI Setup on macOS, `aconnect -l` on Linux — as both an input and an output,
without restarting that application.

Verify data actually flows, using two terminals:

```bash
midi-harbor monitor "Sequencer Bus"         # terminal 1
# terminal 2: send notes from any MIDI application or utility into the port
```

**Then reboot, log in, and without opening anything:**

```bash
midi-harbor port list                       # expect: "Sequencer Bus" present and connected
```

**Pass criteria**: port present with the same name and id, no user action beyond logging in
(SC-002). Total elapsed time from a fresh binary to a working port under 30 seconds (SC-001).

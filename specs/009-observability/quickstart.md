# Quickstart: Observability

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 4 — Diagnosing without log files (US5 · SC-012, SC-013)

```bash
midi-harbor status --watch                  # live phase, time in phase, counters
midi-harbor events --follow                 # state transitions as they happen
```

Induce a failure, then **close every window and terminal**, wait, and reopen:

```bash
midi-harbor events                          # the latest 50, each with the local time it happened
```

**Pass criteria**: the failure and the recovery are both visible with timestamps, proving the
history lives in the daemon and survived the client going away (FR-046, SC-013).

Try a genuinely permanent failure — create a port whose name collides with an existing one:

```bash
midi-harbor port create "Sequencer Bus"     # expect exit 6, message naming the conflict
```

**Pass criteria**: a specific, actionable reason, never a generic failure (SC-012).

```bash
midi-harbor diagnostics export --output /tmp/report.json
```

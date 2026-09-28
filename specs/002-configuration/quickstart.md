# Quickstart: Configuration

Prerequisites and the build are in [001's
quickstart](../001-service-and-clients/quickstart.md#prerequisites), with the automated equivalent
of each scenario.

---

## Scenario 8 — Configuration portability (SC-015)

```bash
midi-harbor config export --output setup.yaml
# move setup.yaml to a machine running the other operating system
midi-harbor config import setup.yaml --mode replace --yes
midi-harbor status
```

**Pass criteria**: the same endpoints and routes exist, with platform-specific items reported
through the capability query rather than failing (FR-053).

**Corruption handling**: truncate the config file mid-document and restart the daemon. Expect the
service to start with defaults, preserve the unreadable file alongside, and report what happened
(FR-051) — never to silently lose the setup.

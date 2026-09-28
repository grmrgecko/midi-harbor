# Feature Specification: Configuration

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: The configuration file: where it lives, its shape for editing by hand, writing it safely,
applying changes to running connections, recovering from a file that cannot be read, export and
import between machines, importing Apple's own MIDI setup, and the report of which capabilities this
machine has.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### Edge Cases

- **Configuration corruption**: When the stored configuration cannot be read, the service starts
  with defaults, preserves the unreadable file, and reports what happened rather than losing the
  user's setup silently.

## Requirements *(mandatory)*

### Functional Requirements

#### Configuration

- **FR-049**: System MUST store configuration in a documented, human-readable file in a standard
  per-user location on each platform.
- **FR-050**: System MUST apply configuration changes to running connections without requiring a
  restart, disturbing only the connections actually affected by the change.
- **FR-051**: System MUST start with working defaults and preserve the original file when stored
  configuration cannot be read, reporting the problem to the user.
- **FR-052**: Users MUST be able to export and import their configuration, so a setup can be moved
  between computers.
- **FR-053**: System MUST report, at runtime, which capabilities are available on the current
  computer, so the interface can present unavailable capabilities as unavailable rather than
  broken.

### Key Entities

- **Configuration**: The persisted set of virtual ports, network sessions, known peers, Bluetooth
  pairings, routes, and preferences that defines the user's setup.
- **Capability**: A statement of whether a given function is available on this computer right now,
  and if not, why.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-015**: The same setup, exported from one computer and imported on another of the other
  supported operating system, produces the same endpoints and routes.

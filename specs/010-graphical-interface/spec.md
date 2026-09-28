# Feature Specification: Graphical Interface

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: The graphical interface: a libcosmic window that is a client of the daemon like the
command line, the onboarding that installs the service, and the redesigned window.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## Requirements *(mandatory)*

The window has no requirements of its own. It is one of the daemon's two clients, and what it
shows and does is required by the spec each capability is in. Those it answers most directly:

- FR-039b, that the window is optional and a build can leave it out, in 001.
- FR-039c and SC-014b, that everything the window does the command line does too, in 001.
- FR-042, offering to install and start the service when it is not running, in 001.
- SC-014, that opening and closing the window disturbs no connection, in 001.
- SC-001, a virtual port created and seen by another application in under 30 seconds, in 003.
- FR-044 to FR-047, state, counters, history and the live monitor, in 009.

The layout the owner chose, and what it asked of the daemon, is research R-078.

## Assumptions

- Only one graphical interface window per computer is expected in normal use, though the service
  must tolerate several clients.

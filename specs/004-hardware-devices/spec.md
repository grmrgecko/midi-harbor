# Feature Specification: Hardware Devices

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: MIDI hardware attached to the computer, and ports other applications provide: finding it,
following it as it is plugged in and out, recognising it again after a replug, telling identical
devices apart, and a device another application holds. Carrying hardware over the network is user
story 4, in [007-routing](../007-routing/spec.md).

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### Edge Cases

- **Device claimed by another application**: When a physical MIDI device is held exclusively by
  another application, Midi Harbor reports it as in use by another application rather than failing
  silently, and claims it when it becomes free.
- **Identical devices**: When two devices reporting the same name are attached at once, they are
  distinguished in the interface so routes address the right one.
- **Device removed mid-message**: When a device is unplugged partway through a system-exclusive
  transfer, the partial message is discarded rather than forwarded as corrupt data.

## Requirements *(mandatory)*

### Functional Requirements

#### Physical MIDI devices

- **FR-015a**: System MUST automatically discover and list physical MIDI hardware attached to the
  computer — USB MIDI interfaces, DIN interfaces, controllers, and keyboards — presenting each with
  its hardware-reported name.
- **FR-015b**: System MUST expose each physical MIDI input and output as an endpoint usable as a
  route source or destination, on equal terms with virtual ports, network sessions, and Bluetooth
  devices.
- **FR-015c**: System MUST detect physical devices being attached and removed while running, and
  update the endpoint list without the user refreshing anything or restarting the service.
- **FR-015d**: System MUST retain the configuration of routes referencing a physical device while
  that device is unplugged, and resume them automatically when the same device is reattached.
- **FR-015e**: System MUST identify a physical device stably enough that reattaching the same
  device — including to a different port on the same computer — restores its routes rather than
  presenting it as a new device.
- **FR-015f**: System MUST silence sounding notes at the destinations of a route when that route's
  physical source device is removed.
- **FR-015g**: System MUST distinguish two physically identical devices attached at the same time,
  so the user can tell them apart and route them separately.

### Key Entities

- **Physical MIDI Device**: An endpoint representing MIDI hardware attached to this computer. Has
  a hardware-reported name, a stable identity that survives being unplugged and reattached, a
  present/absent status, and separate input and output sides.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-010a**: A physical MIDI device plugged in while the service is running appears as a routable
  endpoint within 2 seconds, with no user action.
- **SC-010c**: Across 100 unplug-and-replug cycles of a routed physical device, 100% of routes
  resume automatically and zero notes are left sounding.

## Assumptions

- Physical MIDI devices are accessed through the operating system's own MIDI layer, so any device
  the operating system already recognises is usable; vendor-specific drivers are out of scope.

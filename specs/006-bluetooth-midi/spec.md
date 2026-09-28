# Feature Specification: Bluetooth MIDI

**Status**: Implemented

**Created**: 2026-09-20

**Scope**: Bluetooth LE MIDI in both roles: connecting out to devices, and advertising this computer
as one; pairing memory, device timestamps, and saying why Bluetooth is unavailable.

Split out of the original Midi Harbor specification on 2026-09-27. Requirement, success criterion,
research and task numbers are the ones the original gave them, and the code cites them by number;
[the index](../README.md) says which spec holds each. The architecture, the daemon contract and the
command-line contract are in [001-service-and-clients](../001-service-and-clients/).

## User Scenarios & Testing *(mandatory)*

### User Story 6 - Bluetooth LE MIDI in both directions (Priority: P5)

A user has a Bluetooth MIDI keyboard they want to play into their DAW, and separately wants their
tablet to be able to send MIDI to this computer over Bluetooth. Midi Harbor scans for and connects
to the keyboard, and can also advertise this computer so the tablet can find and connect to it.

**Why this priority**: Bluetooth is valuable but affects fewer setups than local and network MIDI,
and is the most hardware-dependent surface. It is last so the reliable core ships first.

**Independent Test**: With a BLE MIDI peripheral present, scan, connect, and verify MIDI arrives;
separately, enable advertising and verify another device can discover and connect to this
computer and send MIDI to it.

**Acceptance Scenarios**:

1. **Given** Bluetooth is available, **When** the user scans, **Then** nearby Bluetooth MIDI
   devices are listed by name and signal strength.
2. **Given** a discovered device, **When** the user connects to it, **Then** it becomes available
   as an endpoint that can be routed like any other.
3. **Given** a connected Bluetooth device, **When** it moves out of range and later returns,
   **Then** it reconnects automatically without user action and without leaving notes sounding.
4. **Given** a connected Bluetooth device, **When** the user plays it, **Then** the timing of the
   received messages reflects the timestamps the device reported rather than arrival time alone.
5. **Given** the user enables advertising, **When** another device scans for Bluetooth MIDI,
   **Then** this computer appears under its configured name and can be connected to.
6. **Given** no Bluetooth adapter is present or Bluetooth is off, **When** the user opens the
   Bluetooth section, **Then** it is shown as unavailable with the reason, and the rest of the
   application continues to work normally.
7. **Given** the operating system has not granted Bluetooth permission, **When** the user first
   attempts to scan, **Then** they are told permission is required and how to grant it.

---

## Requirements *(mandatory)*

### Functional Requirements

#### Bluetooth LE MIDI

- **FR-016**: System MUST scan for and list nearby Bluetooth LE MIDI devices with their names.
- **FR-017**: System MUST connect to a selected Bluetooth LE MIDI device and expose it as an
  endpoint usable in routes.
- **FR-018**: System MUST be able to advertise this computer as a Bluetooth LE MIDI peripheral
  under a user-configurable name, so other devices can connect to it.
- **FR-019**: System MUST apply the timestamps reported by Bluetooth MIDI devices when ordering and
  delivering received messages.
- **FR-020**: System MUST remember paired Bluetooth devices and reconnect to them automatically
  when they become available again.
- **FR-021**: System MUST report Bluetooth as unavailable with a specific reason when no adapter is
  present, the adapter is off, or permission has not been granted, without affecting other
  features.

### Key Entities

- **Bluetooth Device**: An endpoint representing a Bluetooth LE MIDI device this computer connects
  to, or a remote device connected to this computer while it is advertising. Has a device identity,
  a name, and a pairing status.

## Assumptions

- Bluetooth requires an adapter and operating-system permission that Midi Harbor cannot grant
  itself; the product's obligation is to explain clearly what is needed.

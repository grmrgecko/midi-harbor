# Specifications

One spec per capability, numbered by topic. Each holds its user stories, requirements, success
criteria, research and tasks; the architecture, the daemon contract and the command-line contract
are in 001, and every spec follows [the constitution](../.specify/memory/constitution.md).

Requirement (FR), success criterion (SC), research (R) and task (T) numbers are unique across
every spec, and the code cites them by number. They were given in the order the work was done,
so one spec's numbers are not contiguous; the lists below say which spec holds each. The one
exception is 014, whose task list started again at T001: its T001 to T034 are cited as 014's.

| Spec | What it covers |
|---|---|
| [001-service-and-clients](001-service-and-clients/spec.md) | The single executable, the daemon, the gRPC contract, the command line, installing the service, packaging; the architecture and the original input |
| [002-configuration](002-configuration/spec.md) | The configuration file, applying changes while running, export and import, Apple's setup, capabilities |
| [003-virtual-ports](003-virtual-ports/spec.md) | Ports applications on one computer use to talk to each other |
| [004-hardware-devices](004-hardware-devices/spec.md) | Attached MIDI hardware and other applications' ports, hot-plug, identity |
| [005-network-ports](005-network-ports/spec.md) | RTP-MIDI between computers: discovery, invitations, the journal, machines, the automatic port |
| [006-bluetooth-midi](006-bluetooth-midi/spec.md) | Bluetooth LE MIDI, connecting out and advertising |
| [007-routing](007-routing/spec.md) | Routes between any two endpoints, loops, system-exclusive, the repeater |
| [008-resilience](008-resilience/spec.md) | Recovery: backoff, sleep and wake, releasing notes, restoring state, the MIDI service dying |
| [009-observability](009-observability/spec.md) | State, counters, history, the monitor, the diagnostic report |
| [010-graphical-interface](010-graphical-interface/spec.md) | The libcosmic window and its redesign |
| [011-midi-service-crash-warning](011-midi-service-crash-warning/spec.md) | Warning that other apps may have lost MIDI; when traffic last moved each way |
| [012-test-note](012-test-note/spec.md) | Sending a note out of an endpoint to test what listens |
| [013-windows-support](013-windows-support/spec.md) | Windows as a first-class platform |
| [014-mac-app-store-mode](014-mac-app-store-mode/spec.md) | The sandboxed App Store build, run from the menu bar |
| [015-mac-menus](015-mac-menus/spec.md) | The menus on macOS: File, Edit, View, Window and Help |

## Where each number is

### 001-service-and-clients

- **Requirements**: FR-036–FR-039, FR-039a, FR-039b, FR-039c, FR-039d, FR-039e, FR-039f, FR-039g, FR-039h, FR-040–FR-043
- **Success criteria**: SC-010, SC-014, SC-014a, SC-014b, SC-014c, SC-016
- **Research**: R-003, R-007–R-009, R-012, R-023, R-028, R-042, R-052–R-054, R-059, R-081
- **Tasks**: T001–T006, T009–T016, T028–T035, T038–T039, T041–T060, T063, T146–T147, T151–T154, T157, T166–T167, T172, T241

### 002-configuration

- **Requirements**: FR-049–FR-053
- **Success criteria**: SC-015
- **Research**: R-011, R-043, R-061
- **Tasks**: T022–T026, T040, T061–T062, T149–T150, T173–T174

### 003-virtual-ports

- **Requirements**: FR-001–FR-002, FR-002a, FR-003–FR-007
- **Success criteria**: SC-001, SC-002, SC-008
- **Research**: R-002, R-014, R-048
- **Tasks**: T064–T070, T072–T076, T161, T182, T197

### 004-hardware-devices

- **Requirements**: FR-015a, FR-015b, FR-015c, FR-015d, FR-015e, FR-015f, FR-015g
- **Success criteria**: SC-010a, SC-010c
- **Research**: R-013, R-024, R-027, R-038–R-040, R-047, R-055
- **Tasks**: T104, T107–T111, T116, T119, T158, T188, T190, T193

### 005-network-ports

- **Requirements**: FR-008–FR-015, FR-015h, FR-015i
- **Success criteria**: SC-003, SC-006, SC-009, SC-011
- **Research**: R-004, R-005, R-015–R-016, R-017–R-018, R-020–R-022, R-036–R-037, R-046, R-049, R-056–R-057, R-060, R-063, R-065–R-066, R-068, R-071, R-073, R-076, R-080, R-082
- **Tasks**: T007, T077–T082, T082a, T083–T096, T100–T103, T159, T168, T176, T179, T181, T184–T187, T191, T196, T198–T200

### 006-bluetooth-midi

- **Requirements**: FR-016–FR-021
- **Research**: R-006, R-041, R-058, R-064, R-075, R-077
- **Tasks**: T008, T037, T128–T137, T156, T162, T169–T171, T175

### 007-routing

- **Requirements**: FR-030, FR-030a, FR-030b, FR-031–FR-034, FR-034a, FR-035
- **Success criteria**: SC-010b
- **Research**: R-019, R-029–R-030, R-032, R-062
- **Tasks**: T105–T106, T111a, T112–T115, T117–T118, T160, T177, T183

### 008-resilience

- **Requirements**: FR-022–FR-029
- **Success criteria**: SC-004, SC-005, SC-007
- **Research**: R-010, R-031, R-033–R-035, R-044–R-045, R-050–R-051, R-067, R-069–R-070, R-072, R-074, R-079
- **Tasks**: T017–T020, T020a, T021, T036, T071, T071a, T097–T099, T127, T148, T155, T192, T195

### 009-observability

- **Requirements**: FR-044–FR-048
- **Success criteria**: SC-012, SC-013
- **Tasks**: T027, T120–T126, T163–T165, T178, T194

### 010-graphical-interface

- **Research**: R-001, R-025–R-026, R-078
- **Tasks**: T138–T145, T180, T189, T240

### 011-midi-service-crash-warning

- **Requirements**: FR-M01, FR-M02, FR-M03, FR-M04
- **Success criteria**: SC-M01, SC-M02
- **Research**: R-101
- **Tasks**: T236–T238, T242–T243

### 012-test-note

- **Requirements**: FR-N01, FR-N02
- **Success criteria**: SC-N01
- **Tasks**: T239

### 013-windows-support

- **Requirements**: FR-W01, FR-W02, FR-W03, FR-W04, FR-W05, FR-W06, FR-W07, FR-W08, FR-W09, FR-W10, FR-W11, FR-W12
- **Success criteria**: SC-W01, SC-W02, SC-W03
- **Research**: R-083–R-093
- **Tasks**: T201–T226, T227–T228, T229–T230, T231–T235

### 014-mac-app-store-mode

- **Requirements**: FR-A01, FR-A02, FR-A03, FR-A04, FR-A05, FR-A06, FR-A07, FR-A08, FR-A09, FR-A10, FR-A11, FR-A12, FR-A13, FR-A14
- **Success criteria**: SC-A01, SC-A02, SC-A03, SC-A04, SC-A05
- **Research**: R-094–R-100
- **Tasks**: T001–T034

### 015-mac-menus

- **Requirements**: FR-U01, FR-U02, FR-U03
- **Success criteria**: SC-U01
- **Research**: R-102
- **Tasks**: T244

## Earlier names

Until 2026-09-27 there were three specs. Branches, commit messages and older notes use these names.

| Earlier | Now |
|---|---|
| `001-midi-connectivity-manager` | Split into 001 to 010: the service and command line stayed in [001](001-service-and-clients/spec.md), each capability moved to its own spec, and the tasks added since for the MIDI service crash and the test note became 011 and 012 |
| `002-windows-support` | [013-windows-support](013-windows-support/spec.md) |
| `003-mac-app-store-mode` | [014-mac-app-store-mode](014-mac-app-store-mode/spec.md) |

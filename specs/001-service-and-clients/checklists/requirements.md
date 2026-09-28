# Specification Quality Checklist: Midi Harbor — MIDI Connectivity Manager

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-09-20
**Feature**: [spec.md](../spec.md)

## Content Quality

- [x] No implementation details (languages, frameworks, APIs)
- [x] Focused on user value and business needs
- [x] Written for non-technical stakeholders
- [x] All mandatory sections completed

## Requirement Completeness

- [x] No [NEEDS CLARIFICATION] markers remain
- [x] Requirements are testable and unambiguous
- [x] Success criteria are measurable
- [x] Success criteria are technology-agnostic (no implementation details)
- [x] All acceptance scenarios are defined
- [x] Edge cases are identified
- [x] Scope is clearly bounded
- [x] Dependencies and assumptions identified

## Feature Readiness

- [x] All functional requirements have clear acceptance criteria
- [x] User scenarios cover primary flows
- [x] Feature meets measurable outcomes defined in Success Criteria
- [x] No implementation details leak into specification

## Notes

Validation performed 2026-09-20. 6 user stories (P1–P5), 61 functional requirements, 19 success
criteria, 0 unresolved clarification markers.

**Resolved before drafting** — four decisions that would otherwise have been
`[NEEDS CLARIFICATION]` markers were settled with the product owner up front, and are recorded in
the spec as requirements rather than open questions:

1. Midi Harbor owns its own virtual endpoints and network sessions rather than reconfiguring the
   platform's built-in facilities, which research showed are not reliably programmable.
2. A background service owns all state; interfaces are clients of it.
3. Bluetooth LE MIDI covers both connecting out to devices and advertising this computer.
4. Network sessions must recover MIDI state lost to dropped packets, not merely reconnect.

**Amendment 1 (2026-09-20)**: single executable, service as a subcommand, optional graphical
interface, self-installing per-user service. Captured as User Story 2, FR-039/FR-039a–h, FR-042,
SC-014a–c, and four new edge cases.

**Amendment 2 (2026-09-20)**: physical MIDI hardware added as a first-class endpoint kind, enabling
the repeater use case — a physical device carried over a network session or Bluetooth link.
Captured as a rewritten User Story 4, FR-015a–g, FR-030a–b, SC-010a–c, the Physical MIDI Device
entity, four new edge cases, and research R-013. Re-validated against all 16 checklist items after
the amendment; all still pass.

**Judgement calls on the "no implementation details" criterion**:

- Protocol and discovery names (RTP-MIDI, mDNS, Bluetooth LE MIDI GATT) appear in requirements.
  These are accepted: they are interoperability obligations against named external implementations,
  not internal technology choices. FR-011 is meaningless without naming what must be interoperated
  with.
- `launchd` and `systemd` are named only in the Assumptions section and the amendment note. The
  requirements themselves (FR-039f, FR-039g) say "the platform's native user service manager".
- No programming language, GUI toolkit, crate, or library name appears anywhere in the
  requirements, success criteria, or user stories.

**Scope boundaries explicitly recorded as out of scope**: Windows support, MIDI 2.0, message
transformation (filtering, channel remap, transpose, velocity curves), operation across the public
internet including NAT traversal, transport encryption and authentication, and multi-user or
remote administration.

**Items requiring attention during planning, not spec revision**:

- SC-008 and SC-009 set latency budgets that constrain the data-path design; the plan must show how
  they are measured.
- FR-012 and FR-027 (recovery of lost MIDI state) are the hardest requirements in the document and
  drive the largest share of implementation risk.
- FR-039b (graphical interface excludable at build time) constrains how the interface is coupled to
  the rest of the system and must be reflected in the module boundaries chosen in the plan.
- FR-015e (stable physical device identity across replug) has a known platform limitation on Linux
  for hardware that reports no USB serial number; research R-013 records the composite-key strategy
  and the confidence level that surfaces the ambiguity to the user rather than mis-binding a route.
- FR-033 loop detection spans machines in the repeater case and cannot be satisfied by inspecting
  local configuration alone; the per-message origin marker must be designed in, not retrofitted.

**Downstream artifacts** (all complete as of 2026-09-20): [plan.md](../plan.md),
[research.md](../research.md), [data-model.md](../data-model.md), [quickstart.md](../quickstart.md),
[contracts/ipc-protocol.md](../contracts/ipc-protocol.md),
[contracts/cli-interface.md](../contracts/cli-interface.md).

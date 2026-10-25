# Specification Quality Checklist: Contention Modes with Graduated Yielding

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-10-24
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

- Iteration 2 (2026-10-24), after maintainer decisions. The three earlier markers are resolved
  and recorded under Clarifications:
  - Q1: Neutral by default; legacy `gpu.execution_mode` ignored with a deprecation warning.
  - Q2: floor policy A by default; B and C opt-in.
  - Q3: Neutral memory policy A by default; B opt-in.
- New user story 7 covers setup: the keyboard interface is the default, with command-line flags
  for non-TTY use.
  - FR-046–FR-055 define keys, color, `NO_COLOR`, text markers, caution confirmation, non-TTY
    behavior and config writes.
  - FR-045 justifies the risk-marker mapping.
- Success criteria now reference configuration values with provisional defaults, marked
  "provisional, to validate on real hardware" in the Provisional thresholds table.
- Implementation-detail check. The spec names these, all of them product surfaces:
  - the setup keys (arrows, Space, Enter)
  - `NO_COLOR`
  - two existing configuration keys (`gpu.execution_mode`, and the reserves reused as floors)
  - the commands `observe`, `doctor` and `status`

  Libraries and mechanisms appear only in [plan-notes.md](../plan-notes.md).
- Agent-facing documentation and MCP servers are a separate feature (`specs/002-agent-integration`).
- Iteration 3 (clarify, 2026-10-24): the envelope/spool behavior (FR-057, FR-058) and the ban on
  pausing Guaranteed work (FR-024) were recorded as proposed defaults. The maintainer confirmed
  both the same day. Checklist state is unchanged (16/16).
- Iteration 4 (analyze remediation, 2026-10-24): FR-051 and FR-054 and the hand-edit edge case
  now require an acknowledgement entry for danger options, as constitution Principle II
  requires (*superseded: constitution 2.0.0 has caution options only, acknowledged with
  `acknowledge_caution`*). Neutral and Performance scale-up rows were added to the Provisional thresholds
  table. Checklist state is unchanged (16/16).
- Iteration 5 (maintainer revisions before US5, 2026-10-25), with constitution 2.0.0:
  - Q2 and Q3 above are superseded. Floor policy A is the only floor policy, and floors always
    precede fair share for memory. Removed options fail to load, naming their replacement.
  - The only risk marker is *caution*, for Performance and floors near their minimum, and it
    needs confirmation (FR-045, FR-051, FR-054).
  - New requirements: principals (FR-059), memory headroom peak (FR-060), memory backoff
    (FR-061), readiness (FR-062), the critical floor path (FR-063), never sharing occupied GPUs
    (FR-064) and ordered mode targets (FR-065). New success criteria: reclaim bounds (SC-012)
    and target order (SC-013).
  - The legacy `gpu.execution_mode` values are interpreted (FR-031) instead of ignored.
  - Checklist state is unchanged (16/16).

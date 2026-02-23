# Proposal

Create and manage formal proposals for significant features or changes. Proposals go through: **PROPOSAL → VALIDATING → ACCEPTED/REJECTED**.

## Arguments

- `$AGENT` = agent identity (required)
- `<title>` = proposal title (required)
- `<slug>` = URL-friendly name for the doc, e.g., `agent-health-monitoring` (required)

## Proposal Doc Template

Create at `./notes/proposals/<slug>.md`:

```markdown
# Proposal: <Title>

**Status**: PROPOSAL
**Bone**: bd-xxx
**Author**: <agent-name>
**Date**: YYYY-MM-DD

## Summary

One paragraph describing the problem and proposed solution.

## Motivation

Why this matters. What pain point does it address?

## Proposed Design

Detailed design with:
- Architecture decisions
- Key components
- Integration points
- Example usage

## Open Questions

Questions that need answers before implementation:
1. Question one?
2. Question two?

## Answered Questions

Questions that have been resolved:
1. **Q:** Question? **A:** Answer with reasoning.

## Alternatives Considered

Other approaches and why they weren't chosen.

## Implementation Plan

High-level breakdown of work (becomes child bones if accepted):
1. First deliverable
2. Second deliverable
3. ...
```

## Creating a Proposal

1. Create bone: `maw exec default -- bn create --title "Proposal: <title>" --tag proposal --kind task`
2. Create doc at `./notes/proposals/<slug>.md` using template above
3. Update bone description to reference the doc

## Validating a Proposal

1. Change status header in doc to `**Status**: VALIDATING`
2. Investigate open questions (explore code, prototype, discuss)
3. Move answered questions from "Open Questions" to "Answered Questions" section
4. Add comment to bone with findings: `maw exec default -- bn bone comment add <id> "Validated X, answer is Y"`

## Accepting a Proposal

1. Change status header in doc to `**Status**: ACCEPTED`
2. Create implementation bones using the "Implementation Plan" section
3. Wire dependencies: `maw exec default -- bn triage dep add <proposal-id> --blocks <child-id>`
4. Close proposal bone: `maw exec default -- bn done <id> --reason "Accepted - implementation bones created"`

## Rejecting a Proposal

1. Change status header in doc to `**Status**: REJECTED`
2. Add "## Rejection Reason" section explaining why
3. Close proposal bone: `maw exec default -- bn done <id> --reason "Rejected - <brief reason>"`

## Lifecycle Summary

| Stage | Bone Tag | Status Header | Description |
|-------|----------|---------------|-------------|
| **PROPOSAL** | `proposal` | `**Status**: PROPOSAL` | Initial idea captured, design doc drafted |
| **VALIDATING** | `proposal` | `**Status**: VALIDATING` | Open questions being investigated |
| **ACCEPTED** | removed | `**Status**: ACCEPTED` | Design approved, ready for implementation |
| **REJECTED** | `proposal` | `**Status**: REJECTED` | Documented why not pursuing |

## Finding Proposals

```bash
# List all open proposals
maw exec default -- bn list --tag proposal

# Show a specific proposal bone
maw exec default -- bn show <id>
```

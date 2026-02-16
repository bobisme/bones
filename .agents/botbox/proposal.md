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
**Bead**: bd-xxx
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

High-level breakdown of work (becomes child beads if accepted):
1. First deliverable
2. Second deliverable
3. ...
```

## Creating a Proposal

1. Create bead: `maw exec default -- br create --actor $AGENT --owner $AGENT --title="Proposal: <title>" --labels proposal --type=task --priority=4`
2. Create doc at `./notes/proposals/<slug>.md` using template above
3. Update bead description to reference the doc: `maw exec default -- br update --actor $AGENT <id> --description="See notes/proposals/<slug>.md"`

## Validating a Proposal

1. Change status header in doc to `**Status**: VALIDATING`
2. Investigate open questions (explore code, prototype, discuss)
3. Move answered questions from "Open Questions" to "Answered Questions" section
4. Add comment to bead with findings: `maw exec default -- br comments add --actor $AGENT --author $AGENT <id> "Validated X, answer is Y"`

## Accepting a Proposal

1. Change status header in doc to `**Status**: ACCEPTED`
2. Remove proposal label: `maw exec default -- br label rm --actor $AGENT -l proposal <id>`
3. Create implementation beads using the "Implementation Plan" section
4. Wire dependencies: `maw exec default -- br dep add --actor $AGENT <child-id> <proposal-id>`
5. Close proposal bead: `maw exec default -- br close --actor $AGENT <id> --reason "Accepted - implementation beads created"`

## Rejecting a Proposal

1. Change status header in doc to `**Status**: REJECTED`
2. Add "## Rejection Reason" section explaining why
3. Close proposal bead: `maw exec default -- br close --actor $AGENT <id> --reason "Rejected - <brief reason>"`

## Lifecycle Summary

| Stage | Bead Label | Status Header | Description |
|-------|------------|---------------|-------------|
| **PROPOSAL** | `proposal` | `**Status**: PROPOSAL` | Initial idea captured, design doc drafted |
| **VALIDATING** | `proposal` | `**Status**: VALIDATING` | Open questions being investigated |
| **ACCEPTED** | removed | `**Status**: ACCEPTED` | Design approved, ready for implementation |
| **REJECTED** | `proposal` | `**Status**: REJECTED` | Documented why not pursuing |

## Finding Proposals

```bash
# List all open proposals
maw exec default -- br list --label proposal

# Show a specific proposal bead
maw exec default -- br show <id>
```

# qgh Issue Triage Loop

Goal: keep the GitHub issue backlog actionable for humans and future agents.

GitHub Issues for `juicyjusung/qgh` are the tracker source of truth. #18 is the current loop-state snapshot. #19 is append-only run history.

## Cadence

Run daily while MVP work is active.

## Inputs

- `gh issue list --state open`
- `gh issue view <number> --comments` when detail is needed
- `docs/agents/issue-tracker.md`
- `docs/agents/triage-labels.md`
- qgh PRD, product brief, domain docs, and relevant ADRs
- `gh issue view 18 --comments`
- `gh issue view 19 --comments`

## Outputs

- Top five actionable issues by loop score
- Proposed labels, not applied in L1
- Needs-human items
- Possible duplicates for human confirmation
- Noise/ignored list
- Compact run entry in #19

## L1 Boundaries

- Do not apply labels.
- Do not close issues.
- Do not comment on issues.
- Do not edit issue bodies, titles, milestones, assignees, or state.
- Exception: update #18/#19 for loop state/run history only.
- Do not create branches, PRs, or implementation plans.

## Human Gates

Escalate product scope, privacy, source identity, schema contracts, MCP surface, token handling, hosted egress, and duplicate uncertainty.

---
name: issue-triage
description: >
  qgh backlog-health loop. Reads product GitHub Issues as tracker SSOT, then
  updates GitHub issue-backed loop state in #18/#19. Never mutates product issues in L1.
user_invocable: true
---

# qgh Issue Triage Skill

You keep the qgh GitHub issue backlog legible without changing product issues.

## Source Of Truth

GitHub Issues for `juicyjusung/qgh` are the tracker source of truth.

#18 (`Loop State: qgh 운영 상태`) stores current loop state.
#19 (`Loop Run Log: qgh 실행 이력`) stores append-only run history.

Local `issue-triage-state.md` is only a static pointer. Do not write dynamic state there. Do not use `.scratch/` as a tracker.

## Inputs

- `docs/agents/issue-tracker.md`
- `docs/agents/triage-labels.md`
- `qgh-prd.md`
- `qgh-product-brief.md`
- `CONTEXT.md`
- Relevant ADRs under `docs/adr/`
- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- Open GitHub Issues from `gh issue list --state open --json number,title,body,labels,comments,updatedAt`

## Output

Update only #18 during L1 and append one compact run entry to #19:

```markdown
# Loop State: qgh 운영 상태

Last run: <ISO timestamp>
Open actionable: N (was M)
New since last run: K
Needs human: H

## Top 5 (by loop score)

- #NNN (p1, age/update signal) - short summary - suggested: `label-a`, `label-b`

## Proposed Labels (not applied - L1)

- #NNN: `needs-info`

## Possible Duplicates (human confirm)

- #NNN may duplicate #MMM because ...

## Needs Human

- #NNN - reason

## Noise / Ignored

- #NNN - reason
```

## Canonical Labels

Use only this repo's triage vocabulary:

- `needs-triage`
- `needs-info`
- `ready-for-agent`
- `ready-for-human`
- `wontfix`

## L1 Rules

- Propose labels only. Never apply labels.
- Never close issues.
- Never comment on product/implementation issues from L1 triage.
- Never edit issue bodies, titles, milestones, assignees, or state.
- Exception: #18 and #19 may be updated for loop state/run history only.
- Never create branches, PRs, source edits, or implementation plans from this skill.
- Reference issue numbers and concise summaries. Do not copy full issue bodies into #18/#19.

## Escalate To Human

Put issues in `Needs Human` when they affect:

- PRD or product scope
- Source identity, locators, tombstone, reconciliation, or Citation Contract semantics
- Local DB schema or storage format
- Privacy defaults, hosted egress, telemetry, token handling, or credential persistence
- MCP tool surface or read-only contract
- Duplicate detection with meaningful uncertainty
- Any label or closure action that would be irreversible or politically sensitive

## Scoring

- P0: security, data loss, private-content leak, broken privacy default
- P1: blocks MVP tracer flow or validated release gate
- P2: useful MVP work with clear acceptance
- P3: docs, polish, or post-MVP work
- `needs-info`: missing repro, unclear acceptance, or conflicting scope
- `duplicate?`: possible overlap; human confirms

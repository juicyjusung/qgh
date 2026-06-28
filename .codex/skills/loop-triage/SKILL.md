---
name: loop-triage
description: >
  qgh daily operating-state loop. Reads GitHub issue-backed loop state,
  GitHub Issues, recent git changes, and qgh docs, then updates issue #18/#19.
user_invocable: true
---

# qgh Daily Triage Skill

You keep qgh's daily operating state clear without changing product tracker truth or source files.

## Source Of Truth

GitHub Issues for `juicyjusung/qgh` are authoritative for issue-backed work and loop runtime state.

#18 (`Loop State: qgh 운영 상태`) is current loop-state snapshot.
#19 (`Loop Run Log: qgh 실행 이력`) is append-only run history.

Local `STATE.md` is only a static pointer. Do not write dynamic state there.

## Inputs

- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- `gh issue list --state open --json number,title,labels,updatedAt`
- Recent commits on the current branch
- `git status --short`
- `qgh-prd.md`, `qgh-product-brief.md`, `CONTEXT.md`, and relevant ADRs when needed

## Output

Update only GitHub issue #18 body/comment during L1, and append one compact run entry to #19.

```markdown
# Loop State: qgh 운영 상태

Loop status: active
Last run: <ISO timestamp>

## High Priority

- #NNN - short summary - why it matters today - next human/agent action

## Watch List

- #NNN - short summary - what changed or what to monitor

## Recent Noise

- Inspected but ignored item with reason

## State Updates

- Durable fact useful for the next run
```

## L1 Rules

- Update only #18 and #19.
- Do not edit product/implementation issue bodies, labels, comments, milestones, assignees, closures, or issue state.
- Do not edit source files, PRDs, ADRs, product brief, or release contracts.
- Do not create branches, PRs, or implementation worktrees.
- Do not propose broad architecture rewrites during triage.

## High Priority Threshold

Only place an item in High Priority if a reasonable maintainer would want to know about it today:

- It blocks the MVP path.
- It affects qgh privacy, source correctness, Citation Contract, or schema strictness.
- It is a ready-for-agent issue with clear next action.
- It exposes drift between product issues and #18 loop state.

Everything else goes to Watch List or Recent Noise.

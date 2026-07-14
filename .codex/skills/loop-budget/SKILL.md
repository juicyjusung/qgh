---
name: loop-budget
description: >
  qgh loop budget guard. Checks loop-budget.md plus GitHub issue-backed pause
  and run-log state before a loop run. L1 guard only; mutates only #18/#19 when needed.
metadata:
  internal: true
user_invocable: true
---

# qgh Loop Budget Skill

Check whether a qgh loop run is allowed before it starts.

## Source Of Truth

GitHub Issues remain the tracker source of truth. This skill controls loop execution hygiene through #18/#19.

## Inputs

- `loop-budget.md`
- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- Requested loop pattern: `issue-triage` or `daily-triage`

## L1 Budget Rules

- `Issue Triage`: max 1 run/day, max 0 sub-agent spawns/run
- `Daily Triage`: max 1 run/day, max 0 sub-agent spawns/run
- If #18 contains `Loop status: paused`, reject the run.
- If the run would exceed the daily budget, reject the run and tell the caller to record a budget-exceeded entry in #19.
- `loop-audit` reporting L2/L3 readiness does not authorize L2 behavior.

## Output

Return one concise verdict:

- `ALLOW` with loop name and current count
- `REJECT_BUDGET` with the exceeded limit
- `REJECT_PAUSED` when `STATE.md` pauses loops
- `ESCALATE_HUMAN` for malformed or contradictory budget/state files

## Prohibited

- Do not edit product/implementation GitHub Issues.
- Only #18/#19 may be updated for loop state/run history.
- Do not apply labels.
- Do not start implementation work.
- Do not spawn sub-agents in L1.

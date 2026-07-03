# Loop Budget - qgh

Keep runs cheap and inspectable. Runtime loop state is GitHub issue-backed to avoid worktree divergence.

Audit readiness scores do not change the active phase. Triage loops stay L1 report-only. The Implementation Lane is L2 assisted as of 2026-07-04 (explicit human approval, see LOOP.md and #18/#19).

## Daily Limits

| Loop | Max runs/day | Max sub-agent spawns/run | Phase |
|---|---:|---:|---|
| Issue Triage | 1 | 0 | L1 report-only |
| Daily Triage | 1 | 0 | L1 report-only |
| Implementation Lane | 8 | 1 (checker session) | L2 assisted, draft PR only |

## On Budget Exceed

1. Stop scheduled/manual loop runs for the day.
2. Append the event to GitHub issue #19.
3. Add a human-visible note to GitHub issue #18.

## Kill Switch

- GitHub issue #18 containing `Loop status: paused` stops all loop state updates.
- `loop-pause-all` is a documented future GitHub label kill switch, but this scaffold does not create or require the label.

## Spend Discipline

- Empty watchlist runs should exit quickly.
- L1 must not spawn sub-agents.
- L1 must not trigger implementation, GitHub writes, or external hosted services.

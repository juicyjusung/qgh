# qgh Daily Triage Loop

Goal: keep the repository operating state clear enough that a human can see what matters today.

GitHub Issues are the tracker source of truth. #18 is the current loop-state snapshot. #19 is append-only run history.

## Cadence

Run daily after Issue Triage when both loops run on the same day.

## Inputs

- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- `gh issue list --state open`
- Recent commits and `git status --short`
- Existing `STATE.md`
- qgh PRD, product brief, domain docs, and relevant ADRs when needed

## Outputs

- High Priority issue-number references
- Watch List
- Recent Noise
- State Updates for the next run
- Compact run entry in #19

## L1 Boundaries

- Do not edit GitHub Issues.
- Exception: update #18/#19 for loop state/run history only.
- Do not edit source files, PRDs, ADRs, product brief, or release contracts.
- Do not create branches, PRs, implementation plans, or worktrees.
- Do not write dynamic state to local `STATE.md`.

## Human Gates

Escalate product scope changes, ADR changes, GitHub writes, implementation work, privacy changes, and source-correctness risks.

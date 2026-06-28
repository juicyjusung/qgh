# qgh Loop Engineering Design

Date: 2026-06-28 KST
Status: Proposed for review

## Purpose

qgh should run with a small loop-engineering system that keeps the MVP issue queue and daily operating state legible without changing product scope or taking autonomous write actions.

The first rollout is L1 report-only. The loops may read repository docs, git history, and GitHub Issues, then update local loop state files. They must not modify GitHub issues, labels, source files, PRD, ADRs, or release contracts unless a human explicitly asks for that action in a normal Codex session.

## Source Context

The loop system must follow these qgh sources of truth:

- `qgh-prd.md`
- `qgh-product-brief.md`
- `CONTEXT.md`
- `docs/adr/`
- `docs/agents/issue-tracker.md`
- `docs/agents/triage-labels.md`
- `docs/agents/domain.md`
- GitHub Issues for `juicyjusung/qgh`

The upstream loop-engineering reference contributes the operating pattern: durable state, explicit cadence, budget, run log, maker/checker split, and human gates. qgh adapts that pattern to a local-first, read-only CLI/MCP product.

## Active Loops

### Issue Triage Loop

Goal: keep the GitHub issue backlog actionable for humans and agents.

Cadence: daily while the MVP is being built. It can later move to twice daily if issue volume grows.

State file: `issue-triage-state.md`

Inputs:

- Open GitHub Issues from `juicyjusung/qgh`
- Existing labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`
- Previous `issue-triage-state.md`
- qgh PRD, product brief, ADRs, and domain language

Outputs:

- Last run timestamp
- Open actionable count and delta from the previous run
- Top five issues by current implementation usefulness
- Proposed label changes, not applied in L1
- Needs-human bucket for ambiguity, scope changes, privacy risk, product contract changes, or duplicate uncertainty
- Possible duplicates for human confirmation
- Noise/ignored list for inspected items that should not create work

L1 constraints:

- Never apply labels.
- Never close issues.
- Never comment on issues.
- Never change issue bodies.
- Never create implementation branches or PRs.

### Daily Triage Loop

Goal: keep the repository operating state clear enough that a human or future loop can see what matters today.

Cadence: daily, after Issue Triage when both run on the same day.

State file: `STATE.md`

Inputs:

- `issue-triage-state.md`
- Recent commits on the current branch
- Git status
- Open GitHub Issues
- PRD/brief/ADR changes
- Existing `STATE.md`

Outputs:

- Last run timestamp
- High Priority section
- Watch List section
- Recent Noise section
- State Updates section for facts the next run should remember

L1 constraints:

- Do not edit product docs, source files, issue tracker, labels, or GitHub comments.
- Do not propose broad architecture rewrites during triage.
- Do not turn search results or issue snippets into final answers; qgh's citation-contract language must remain intact.

## State Files

### `issue-triage-state.md`

Expected shape:

```markdown
# Issue Triage State - qgh

Last run: never
Open actionable: 0
New since last run: 0
Needs human: 0

## Top 5 (by loop score)

## Proposed Labels (not applied - L1)

## Possible Duplicates (human confirm)

## Needs Human

## Noise / Ignored

## Allowlisted Labels (L2 only)

`needs-info`, `ready-for-agent`

## Denylist (always human)

privacy, tokens, source identity, storage format, schema contracts, MCP write scope,
hosted egress, PRD/ADR scope changes
```

### `STATE.md`

Expected shape:

```markdown
# Loop State - qgh

Last run: never

## High Priority

## Watch List

## Recent Noise

## State Updates

---
Run log: see `loop-run-log.md`
```

### `loop-budget.md`

The initial budget should keep loops cheap and auditable:

| Loop | Max runs/day | Max sub-agent spawns/run | Phase |
|---|---:|---:|---|
| Issue Triage | 1 | 0 | L1 report-only |
| Daily Triage | 1 | 0 | L1 report-only |

Budget exceed behavior:

1. Stop scheduled/manual loop runs for the day.
2. Append the event to `loop-run-log.md`.
3. Escalate to the human in `STATE.md`.

Kill switch:

- If `STATE.md` contains `Loop status: paused`, loops must not update state files.
- If GitHub label `loop-pause-all` exists on an issue, the loop should report the pause and stop.

### `loop-run-log.md`

Append one JSON object per run under a Recent Runs section:

```json
{
  "run_id": "2026-06-28T00:00:00Z",
  "pattern": "issue-triage",
  "duration_s": 0,
  "items_found": 0,
  "actions_taken": 0,
  "escalations": 0,
  "outcome": "report-only"
}
```

## Skills and Agents

### `.codex/skills/issue-triage/SKILL.md`

The skill should be qgh-specific. It should read GitHub Issues through `gh`, respect `docs/agents/issue-tracker.md`, and update only `issue-triage-state.md` in L1.

It must classify work using the repo's canonical labels:

- `needs-triage`
- `needs-info`
- `ready-for-agent`
- `ready-for-human`
- `wontfix`

It must escalate to needs-human when an issue affects:

- PRD or product scope
- Source identity and locator contracts
- Local DB schema or storage format
- Privacy defaults or hosted egress
- Token handling
- MCP tool surface or read-only contract
- Citation Contract semantics

### `.codex/skills/loop-triage/SKILL.md`

The skill should summarize daily engineering state. It should read the issue-triage state first, then combine it with recent repository changes.

It must not duplicate full issue bodies or private content in `STATE.md`. Issue references should use issue numbers and short summaries.

### `.codex/agents/verifier.toml`

The verifier exists as L2 preparation only. In L1, there are no auto-fixes to verify.

When L2 begins, the verifier must reject work unless it has evidence for:

- Minimal scope
- Passing relevant tests or documented blocker
- No disabled tests or weakened assertions
- No violation of qgh product guardrails
- Human review for medium or higher risk

## Safety Gates

The following require human review in all phases:

- Any change to `qgh-prd.md`, `qgh-product-brief.md`, `CONTEXT.md`, or `docs/adr/`
- Any GitHub issue body edit, issue closure, or label application in L1
- Any change that broadens qgh beyond Issues, issue comments, and Wiki retrieval
- Any write-capable MCP tool, sync-capable MCP tool, or hosted embedding/rerank default
- Any storage, log, fixture, or config behavior that could persist tokens or private content unexpectedly
- Any change to strict schema validation, source identity, tombstone/reconciliation, or Citation Contract behavior
- Any dependency addition or automation that introduces network egress beyond GitHub without explicit opt-in

## Phase Plan

### L0 to L1

Add the loop documentation and state scaffolding:

- `LOOP.md`
- `STATE.md`
- `issue-triage-state.md`
- `loop-budget.md`
- `loop-run-log.md`
- `docs/safety.md`
- `.codex/skills/issue-triage/SKILL.md`
- `.codex/skills/loop-triage/SKILL.md`
- `.codex/agents/verifier.toml`

Run `npx @cobusgreyling/loop-audit . --suggest` and record the score.

### L1 Calibration

Run both loops manually at least three times.

Acceptance for staying in L1:

- State files are concise and useful.
- Proposed labels match `docs/agents/triage-labels.md`.
- No source or issue tracker writes happen during runs.
- Noise does not dominate the state files.
- A human can identify the top MVP work from `STATE.md` and `issue-triage-state.md`.

### L2 Assisted

L2 is out of scope for the first implementation. It can be considered after L1 has three useful runs and the human approves the transition.

Possible L2 actions:

- Apply allowlisted labels after verifier approval.
- Draft a single-issue implementation plan in an isolated worktree.
- Comment on an issue with a Korean agent-ready brief when explicitly requested.

Still forbidden without a separate scope decision:

- Auto-close issues.
- Auto-merge PRs.
- Add write-capable MCP tools.
- Change PRD/ADR scope automatically.

## Verification

Docs-only verification for the first implementation:

1. Confirm the new files exist.
2. Confirm Markdown links and referenced local paths exist where practical.
3. Run `npx @cobusgreyling/loop-audit . --suggest`.
4. Confirm the audit score improves from the observed baseline of `19/100 L0`.
5. Inspect `git diff` to confirm only loop-system files changed.

Runtime verification after L1 scaffold:

1. Run Issue Triage manually once.
2. Confirm it updates only `issue-triage-state.md` and `loop-run-log.md`.
3. Run Daily Triage manually once.
4. Confirm it updates only `STATE.md` and `loop-run-log.md`.
5. Confirm no GitHub issue labels, comments, titles, or bodies changed.

## Deferred Decisions

- The first manual Issue Triage run remains local-only. It must not write a GitHub issue comment until L1 has at least three useful report-only runs.
- GitHub Actions automation is deferred. Initial loop execution uses manual Codex sessions or Codex Automations without repository workflows.
- `loop-pause-all` remains a documented future kill switch until automation exists. The first implementation should not create this GitHub label.

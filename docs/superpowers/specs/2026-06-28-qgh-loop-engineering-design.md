# qgh Loop Engineering Design

Date: 2026-06-28 KST
Status: Approved for L1 scaffold

## Purpose

qgh should run with a small loop-engineering system that keeps the MVP issue queue and daily operating state legible without changing product scope or taking autonomous write actions.

The first rollout is L1 report-only. The loops may read repository docs, git history, and GitHub Issues, then update GitHub issue-backed loop state in #18/#19. They must not modify product/implementation issue bodies, labels, source files, PRD, ADRs, or release contracts unless a human explicitly asks for that action in a normal Codex session.

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

## Research Findings Applied

The current loop-engineering guidance converges on a few operational practices that qgh should adopt from the start:

- Loops need a heartbeat, isolated workspaces, skills, connectors, sub-agents, and durable state/memory. qgh uses manual Codex/Codex Automation as the heartbeat, project skills for loop behavior, and GitHub issues as durable loop memory.
- Production loops should start with a single clear goal, explicit non-goals, tight skill outputs, report-only rollout, state updates, human handoff, budget limits, and run logs.
- Multiple loops need clear state ownership. qgh keeps current loop state in #18 and run history in #19 so worktrees do not fork dynamic state.
- Triage loops should report and propose; action loops execute. qgh's first rollout has only report/proposal loops.
- Safety guidance favors least-privilege connectors, no auto-merge by default, denylisted high-risk paths, and human approval before external writes.
- GitHub Issues are suitable for planning and tracking work; labels classify issues. For qgh, this means GitHub Issues remain the tracker source of truth and also hold loop runtime state.

## GitHub Issue Tracker SSOT

GitHub Issues for `juicyjusung/qgh` are the single source of truth for issue-backed work, PRDs mirrored to issues, implementation slices, labels, blockers, and final verification.

Loop state lives in GitHub issues:

- #18 (`Loop State: qgh 운영 상태`) stores the current loop-state snapshot.
- #19 (`Loop Run Log: qgh 실행 이력`) stores append-only run history in comments.

Local `STATE.md`, `issue-triage-state.md`, and `loop-run-log.md` are static pointers only. They must not receive dynamic state from worktrees. If local pointer files disagree with GitHub, GitHub wins.

The loop system must not use `.scratch/` as a tracker SSOT. Local pointer files may reference GitHub issue numbers but must not replace issue bodies, issue comments, labels, acceptance criteria, or loop runtime state.

In L1, loops may update only #18/#19 for loop state/run history. Any update to product/implementation issue bodies, comments, labels, milestones, assignees, or issue state requires an explicit human request in a normal Codex session. When such an update happens, the final response must include `Updated #<number>: <reason>` per `AGENTS.md`.

## Active Loops

### Issue Triage Loop

Goal: keep the GitHub issue backlog actionable for humans and agents.

Cadence: daily while the MVP is being built. It can later move to twice daily if issue volume grows.

State: GitHub issue #18. Run log: GitHub issue #19.

Inputs:

- Open GitHub Issues from `juicyjusung/qgh`
- Existing labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`
- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- qgh PRD, product brief, ADRs, and domain language

Outputs:

- Last run timestamp
- Open actionable count and delta from the previous run
- Top five issues by current implementation usefulness
- Proposed label changes, not applied in L1
- Needs-human bucket for ambiguity, scope changes, privacy risk, product contract changes, or duplicate uncertainty
- Possible duplicates for human confirmation
- Noise/ignored list for inspected items that should not create work

SSOT rule:

- Read issue truth from GitHub with `gh`.
- Record only issue numbers, short summaries, proposed labels, and review notes in #18/#19.
- Never treat local `issue-triage-state.md` as dynamic state.

L1 constraints:

- Never apply labels.
- Never close issues.
- Never comment on product/implementation issues.
- Exception: #18/#19 may be updated for loop state/run history only.
- Never change issue bodies.
- Never create implementation branches or PRs.

### Daily Triage Loop

Goal: keep the repository operating state clear enough that a human or future loop can see what matters today.

Cadence: daily, after Issue Triage when both run on the same day.

State: GitHub issue #18. Run log: GitHub issue #19.

Inputs:

- `gh issue view 18 --comments`
- `gh issue view 19 --comments`
- Recent commits on the current branch
- Git status
- Open GitHub Issues
- PRD/brief/ADR changes

Outputs:

- Last run timestamp
- High Priority section
- Watch List section
- Recent Noise section
- State Updates section for facts the next run should remember

SSOT rule:

- #18 can summarize current priorities, but issue-backed work must point back to GitHub issue numbers.
- If a priority requires changing scope, acceptance criteria, labels, or blockers, the loop must put it in the human inbox instead of editing the tracker.

L1 constraints:

- Do not edit product docs, source files, product/implementation issue tracker, labels, or product issue comments.
- Do not propose broad architecture rewrites during triage.
- Do not turn search results or issue snippets into final answers; qgh's citation-contract language must remain intact.

## State Issues And Pointer Files

### GitHub issue #18

Expected body shape:

```markdown
# Loop State: qgh 운영 상태

Loop status: active
Loop phase: L1 report-only
Last run: <ISO timestamp>

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

### GitHub issue #19

Expected comments shape:

```markdown
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

### Local pointer files

- `STATE.md` points to #18.
- `issue-triage-state.md` points to #18.
- `loop-run-log.md` points to #19.

These files are static. Loops must not write dynamic state to them.

### `loop-budget.md`

The initial budget should keep loops cheap and auditable:

| Loop | Max runs/day | Max sub-agent spawns/run | Phase |
|---|---:|---:|---|
| Issue Triage | 1 | 0 | L1 report-only |
| Daily Triage | 1 | 0 | L1 report-only |

Budget exceed behavior:

1. Stop scheduled/manual loop runs for the day.
2. Append the event to #19.
3. Escalate to the human in #18.

Kill switch:

- If #18 contains `Loop status: paused`, loops must not update loop state.
- If GitHub label `loop-pause-all` exists on an issue, the loop should report the pause and stop.

## Skills and Agents

### `.codex/skills/issue-triage/SKILL.md`

The skill should be qgh-specific. It should read GitHub Issues through `gh`, respect `docs/agents/issue-tracker.md`, update #18 for loop state, and append run entries to #19 in L1.

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

It must also enforce the tracker SSOT boundary:

- GitHub Issues are authoritative for issue-backed work.
- local `issue-triage-state.md` is a static pointer.
- L1 may propose label changes but must not apply them.
- L1 may write only #18/#19. It may not write product/implementation issue comments, bodies, milestones, assignees, or state.

### `.codex/skills/loop-triage/SKILL.md`

The skill should summarize daily engineering state. It should read #18 first, then combine it with recent repository changes.

It must not duplicate full issue bodies or private content in #18/#19. Issue references should use issue numbers and short summaries.

It must not turn #18 into a replacement issue tracker. The Daily Triage loop should link back to GitHub issue numbers and leave implementation scope, acceptance criteria, and final verification in the owning GitHub Issues.

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
- Any product/implementation GitHub issue body edit, issue closure, or label application in L1
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
- `patterns/registry.yaml`
- `patterns/issue-triage.md`
- `patterns/daily-triage.md`
- `.codex/skills/issue-triage/SKILL.md`
- `.codex/skills/loop-triage/SKILL.md`
- `.codex/skills/loop-budget/SKILL.md`
- `.codex/agents/verifier.toml`
- GitHub issue #18 for loop state
- GitHub issue #19 for run log

Run `npx @cobusgreyling/loop-audit . --suggest` and record the score.

An audit score is readiness evidence, not permission to advance phases. Even if `loop-audit` reports L2/L3 structural readiness, qgh remains L1 until three useful report-only runs complete and a human explicitly approves L2.

### L1 Calibration

Run both loops manually at least three times.

Acceptance for staying in L1:

- #18/#19 are concise and useful.
- Proposed labels match `docs/agents/triage-labels.md`.
- No source or product/implementation issue tracker writes happen during runs.
- Noise does not dominate #18/#19.
- A human can identify the top MVP work from #18.

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
2. Confirm Markdown links, referenced local paths, and #18/#19 URLs exist where practical.
3. Run `npx @cobusgreyling/loop-audit . --suggest`.
4. Confirm the audit score improves from the observed baseline of `19/100 L0`.
5. Inspect `git diff` to confirm only loop-system files changed.

Runtime verification after L1 scaffold:

1. Run Issue Triage manually once.
2. Confirm it updates only #18/#19.
3. Run Daily Triage manually once.
4. Confirm it updates only #18/#19.
5. Confirm no product/implementation GitHub issue labels, comments, titles, or bodies changed.

## Deferred Decisions

- The first manual Issue Triage run was migrated into GitHub issue-backed loop state (#18/#19) to avoid worktree divergence.
- GitHub Actions automation is deferred. Initial loop execution uses manual Codex sessions or Codex Automations without repository workflows.
- `loop-pause-all` remains a documented future kill switch until automation exists. The first implementation should not create this GitHub label.

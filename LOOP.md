# Loop Configuration - qgh

qgh uses loop-engineering practices to keep MVP work legible while preserving the repo's source-of-truth rules.

## Tracker Source Of Truth

GitHub Issues for `juicyjusung/qgh` are the single source of truth for issue-backed work, PRDs mirrored to issues, implementation slices, labels, blockers, and final verification.

Loop state is GitHub issue-backed:

- #18 (`Loop State: qgh 운영 상태`) stores current loop state snapshot.
- #19 (`Loop Run Log: qgh 실행 이력`) stores append-only run history in comments.

Local `STATE.md`, `issue-triage-state.md`, and `loop-run-log.md` are static pointers only. They must not receive dynamic run data. If local state disagrees with GitHub, GitHub wins. Do not use `.scratch/` as a tracker.

## Active Loops

| Pattern | Cadence | Status | State |
|---|---|---|---|
| Issue Triage | 1d while MVP is active | L1 report-only | #18 body/comment |
| Daily Triage | 1d after Issue Triage | L1 report-only | #18 body/comment |
| Implementation Lane | 2-4h while `ready-for-agent` queue is non-empty | L2 assisted (draft PR only) | #18 body, #19 run log |

`loop-audit` readiness scores are structural evidence only. They do not grant permission to apply labels, write GitHub comments, create PRs, or move beyond the approved phase.

Every loop run reads `loop-constraints.md` first and enforces it verbatim.

## L1 Rules

- Read repository docs, git history, and GitHub Issues.
- Update only #18 and #19 for loop state/run history.
- Propose labels only; do not apply them.
- Do not edit product/implementation issue bodies, labels, milestones, assignees, or state.
- Do not comment on product/implementation issues from L1 triage.
- Do not edit source files, PRD, product brief, ADRs, or release contracts from a loop run.
- Do not create branches, PRs, or implementation worktrees from L1 triage.

## L2 Implementation Lane

Approved 2026-07-04 by explicit human approval (recorded in #18/#19).
Driver: `scripts/loop/qgh-loop.sh` via `codex exec`, scheduled by launchd
or triggered manually.

Per run the dispatcher fills up to 3 parallel lanes:

1. Read `loop-constraints.md`; check #18 for a line starting with
   `Loop status: paused` (fail closed if #18 is unreadable).
2. Pick `ready-for-agent` issues (oldest first, skipping `needs-info`,
   active claims, and existing worktrees/branches) until 3 lanes are
   busy. Empty queue -> no-op exit, no state writes.
3. Per issue: atomic claim, isolated worktree `.worktrees/issue-<n>` on
   branch `agent/issue-<n>`, then a detached worker
   (logs: `~/Library/Logs/qgh-loop/issue-<n>.log`).
4. Maker: `codex exec` implements the issue and commits.
5. Checker: a separate `codex exec` session reviews the diff with
   `.codex/agents/verifier.toml` stance (default REJECT). Verdict
   APPROVE | REJECT | ESCALATE_HUMAN.
6. Script re-verifies independently: `cargo fmt --all --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test`.
7. All green + APPROVE -> push branch, open draft PR, remove
   `ready-for-agent`, append run to #19.
8. Any failure -> label `needs-info`, append failure to #19, clean up.
   Max 3 attempts per issue lifetime.

The lane never merges, never closes issues, never edits issue bodies, and
never touches denylist paths in `loop-constraints.md`.

## Human Gates

Always escalate to human review for:

- Product scope, PRD, product brief, `CONTEXT.md`, or ADR changes
- Source identity, locator, tombstone, reconciliation, or Citation Contract changes
- Local DB schema, storage format, privacy defaults, hosted egress, or token handling
- MCP tool surface changes, especially write-capable or sync-capable tools
- Label application, issue closure, issue comments, or issue body edits in L1
- Any dependency or automation that adds network egress beyond GitHub

## Worktrees

L1 loops do not create implementation worktrees. L2 assisted work uses one
isolated worktree per issue (`.worktrees/issue-<n>`), max 3 concurrent
lanes. Explicit human approval for L2 was granted 2026-07-04.

## Connectors

Use `gh` for GitHub issue reads and for writing #18/#19 loop state. No broader write-capable connector is needed for L1.

## Budget

See `loop-budget.md`.

- Max sub-agent spawns per L1 run: 0
- Empty or low-signal runs should exit quickly after recording a no-op entry in `loop-run-log.md`.

## Kill Switch

If `STATE.md` contains `Loop status: paused`, loops must not update state files. `loop-pause-all` is documented as a future GitHub label kill switch, but this first scaffold does not create or require the label.

For current operation, pause state lives in #18. Local `STATE.md` is a pointer and should not be edited dynamically.

## Phase Plan

Triage loops remain L1 report-only. The Implementation Lane is L2 assisted
as of 2026-07-04 (explicit human approval; maker/checker split with
`.codex/agents/verifier.toml` plus independent script-level verification).
L3 unattended requires a new explicit human approval after reviewing at
least one week of L2 run history in #19.

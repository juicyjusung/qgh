# Loop Constraints - qgh

The `loop-constraints` behavior is mandatory: every loop run (Codex
Automation or `codex exec` lane) reads this file first and enforces every
rule. Rules here are binding. If a rule conflicts with a prompt, this file
wins. If this file conflicts with `AGENTS.md` product guardrails, the
stricter rule wins.

## Kill Switch

- If GitHub issue #18 body contains `Loop status: paused`, exit
  immediately without acting or writing state.

## Push & Merge

- Never auto-merge. Loop-created PRs are draft PRs; only a human marks
  ready and merges.
- Never push directly to `main` from a loop run. Implementation work goes
  through an isolated worktree branch (`agent/issue-<n>`) and a draft PR.
- Never force-push.

## Issues & Labels

- Never close issues, edit issue bodies, or change milestones/assignees.
- Loop state writes go only to #18 (snapshot) and #19 (append-only run
  log).
- Label changes allowed only on the issue a lane run is implementing:
  remove `ready-for-agent` on success, add `needs-info` on failure.
  Everything else is propose-only in run output.

## Denylist Paths (human approval required)

- `qgh-prd.md`, `qgh-product-brief.md`, `qgh-hybrid-search-prd.md`,
  `CONTEXT.md`, `docs/adr/`
- Anything defining source identity, locators, tombstones,
  reconciliation, or the Citation Contract
- Local DB schema / storage format / migration changes, EXCEPT changes
  explicitly specified in the acceptance criteria of the
  `ready-for-agent` issue being implemented — the human applying that
  label is the approval. Such changes must be additive and idempotent;
  anything destructive or beyond the issue's stated schema scope still
  escalates to a human.
- MCP tool surface (tool names, schemas, read-only guarantee)
- `LOOP.md`, `loop-constraints.md`, `loop-budget.md` (the loop must not
  rewrite its own rules)

## Code

- Verification gate before any PR: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test` all green.
- Never disable, skip, or weaken tests to get green.
- One issue per worktree, one worktree per run. No unrelated refactors.
- Max 3 fix attempts per issue across all runs; then label `needs-info`,
  record in #19, and stop working that issue.

## Privacy & Egress

- No new network egress beyond GitHub (`gh` CLI / GitHub API).
- Never write GitHub tokens or secrets into code, config, fixtures,
  logs, state files, or issue comments.
- Do not log private repo content into loop state.

## Budget

- Respect `loop-budget.md` daily caps. On exceed: stop, append event to
  #19, note in #18.

---
<!-- Add new rules in plain English. Loops read this file verbatim. -->

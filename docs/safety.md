# qgh Loop Safety

Loops amplify both good and bad judgment. qgh starts at L1 report-only and keeps GitHub Issues as the tracker source of truth.

## Tracker Boundary

- GitHub Issues for `juicyjusung/qgh` are authoritative for issue-backed scope, labels, blockers, final verification, and loop runtime state.
- #18 stores current loop state. #19 stores append-only run history.
- `STATE.md`, `issue-triage-state.md`, and `loop-run-log.md` are static pointers only.
- L1 loops may edit/comment only #18/#19 for loop state. They must not edit product/implementation issue bodies, labels, milestones, assignees, closures, or state.
- If local pointer files disagree with GitHub, GitHub wins.

## Path Denylist For Autonomous Changes

Loops must never auto-edit these without explicit human approval:

- `qgh-prd.md`
- `qgh-product-brief.md`
- `CONTEXT.md`
- `docs/adr/`
- `docs/agents/`
- `AGENTS.md`
- `CLAUDE.md`
- Any config, fixture, log, cache, DB, or env file containing tokens or private GitHub content

## Product Guardrails

Escalate to human review for any change involving:

- Repo allowlist behavior
- Source identity, locators, Source Version, tombstone, or reconciliation
- Citation Contract semantics
- Strict CLI/config/MCP schema validation
- MCP tool surface, especially write-capable or sync-capable tools
- Hosted embedding, hosted rerank, telemetry, or any non-GitHub egress
- Token source handling or credential persistence

## Connector Policy

L1 uses `gh` for issue discovery and for updating #18/#19 only. No write-capable MCP connector or GitHub workflow is required for the first rollout.

## Auto-Merge Policy

No auto-merge. L1 loops do not create branches, PRs, or fixes.

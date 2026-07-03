---
name: orchestrating-qgh-worktrees
description: Use when coordinating qgh GitHub issues and isolated worktrees: choosing next issue, excluding active worktrees, recommending safe parallel batches, starting issue worktrees, verifying completed issue lanes, or routing unclear issue work before implementation.
---

# Orchestrating qgh Worktrees

## Overview

Coordinate qgh issue work from the center. GitHub Issues are the planning source of truth, #18/#19 hold loop operating state, and git worktrees are temporary execution lanes.

**Core principle:** inventory first, then route. Do not recommend, create, close, comment, or implement from memory.

## Required Context

Read `references/operations.md` before creating or checking worktrees, posting GitHub issue updates, verifying completed issue work, or recommending a parallel batch.

Load related skills only when the request crosses into their domain:

| Situation | Skill |
| --- | --- |
| Worktree setup or isolation decision | `using-git-worktrees` |
| Unclear issue, open D2, or pre-implementation design sharpening | `issue-grill-with-docs` |
| Starting actual implementation | `implement` |
| Incoming raw bug/request triage | `triage` |
| Splitting oversized scope into vertical slices | `to-issues` |
| Unsure which qgh flow applies | `ask-matt` |

## L2 Implementation Lane (autonomous dispatch)

An automated lane exists (`scripts/loop/qgh-loop.sh`, LOOP.md "L2
Implementation Lane"): a dispatcher fills up to 3 parallel worker lanes
from open issues labeled `ready-for-agent` (oldest first), each worker
running maker -> gates -> checker -> draft PR in `.worktrees/issue-<n>`
on branch `agent/issue-<n>`.

Consequences for this skill:

- `ready-for-agent` label = dispatch queue. Applying it IS the routing
  action; a "safe parallel batch" is delivered by labeling that set.
- `needs-info` = parked after a failed lane run; requires human
  re-label to retry (check #19 for the failure reason first).
- `agent/issue-<n>` branches, `.worktrees/issue-<n>` dirs, and
  `.worktrees/.claim-<n>` dirs are lane-owned. Treat them as `active`.
  Never create a manual worktree for an issue that is labeled
  `ready-for-agent` or lane-active — remove the label first if a human
  session must take the issue over.
- Worker logs: `~/Library/Logs/qgh-loop/issue-<n>.log`; dispatcher log:
  `~/Library/Logs/qgh-loop.log`.

## qgh Boundaries

- Tracker SSOT is GitHub Issues for `juicyjusung/qgh`.
- #18 is loop state. #19 is append-only loop run history.
- Local `STATE.md`, `issue-triage-state.md`, and `loop-run-log.md` are static pointers only.
- `.scratch/` is not a tracker for qgh unless the user explicitly asks for local markdown.
- GitHub-facing writing is Korean by default.
- Product scope stays on GitHub Issues, issue comments, and Wiki retrieval.
- MCP v1 remains read-only: `query`, `get`, and `status` only.
- BM25-only must remain a complete path.
- Do not store tokens or private repo content in config, fixtures, logs, or issue comments.

## Routing Workflow

1. Inventory git state, existing worktrees, #18/#19, and relevant open issues.
2. Mark active issues from existing branches/worktrees and explicit user status.
3. Classify candidates as `active`, `ready`, `needs-grill`, `human-needed`, `blocked`, or `merge-risk`.
4. Exclude #18/#19 from product implementation candidate lists.
5. Recommend a small batch, usually 1-3 lanes for qgh.
6. Create worktrees only after duplicate checks and the `using-git-worktrees` flow.
7. For completed issue lanes, verify issue acceptance criteria, diff, tests, and issue updates before reporting done.

## Classification

| Bucket | Criteria |
| --- | --- |
| `active` | Existing issue branch/worktree (including lane-owned `agent/issue-<n>`, `.claim-<n>`), open draft PR, recent explicit user status |
| `parked` | Labeled `needs-info` by a failed lane run — read the #19 failure entry before recommending re-label |
| `ready` | Clear acceptance criteria, blockers closed, D2 decisions resolved or explicitly accepted |
| `needs-grill` | Ambiguous contract, missing acceptance criteria, unresolved D2, or risky qgh domain decision |
| `human-needed` | Scope, ADR, privacy, schema, MCP surface, token, hosted egress, or release-gate judgment |
| `blocked` | Body/comments name open blocker or upstream issue is active |
| `merge-risk` | Candidate likely edits same shared contract, schema, docs, tests, or module as active work |

## qgh Dependency Heuristics

Be conservative around:

- source identity, locators, Source Version, tombstones, and reconciliation
- CLI/config/MCP schemas and structured errors
- local DB schema, FTS migration, storage paths, and permissions
- citation/result round-trip contracts
- privacy defaults, token source handling, and hosted egress
- #16 release validation targets

Common early ordering:

1. Configuration and repo allowlist base work before sync/query features.
2. Source identity before issue/comment/wiki sync details.
3. `query -> get -> cite` contracts before ranking polish.
4. Status/error envelopes before release validation.

## Parallel Batch Rules

Recommend parallel lanes only when all are true:

- No open blocker or unresolved D2 controls the candidate.
- No active worktree already owns the issue.
- Candidates do not edit the same contract, schema, fixture set, or source module.
- Each lane can finish with its own tests and issue update.

Avoid parallel implementation when issues share qgh cross-cutting surfaces such as `source_id`, `source_version`, strict schema validation, status output shape, DB migration, or Citation Contract semantics.

## Output Contract

For next-work or status requests, report:

- current git branch and worktree state
- active issue lanes
- safe candidates with reasons
- blocked/merge-risk candidates with reasons
- recommended next actions
- `Issue update not needed: <reason>` unless GitHub truth changed

For worktree creation, report:

- issue number
- branch
- path
- base commit
- baseline verification status
- `Issue update not needed: worktree creation only.` unless an issue comment/state changed

For completion verification, end with:

- `Updated #N: <reason>` if the owning issue changed
- `Issue update not needed: <reason>` if nothing needed to change

## Red Flags

Stop and gather more context when:

- #18/#19 show loop pause or a conflicting current state.
- The issue has unresolved D2/D3 comments.
- A candidate depends on an open or active issue.
- The work would broaden MVP scope beyond Issues, issue comments, and Wiki.
- The work would add write-capable MCP behavior.
- The work would weaken source identity or citation round-trip guarantees.
- Two candidate lanes touch the same schema, storage, or output contract.
- A worktree is dirty or behind relevant `main` changes.

# qgh Issue/Worktree Operations

Use the smallest command set that answers the user's question. Parallelize independent reads when practical.

## Inventory Commands

```bash
rtk git status --short --branch
rtk git branch --show-current
rtk git log --oneline --decorate -8
rtk git worktree list
rtk gh issue list --state open --limit 100 --json number,title,state,labels,assignees,updatedAt
rtk gh issue view 18 --comments --json number,title,body,comments,state,updatedAt,url
rtk gh issue view 19 --comments --json number,title,body,comments,state,updatedAt,url
```

For a specific issue:

```bash
rtk gh issue view <N> --comments --json number,title,state,stateReason,labels,body,comments,closedAt,url
```

Search code, docs, and tests for issue terms before classifying overlap:

```bash
rtk rg -n "<term|contract|module>" .
```

## Classification Playbook

| Bucket | How to decide |
| --- | --- |
| Active | `git worktree list`, issue branch, recent user status, or PR says work is in progress |
| Ready | Acceptance criteria are clear, blockers closed, no unresolved D2/D3 controls the issue |
| Needs-grill | The next step is a product/contract decision, not code |
| Human-needed | Scope, ADR, privacy, token, hosted egress, MCP write surface, or release gate |
| Blocked | Issue body/comments name open blocker or active dependency |
| Merge-risk | Candidate touches same shared contract, schema, fixtures, or module as active work |

## Starting a Worktree

Use `using-git-worktrees` before creating an implementation lane. That skill owns isolation detection, native tool preference, `.worktrees/` selection, ignore checks, setup, and baseline tests.

Before creation:

1. Read the owning issue and current comments.
2. Check #18/#19 for loop pause or active-state conflicts.
3. Run `rtk git status --short --branch`.
4. Run `rtk git worktree list`.
5. Check whether an issue branch/worktree already exists.
6. Confirm the issue is not blocked and does not require `issue-grill-with-docs`.

Branch convention:

```text
issue-<number>-<short-kebab-title>
```

Manual fallback command shape, only when no native worktree tool is available and `using-git-worktrees` allows fallback:

```bash
rtk git worktree add .worktrees/issue-<number>-<short-kebab-title> -b issue-<number>-<short-kebab-title>
```

If the user asks for "worktree only", create/check the lane and stop. Do not start implementation.

## Checking Completed Work

When the user says an issue lane is complete:

1. Inspect the owning issue, latest comments, labels, and acceptance criteria.
2. Inspect branch/worktree state and recent commits.
3. Verify changed scope against the issue and qgh guardrails.
4. Run targeted tests and formatting checks. For docs-only work, inspect rendered/diffed markdown and path references.
5. Search open issues for blockers depending on the completed issue.
6. Update the owning issue only when project truth changed.
7. Update #18/#19 only when loop operating state changed.
8. Report stale branches/worktrees, but do not delete them unless asked.

Useful checks:

```bash
rtk git diff --check
rtk git status --short
```

Use project-specific test commands from the repo once identified. Do not invent success when checks were not run.

## Safe Parallel Batch Recommendation

Recommended output:

```markdown
**현재 상태**
- branch/worktree 상태
- active issue lanes

**안전 후보**
- #N <title>: <why independent>

**대기/위험**
- #N <title>: <blocker, D2, or merge-risk>

**추천**
1. <first action>
2. <second action>

Issue update not needed: <reason>
```

Batch size should usually be 1-3 for qgh. Prefer fewer lanes when work touches shared contracts or release criteria.

## Issue Updates

Use GitHub Issues as append-only decision trail. Keep comments concise and Korean by default.

Do not post:

- full transcripts
- local failed attempts
- secrets, tokens, config values, private content, or sensitive snippets
- speculative status that was not verified

Do not edit issue body, labels, milestones, assignees, state, or closure unless the user explicitly asked and the relevant flow allows it.

Completion/status line:

```markdown
Updated #N: <what changed or why verified>
```

No-change line:

```markdown
Issue update not needed: <reason>
```

## Common Mistakes

| Mistake | Fix |
| --- | --- |
| Recommending from memory | Re-read git worktrees, #18/#19, and relevant issues |
| Treating #18/#19 as implementation issues | Exclude them from product candidate batches |
| Starting implementation with unresolved D2 | Route to `issue-grill-with-docs` or ask for confirmation |
| Ignoring qgh MVP guardrails | Check PRD/AGENTS/LOOP before widening scope |
| Over-parallelizing contracts | Keep shared schema/source identity/citation work serial |
| Updating local state files dynamically | Update GitHub issues; local loop files are pointers |
| Closing from "looks done" | Verify acceptance criteria, tests, and dependent blockers |

---
name: loop-constraints
description: >
  Read loop-constraints.md at the start of every run and enforce every rule.
  This skill runs BEFORE triage or any action skill. Constraints are binding.
user_invocable: true
---

# Loop Constraints Enforcer (qgh)

You are the guardrail. Before any other work begins, you MUST:

1. Read `loop-constraints.md` from the project root.
2. Load every rule into your working memory.
3. Kill switch: if GitHub issue #18 body has a line starting with
   `Loop status: paused`, exit immediately without acting or writing state.
4. Apply these rules to EVERY action that follows.

## How to enforce

- Before pushing: re-read Push & Merge. Draft PR only; never push `main`;
  never merge.
- Before editing a file: re-read Denylist Paths. Match -> ESCALATE_HUMAN.
- Before proposing a fix: re-read Code. Gates must be green; one issue per
  worktree.
- Before any GitHub write: re-read Issues & Labels. State writes only to
  #18/#19; label changes only per lane policy.
- Always: Privacy & Egress — no egress beyond GitHub, no tokens/private
  content in state or logs.

## Output at start of run

Always begin with a one-line confirmation:

```
Constraints loaded from loop-constraints.md: N rules active.
```

If `loop-constraints.md` is missing, stop and escalate — do not proceed
with defaults.

## Interaction with other skills

- `loop-triage` / `issue-triage` — constraints override triage priority.
- `loop-budget` — constraints may impose stricter budget than
  `loop-budget.md`.
- `.codex/agents/verifier.toml` — denylist paths here are part of the
  verifier checklist.

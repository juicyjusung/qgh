---
name: qgh
description: "Operate qgh and retrieve or cite its local GitHub Issue/comment evidence safely. Always invoke this skill, even for a simple request, when the user asks to run or explain a qgh command, interpret qgh output, install/configure/sync/repair the qgh CLI, investigate repository Issue/comment history, or refers to a repo-scoped Issue such as #47, an Issue/comment URL, or `gh issue` — including an implementation request anchored to that Issue. Invocation is a routing check: live-only state and writes may go to `gh` without running qgh. When qgh helps, preserve the query, get, and cite sequence; snippets are not evidence. Do not trigger for PR-only numbers, Markdown headings, `gh pr`/`gh auth`/`gh release`, a source/docs edit with no Issue or history signal, installing this Agent Skill, code or Wiki search, generic web research, or an explicit no-lookup request."
---

# qgh

Use qgh as a local Issue/comment evidence layer, not as an answer generator. Preserve `query -> get -> cite`: query results are candidates, and only an opened source can support a finding.

## Route the Request

Choose the smallest route that completes the task. Read only its reference:

| Need | Reference |
| --- | --- |
| One decision, issue, comment, or small evidence set | [references/retrieval.md](references/retrieval.md) |
| Decision archaeology, root-cause synthesis, or an implementation/review brief | [references/evidence-research.md](references/evidence-research.md), after the retrieval contract |
| Missing binary, initialization, sync, coverage, model, readiness, or repair | [references/setup-and-recovery.md](references/setup-and-recovery.md) |

For a mixed task, complete setup only to the explicitly authorized state, then return to retrieval. Changing routes never grants additional authorization.

## Issue-Aware Triggering

Invoking this skill decides whether qgh adds value; it does not require a qgh command to run.

- Treat a repository-scoped `#N`, GitHub Issue/comment URL, or `gh issue` task as an Issue-context signal, even when qgh is not named.
- An implementation or edit request anchored to that Issue still invokes this routing check; retrieve only the Issue context that helps the separate code task.
- Use qgh when the task benefits from synchronized content, historical rationale, comments, or source-backed context.
- For live-only state or an authorized Issue write that needs no historical context, use `gh` and do not run qgh merely because this skill was invoked.
- For mixed history/current-state work, keep qgh evidence and live `gh` evidence as separate layers.
- Do not assume a PR number, Markdown heading, generic numeric label, source line number, or pasted text is an Issue lookup. Honor an explicit request not to search.

## Authorization Boundary

The following local, read-only checks may run when they help answer the request:

- `command -v qgh`
- `qgh --version` and `qgh --help`
- `qgh status --json`
- `qgh query ... --json`
- default local `qgh get ... --json`

Installing qgh, `gh auth status`, `qgh init`, `qgh sync`, `qgh sync --backfill`, `qgh doctor`, `qgh model install`, `qgh embed --force`, and `qgh get --verify-lifecycle` require explicit authorization for that operation. An explicit request to perform the operation counts; a request to explain, retrieve, research, or diagnose does not. Before execution, state the relevant network access, local writes, possible purge, download, or resource cost. If the operation was not requested, present the exact next command without running it.

Never request or persist a literal GitHub token. Use only token source references. Treat local databases, search indexes, queries, bodies, snippets, embeddings, caches, logs, and local paths as sensitive derivative data. Do not copy or share those artifacts in fixtures, issues, benchmarks, or diagnostics.

## Retrieval Invariant

1. Confirm the binary and inspect `qgh status --json`.
2. Verify explicit profile/repository scope, freshness, coverage, warnings, and retrieval fences. BM25 remains a complete path without embeddings.
3. Query exact evidence first. Empty results mean only “no match in this local snapshot.” Ranking is ordering, not confidence.
4. Open every relied-on result using both values emitted by that same result:

   ```sh
   qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
   ```

5. Cite the full `get` body's canonical URL and source version. Never cite a query snippet as evidence.
6. Separate recorded evidence, inference, contradictions, and snapshot limits. Local status is not current GitHub truth.

If the executable or a usable publication is missing, stop retrieval and follow the setup reference. Do not fabricate evidence or silently switch to GitHub search.

## Tool Boundary

Use qgh only for the configured local snapshot of GitHub Issues and comments. Use `gh` separately for live GitHub state and authorized mutations. Do not blur qgh history, current code/worktree observations, and live GitHub evidence into a single claim.

## Output

Match the chosen route:

- Direct retrieval: a compact finding with canonical citation, source version, freshness, coverage, and separated inference.
- Research: a decision-ready brief with recorded evidence, interpretation, contradictions or gaps, and a supported next step.
- Setup/recovery: the content-free state, one exact next command, its side effects, and what remains unauthorized.

Do not persist raw queries, bodies, tokens, complete JSON envelopes, or user-local paths in fixtures, issue comments, benchmarks, or diagnostic logs.

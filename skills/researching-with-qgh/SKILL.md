---
name: researching-with-qgh
description: Investigate product and engineering questions with multiple qgh Issue and comment sources, then produce an evidence-backed brief. Use for decision archaeology, root-cause analysis, implementation planning, architecture work, or reviews that depend on historical repository rationale. Use a simpler retrieval workflow for one source; do not use for live-only GitHub state, GitHub writes, code search, or generic web research.
---

# Researching with qgh

Turn locally synchronized GitHub Issue and comment history into a traceable research brief. This skill is for synthesis across sources; it does not turn snippets into facts or local snapshots into live GitHub truth.

Read [references/evidence-brief.md](references/evidence-brief.md) before starting an investigation.

## Safety Boundary

Apply the same retrieval boundary as `using-qgh-context`, even when that sibling skill is not installed:

- Automatic read-only operations are limited to `command -v qgh`, `qgh status --json`, `qgh query ... --json`, and default local `qgh get ... --json`.
- Never automatically run `init`, `sync`, `doctor`, `embed`, `model install`, or `get --verify-lifecycle`.
- Never use qgh for code, pull requests, Wiki, live-only issue state, or GitHub mutations.
- Never persist raw private queries, source bodies, tokens, full JSON responses, or local storage paths in research artifacts.

For every authoritative source, use exactly:

```sh
qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
```

Do not substitute `--profile`, `status.meta.profile_id`, or a value inferred from the current directory. Do not invent qgh Issue/PR/code subcommands; qgh research is built from `status`, `query`, and `get` over Issues and comments.

If qgh is unavailable, uninitialized, retrieval-fenced, or has no usable publication, stop and provide an explicit setup handoff. Do not fabricate historical evidence or silently switch to GitHub search.

## Investigation Workflow

1. **Frame one research question.** Define the decision, failure, contract, or change being investigated and what evidence would disconfirm the leading interpretation.
2. **Inspect the local snapshot.** Run `qgh status --json`. Record the resolved profile/repo, freshness, coverage, and relevant warnings. A partial or stale snapshot can support qualified findings but not exhaustive claims.
3. **Build a small query matrix.** Start with exact issue/comment URLs, issue numbers, identifiers, error codes, or contract names. Add two to four concise semantic variants for rationale and consequences. Keep repository scope explicit. Use only terms derived from the current question; never copy a reference example as an unrelated live query.
4. **Collect candidates.** Run JSON queries and select sources by relevance to the research question, not ranking score magnitude. Scores are ordering signals, not confidence.
5. **Open every relied-on source.** Use each result's exact `get_args.source_id` and `get_args.profile_id` with the command above. Read the authoritative body and source version. For a comment, inspect its parent issue context when the conclusion depends on it.
6. **Triangulate.** Prefer at least two relevant sources when available. Look for later corrections, conflicting comments, superseding decisions, and gaps caused by coverage or freshness.
7. **Separate layers.** Label retrieved facts, inference, contradictions, and unknowns separately. Include current-code observations only when the user explicitly asks for implementation comparison or they are necessary to answer the question. qgh evidence explains recorded history; it does not prove the current implementation matches it.
8. **Add live truth only when the question requires it and the user authorizes it.** Do not propose `gh`, git history, or worktree inspection as routine “optional” checklist items for a historical question. When current issue state, code comparison, or a write is genuinely in scope, use the separate tool explicitly and never blend it into a qgh citation without naming the different source and time boundary.
9. **Produce a decision-ready brief.** Cite canonical URLs and source versions. Recommend next steps proportionate to the evidence and explicitly list unresolved gaps. Return the brief, not a verbose replay of the entire retrieval procedure.

## Stop Conditions

Stop and return a qualified gap instead of continuing when:

- the intended repository or profile cannot be resolved explicitly;
- the snapshot has never synced or retrieval is fail-closed;
- an authoritative candidate cannot round-trip through `get`;
- the question requires current GitHub truth but no live check is authorized;
- the available evidence is ambiguous and a second source or human decision is required.

## Output Contract

Use this shape:

```text
Question: <single research question>
Snapshot: profile/repo; freshness; coverage; live GitHub checked=no

Recorded evidence
- <fact> — <canonical_url> (github_updated_at=<value>)

Interpretation
- <inference tied to named evidence>

Contradictions or gaps
- <conflict, missing source, partial coverage, or current-state gap>

Decision / next step
- <action supported by the evidence>
```

For implementation or review work, add a short acceptance/verification checklist derived from the cited contracts, not from ranking snippets.

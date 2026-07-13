---
name: using-qgh-context
description: Retrieve and cite a specific repository decision, issue-history item, comment, or small evidence set from qgh's local GitHub snapshot. Use for direct historical lookups and lightweight citation support, even when the user does not explicitly mention qgh. For multi-source decision archaeology, root-cause synthesis, or an implementation brief, use researching-with-qgh instead. Do not use for live-only state, GitHub writes, installation, or repair.
---

# Using qgh Context

Use qgh as a local evidence layer, not as an answer generator. Preserve `query -> get -> cite`: search results are candidates, and the authoritative source must be opened before relying on or citing it.

Keep this skill focused on a direct lookup or a small citation set. When the user asks to triangulate several discussions, trace a root cause, or create an implementation/architecture brief, hand off to `researching-with-qgh`; this skill can remain its source-opening subroutine.

## Safety Boundary

The core retrieval workflow may run only these content-reading checks without additional authorization:

- `command -v qgh`
- `qgh status --json`
- `qgh query ... --json`
- local-only `qgh get ... --json`

Never automatically run `init`, `sync`, `doctor`, `embed`, `model install`, or `get --verify-lifecycle`. Those operations can write configuration, contact external services, load models, rebuild data, or purge confirmed unavailable content. If one is needed, explain why and route the user to an explicit setup or operator step.

Use qgh for locally synchronized GitHub Issues and issue comments. Do not use it for code search, pull requests, Wiki content, organization discovery, current GitHub truth, or GitHub writes. Use `gh` for live GitHub state and mutations when the user's request authorizes them.

## Retrieval Workflow

1. **Confirm qgh is available.** Run `command -v qgh`. If absent, stop. Do not install it. Give the user a standalone handoff: the CLI guide is `https://github.com/juicyjusung/qgh#install`, and a compatible agent can add the operator workflow with `npx skills add juicyjusung/qgh --skill setting-up-qgh`. State only that retrieval could not run; executable absence does not prove that no older snapshot files exist.
2. **Inspect local readiness.** Run `qgh status --json`. Confirm the resolved `meta.profile_id` and `meta.repo` match the intended scope. Stop if no usable publication exists or retrieval is fenced. A missing embedding model does not block the complete BM25 path. The status profile is a readiness check, not a value to reuse in a later `get` command.
3. **Record evidence limits.** Read freshness, coverage, and warnings before searching. `remote_checked: false` means the status is local metadata, not a live GitHub check. `coverage.mode: partial` means useful results may exist but the history is not exhaustive.
4. **Search exact evidence first.** Start with an issue URL, comment URL, issue number plus explicit repo, or distinctive identifier. Then try one to three short semantic variants only if needed. Never broaden to implicit organization-wide search.
5. **Treat every hit as a candidate.** Run `qgh query '<terms>' --json`. An empty successful result set is evidence of no match in the available local snapshot, not proof that no GitHub source exists. Ranking fields order candidates; they are not confidence or probability.
6. **Open selected sources.** Copy both values from the same query result's `get_args` and run:

   ```sh
   qgh get '<source_id>' --profile-id '<profile_id>' --json
   ```

   Never replace `get_args.profile_id` with `status.meta.profile_id`, a CLI flag chosen from memory, or a value inferred from the current directory. Use the full body, canonical URL, source identity, and source version from `get`. Never cite a query snippet as evidence.
7. **Separate evidence from inference.** State what the source records, then label any interpretation. Preserve contradictions instead of silently reconciling them.
8. **Report the snapshot boundary.** Include freshness and partial-coverage caveats. If current GitHub state matters, say that a separate live `gh` check is required; do not imply qgh already performed it.

Read [references/retrieval-contract.md](references/retrieval-contract.md) before executing the workflow or interpreting qgh JSON.

## Output Contract

Return a compact evidence note:

```text
Finding: <what the retrieved source establishes>
Evidence: <repo>#<issue> — <canonical_url>
Evidence basis: full get body; query snippet not used
Source version: github_updated_at=<value>; lifecycle_state=<value>
Snapshot limits: freshness=<decision>; coverage=<mode>; live GitHub not checked
Inference: <clearly separated interpretation, or "none">
```

For multiple sources, repeat the evidence line and call out disagreement or missing evidence.

## Privacy

Treat local databases, snippets, embeddings, cache paths, query text, and full source bodies as sensitive derivative data. Do not persist raw queries, bodies, tokens, complete JSON responses, or user-local paths in fixtures, issue comments, benchmark artifacts, or diagnostic logs. Log only content-free error codes, warning codes, and aggregate counts when durable diagnostics are necessary.

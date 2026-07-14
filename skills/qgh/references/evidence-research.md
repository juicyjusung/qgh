# qgh Evidence Research

Use this reference after [retrieval.md](retrieval.md) when the task needs synthesis across multiple Issue/comment sources. It does not turn snippets into facts or a local snapshot into live GitHub truth.

## Frame the Investigation

Define one decision, failure, contract, or change being investigated. State what evidence would disconfirm the leading interpretation. Keep the repository scope explicit.

Build only the query rows relevant to that question:

| Evidence need | Query form | Example shape |
| --- | --- | --- |
| Owning discussion | Exact issue URL or number plus repo | `<issue-url>` or `#47` with `--repo owner/repo` |
| Contract or error | Stable identifier | `freshness.stale`, `query -> get -> cite` |
| Rationale | Short intent phrase | `why publication snapshot pinned` |
| Consequence/regression | Symptom phrase | `stale leakage concurrent sync` |
| Later correction | Decision plus update terms | `reranker decision correction` |

Use only terms derived from the current question. Never copy a reference example as an unrelated live query, and do not save raw command output as a research artifact.

## Collect and Open Evidence

1. Inspect the resolved profile/repo, freshness, coverage, and warnings with `qgh status --json`.
2. Search exact identifiers first, followed by two to four concise semantic variants when useful.
3. Select candidates for relevance, not score magnitude. Ranking is ordering, not confidence.
4. Open every source used in the conclusion with exactly:

   ```sh
   qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
   ```

5. For a comment, inspect its parent issue when the conclusion depends on that context.
6. Prefer at least two relevant sources when available. Look for later corrections, explicit supersession, disagreement, and snapshot gaps.

If a relied-on candidate cannot round-trip through `get`, mark it as a gap and do not synthesize it as evidence.

## Multiple Issue Sets

When the user names multiple Issue relationships or asks for a relationship map, keep the set bounded and deterministic:

1. Query each supplied repo-scoped number as an exact locator with `#N`, `--repo`, and `--issue N`; do not manufacture URLs.
2. Check each envelope's `ok` before reading `data.results`, then require the intended `entity_type`.
3. Collect only the `get_args.source_id` and `get_args.profile_id` emitted by those results.
4. Open sources sharing one profile with one batch `get` when the set contains no more than 20 IDs; split larger sets into bounded batches.
5. Check each batch item before using its full body. Build relationships from opened sources, not query snippets, guessed links, or ranking proximity.

If comments or parent context determine a relationship, retrieve and open those sources separately under the same contract. A named Issue set does not by itself authorize live `gh`, sync, lifecycle verification, or another network operation.

## Evidence Quality

Classify each opened source:

- **Direct decision:** acceptance criteria, explicit conclusion, approved design, or authoritative correction.
- **Direct observation:** reproduction, measurement, test result, or described runtime behavior.
- **Supporting context:** background, proposal, or unresolved discussion.
- **Inference:** the agent's synthesis; never present it as a quote or recorded decision.

Issue comments may record later truth than the issue body. Check timestamps and source versions, but do not assume newer always means authoritative.

## Keep Evidence Layers Separate

| Layer | What it can establish |
| --- | --- |
| qgh local snapshot | What synchronized Issue/comment sources record at represented versions. |
| Current code/worktree | What the inspected implementation currently does. |
| `gh` live check | Current GitHub state at the time of the API call. |

Do not add a live `gh` check, git history pass, or worktree inspection merely because it is available. Add one only when current state or code comparison is part of the user's authorized request, and label it as a separate source and time boundary.

## Minimum Source Capture

For every fact used in the final brief, retain only:

- repository and issue number;
- `canonical_url`;
- `source_version.github_updated_at` and lifecycle state when relevant;
- whether the source is an issue or comment;
- a concise paraphrase.

Do not persist full bodies, snippets, raw queries, tokens, complete JSON envelopes, unnecessary hashes, or local filesystem paths.

## Evidence Brief

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

For implementation or review work, add a short acceptance and verification checklist derived from cited contracts, not ranking snippets.

## Completion Checklist

- Every cited result round-tripped through `get`.
- No snippet is used as evidence.
- Canonical URLs and source versions are present.
- Freshness and coverage are reported.
- Facts, inference, contradictions, and unknowns are separated.
- Current/live state is not inferred from local freshness metadata.
- No setup, sync, doctor, model, lifecycle verification, or write operation ran without explicit authorization.

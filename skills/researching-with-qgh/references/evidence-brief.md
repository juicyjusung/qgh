# qgh Evidence Brief

Use this reference to keep multi-source investigations reproducible and honest about snapshot limits.

## Query Matrix

Build only the rows relevant to the question. The examples below illustrate query shapes; never copy their subject matter into an unrelated investigation:

| Evidence need | Query form | Example shape |
| --- | --- | --- |
| Owning discussion | Exact issue URL or number plus repo | `<issue-url>` or `#47` with `--repo owner/repo` |
| Contract or error | Stable identifier | `freshness.stale`, `query -> get -> cite` |
| Rationale | Short intent phrase | `why publication snapshot pinned` |
| Consequence/regression | Symptom phrase | `stale leakage concurrent sync` |
| Later correction | Decision plus update terms | `reranker decision correction` |

Use JSON output for every agent call. Do not save raw command output as a research artifact.

The only stable source-opening shape is:

```sh
qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
```

Never change `--profile-id` to `--profile`, and never replace the query result's profile ID with the status profile. qgh has no `issue view`, PR, or code-search command.

## Evidence Quality

Classify each opened source:

- **Direct decision:** acceptance criteria, explicit conclusion, approved design, or authoritative correction.
- **Direct observation:** reproduction, measurement, test result, or described runtime behavior.
- **Supporting context:** background, proposal, or unresolved discussion.
- **Inference:** your synthesis; never present it as a quote or recorded decision.

Issue comments may contain later truth than the issue body. Check timestamps and source versions, but do not assume newer always means authoritative. Preserve explicit supersession and disagreement.

## Source Capture

For every fact used in the final brief, retain only the minimum citation metadata in the response:

- repository and issue number;
- `canonical_url`;
- `source_version.github_updated_at` and lifecycle state when relevant;
- whether the source is an issue or comment;
- a concise paraphrase.

Do not persist full bodies, snippets, raw queries, tokens, complete JSON envelopes, hashes that are not needed for the decision, or local filesystem paths.

## Snapshot and Live Layers

State these separately:

| Layer | What it can establish |
| --- | --- |
| qgh local snapshot | What the synchronized issue/comment sources record at their represented versions. |
| Current code/worktree | What the inspected implementation currently does. |
| `gh` live check | Current GitHub state at the time of the API call. |

A robust implementation brief can compare all three, but must not collapse them into one claim.

Do not add a `gh` live check, git history pass, or current-worktree inspection merely because it is available. It belongs in the plan only when current GitHub state, code comparison, or a mutation is part of the user's question and authorized scope.

## Brief Templates

### Decision archaeology

```text
Decision being traced:
Original proposal:
Accepted/superseding decision:
Recorded rationale:
Known trade-offs:
Evidence gaps:
```

### Root-cause investigation

```text
Observed symptom:
Historical contract:
Relevant change or prior incident:
Hypothesis (inference):
Disconfirming test:
Verification checklist:
```

### Implementation or review brief

```text
Required behavior:
Non-negotiable boundaries:
Source-backed acceptance criteria:
Current-code mismatch:
Risks and unknowns:
Focused gates:
```

## Completion Checklist

- Every cited result round-tripped through `get`.
- No snippet is used as evidence.
- Canonical URLs and source versions are present.
- Freshness and coverage are reported.
- Facts and inference are separated.
- Contradictions and later corrections are visible.
- Current/live state is not inferred from local freshness metadata.
- No setup, sync, doctor, model, lifecycle verification, or write operation ran implicitly.

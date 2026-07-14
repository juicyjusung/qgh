# qgh Retrieval Contract

Use this reference for direct lookup and as the evidence-opening contract for multi-source research. The released CLI and its `qgh.v1` JSON envelopes are authoritative; human terminal wording may evolve.

## Readiness

`qgh status --json` is local-only and does not load a model or contact GitHub.

Check these fields before retrieval:

| Field | Interpretation |
| --- | --- |
| `ok` | Whether the command succeeded. Handle structured errors; do not scrape stderr text. |
| `meta.profile_id` | The local profile store that owns the snapshot. |
| `meta.repo` | The effective explicit repository scope, or `null` when none resolved. |
| `data.freshness.decision` | `fresh`, `stale_warn`, `stale_fail`, or `never_synced`. |
| `data.freshness.remote_checked` | Always `false`; freshness comes from local sync metadata. |
| `data.coverage.mode` | `partial` permits retrieval but cannot claim exhaustive history. |
| `data.coverage.next_action` | An operator recommendation, not permission to execute it. |
| `data.embedding.state` | Hybrid readiness when the optional `embedding` object is present. Its absence is normal for a BM25-only profile. |
| `data.purge.retrieval_blocked` | Stop retrieval when true and ask an operator to repair the local state. |

Do not copy local database, cache, index, or log paths from status into external reports.

## Query Planning

Prefer the smallest explicit scope:

```sh
qgh query '<issue-or-comment-url>' --json
qgh query '<distinctive identifier>' --repo owner/repo --json
qgh query '<concise terms>' --repo owner/repo --json
```

The current repository policy may resolve scope, but never infer or discover an organization-wide scope. If the intended repo is ambiguous, ask for it or use an explicitly provided `owner/repo`.

For each result:

- `source_id` is the stable qgh identity.
- `canonical_url` is navigation metadata until the source is opened.
- `snippet` is a preview, never citation evidence.
- `get_args.source_id` and `get_args.profile_id` form the stable round trip.
- `source_version` records the version represented by the local result.
- ranking fields are ordering signals, not confidence or probability.

`ok: true` with `data.results: []` means “no match in the available local snapshot,” qualified by freshness and coverage. It does not prove that no GitHub source exists.

## Get and Cite

Use the two exact round-trip arguments emitted by the same query result:

```sh
qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
```

`status.meta.profile_id` validates readiness but is not a substitute for the query result's `get_args.profile_id`. This preserves round trips across working directories and multi-profile stores.

Single-source `get` returns a full authoritative `data.source` object. Cite only after reading its `body`, `canonical_url`, `source_id`, `source_version`, and lifecycle metadata. CLI batch `get` accepts 1 to 20 source IDs and reports per-item failures.

Default `get` is local-only. Do not add `--verify-lifecycle` unless the user explicitly authorizes its GitHub request and possible purge.

## Actions and Errors

- Keep `--json` on every agent command.
- Do not execute `coverage.next_action.json_command` or `retry_action.json_command` unless the current operator task explicitly authorizes that action and scope.
- Do not silently retry rate limits or network failures.
- Do not treat stale, partial, BM25-only, or no-result states as equivalent.
- Do not expose raw source content while reporting an error.
- If the executable is absent, report only that qgh retrieval is unavailable; do not infer that no old snapshot files exist.

## qgh versus gh

| Need | Tool |
| --- | --- |
| Search synchronized issue/comment history without GitHub search quota | qgh |
| Open and cite the local authoritative source body | qgh `get` |
| Verify whether an issue is currently open, edited, transferred, or deleted | `gh` live API/CLI |
| Create, edit, comment on, close, or label an issue | `gh` |
| Search code, pull requests, Discussions, Projects, or Wiki | Another purpose-built path; not qgh |

When both historical context and current state matter, report them as separate evidence layers.

## Direct Retrieval Output

```text
Finding: <what the opened source establishes>
Evidence: <repo>#<issue> — <canonical_url>
Evidence basis: full get body; query snippet not used
Source version: github_updated_at=<value>; lifecycle_state=<value>
Snapshot limits: freshness=<decision>; coverage=<mode>; live GitHub not checked
Inference: <clearly separated interpretation, or "none">
```

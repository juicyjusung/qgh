# qgh Setup and Recovery

Use the smallest path that reaches the user's requested state. Agent skill installation does not install the qgh binary, create a profile, authenticate GitHub, download a model, or synchronize any content.

## First-Time Setup

Published binaries currently support macOS Apple Silicon and Linux x86_64 with glibc 2.38 or newer:

```sh
brew install juicyjusung/tap/qgh
qgh --version
```

Confirm GitHub CLI authentication for the intended host without copying credentials into qgh config. Treat this as a possible network check and respect the user's authorization boundary:

```sh
gh auth status --hostname github.com
```

For GitHub Enterprise, replace the host. Then initialize explicit repo/profile scope:

```sh
qgh init
```

For an environment token source, pass the variable name, never its value:

```sh
qgh init --token-source env --token-env GITHUB_TOKEN
```

`qgh init -y` accepts inferred defaults non-interactively. Use it only when automation was explicitly requested and the inferred host, repo, profile, and paths have been reviewed.

## Retrieval Modes

| Desired state | Required action |
| --- | --- |
| Complete lexical retrieval | No model install; synchronize and use BM25. |
| Hybrid semantic candidates | Explicitly install and configure the supported Qwen embedding preset, then sync. |
| Optional top-candidate reranking | Explicitly install/configure the reranker and request `--rerank`; it cannot add a missing source. |

Supported model acquisition is explicit:

```sh
qgh model install qwen3-embedding-0.6b
qgh model install qwen3-reranker-0.6b
```

These commands contact Hugging Face for pinned public artifacts. They do not send repository content, metadata, embeddings, or queries. Model weights are not bundled and are never acquired by `sync`, `query`, `get`, `status`, `doctor`, or MCP.

## Sync, Coverage, and Backfill

Normal sync is the standard refresh path:

```sh
qgh sync
```

It contacts the configured GitHub host, applies current issue/comment changes, rebuilds BM25, and incrementally refreshes configured embeddings that are missing or changed.

Interpret `coverage.mode: partial` as “searchable but not exhaustive.” Follow qgh's scope-preserving recommendation in order:

```sh
qgh sync --all --profile PROFILE
qgh sync --backfill --all --profile PROFILE
```

The first command completes open coverage for the profile. Each backfill command performs one bounded historical pass. Repeat only when the new status/action recommends another pass. Review `coverage.next_action.command` for a person or `json_command` for automation; neither is implicit authorization.

Freshness and coverage are independent. A recent snapshot can still have partial historical coverage. A complete corpus can later become stale.

## Troubleshooting Matrix

| State or symptom | Interpretation | Smallest next step |
| --- | --- | --- |
| qgh command missing | Binary is not installed or not on `PATH`. | Explain/install the Homebrew binary only with authorization. |
| profile/config missing | No explicit local snapshot scope exists. | Preview and run `qgh init` only when authorized. |
| `freshness.decision: never_synced` | Profile exists but no successful snapshot is published. | Preview an explicit scoped `qgh sync`. |
| `freshness.decision: stale_warn` | Retrieval is available from old local data. | Report caveat; sync only if requested. |
| `coverage.mode: partial` | Available history is incomplete. | Follow the exact open-coverage or backfill action. |
| embedding missing/invalid | Hybrid is unavailable. | Continue with BM25 or explicitly install/repair the configured model. |
| embedding refresh failed during sync | BM25 publication may still be usable. | Inspect status; do not jump directly to a forced rebuild. |
| full vector repair required | Incremental sync is insufficient for the diagnosed vector state. | Use `qgh embed --force` only after diagnosis and explicit authorization. |
| retrieval fenced/purge pending | qgh is intentionally fail-closed. | Stop query/get and follow operator remediation; do not bypass the fence. |
| retry/backoff action present | GitHub requested bounded retry timing. | Preserve the exact action and wait; do not silently hammer the API. |

Use `qgh doctor` only when the user wants an explicit GitHub connectivity and model-runtime probe. It is not a harmless alias for status.

`status` exposes content-free readiness, coverage, freshness, embedding, and purge state. It cannot name the exact source or chunk behind an embedding error. Use only released commands shown by `qgh --help`; qgh has no `auth`, `log`, or `download` subcommand, no `embed --only` option, no `.qghignore` contract, and no code-file indexing scope.

After setup, `qgh query --json` and default `qgh get --json` remain local-only. A normal first search therefore needs no GitHub request after the snapshot exists: query candidates, open the selected result with `qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json`, and cite the full source rather than the snippet.

## Privacy Checks

- Never place token values in config, shell examples, logs, or reports.
- Do not paste private source bodies, raw queries, full command envelopes, or user-local paths into an issue or fixture.
- Treat the database, Tantivy index, snippets, embeddings, logs, and cache as sensitive derivative data.
- qgh-managed purge does not delete user-created filesystem backups outside qgh generation paths.

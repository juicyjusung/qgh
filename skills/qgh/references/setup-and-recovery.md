# qgh Setup and Recovery

Use the smallest path that reaches the user's requested state. Agent Skill installation does not install the qgh binary, create a profile, authenticate GitHub, download a model, or synchronize content.

## Operating Rule

Start with read-only checks. Run a side-effecting operation only when the current request explicitly authorizes it; otherwise preview the exact command, affected profile/repo, network destination, local writes, possible purge, and resource cost.

Do not ask for or persist a literal GitHub token. qgh configuration stores a `github_cli` or environment-variable source reference, never the token value.

Keep these layers distinct:

1. Agent Skill instructions.
2. The Homebrew qgh binary.
3. Profile and repository configuration created by `qgh init`.
4. Optional local model weights for hybrid retrieval.
5. The local snapshot created or refreshed by `qgh sync`.

## First-Time Setup

Published binaries currently support macOS Apple Silicon and Linux x86_64 with glibc 2.38 or newer:

```sh
brew install juicyjusung/tap/qgh
qgh --version
```

Confirm GitHub CLI authentication for the intended host without copying credentials into qgh config. Treat this as a possible network check:

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

Use `qgh init -y` only when automation was explicitly requested and the inferred host, repo, profile, and paths have been reviewed.

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

Normal sync contacts the configured GitHub host, applies current Issue/comment changes, rebuilds BM25, and incrementally refreshes configured embeddings that are missing or changed:

```sh
qgh sync
```

Interpret `coverage.mode: partial` as “searchable but not exhaustive.” Follow the status-emitted, scope-preserving recommendation in order. Typical released shapes are:

```sh
qgh sync --all --profile PROFILE
qgh sync --backfill --all --profile PROFILE
```

The first command completes open coverage for the profile. Each backfill command performs one bounded historical pass. Repeat only when the new status/action recommends another pass. `coverage.next_action.command` and `json_command` are proposals, not implicit authorization.

Freshness and coverage are independent. A recent snapshot can have partial historical coverage; a complete corpus can later become stale.

## Troubleshooting Matrix

| State or symptom | Interpretation | Smallest next step |
| --- | --- | --- |
| qgh command missing | Binary is not installed or not on `PATH`. | Explain or install the Homebrew binary only when authorized. |
| profile/config missing | No explicit local snapshot scope exists. | Preview and run `qgh init` only when authorized. |
| `freshness.decision: never_synced` | Profile exists but no successful snapshot is published. | Preview an explicitly scoped `qgh sync`. |
| `freshness.decision: stale_warn` | Retrieval is available from old local data. | Report the caveat; sync only if requested. |
| `coverage.mode: partial` | Available history is incomplete. | Follow the exact open-coverage or backfill action. |
| embedding missing/invalid | Hybrid is unavailable. | Continue with BM25 or explicitly install/repair the configured model. |
| embedding refresh failed during sync | BM25 publication may still be usable. | Inspect status; do not jump directly to a forced rebuild. |
| full vector repair required | Incremental sync is insufficient for the diagnosed vector state. | Use `qgh embed --force` only after diagnosis and authorization. |
| retrieval fenced/purge pending | qgh is intentionally fail-closed. | Stop query/get and follow operator remediation; do not bypass the fence. |
| retry/backoff action present | GitHub requested bounded retry timing. | Preserve the exact action and wait; do not silently hammer the API. |

Use `qgh doctor` only when the user wants the GitHub connectivity and model-runtime probe. It is not a harmless alias for status. Status exposes content-free readiness and aggregate state; it cannot identify an exact offending source or chunk.

Use only commands and options confirmed by the installed `qgh --help` or released reference. Do not invent `qgh auth`, `qgh log`, `qgh download`, `qgh embed --only`, `.qghignore`, or code indexing.

After an authorized operation, re-run `qgh status --json`. When lexical retrieval is ready, return to `query -> get -> cite` with:

```sh
qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json
```

Do not claim live GitHub correctness from local status.

## Setup Output

```text
State: <missing | uninitialized | ready-bm25 | ready-hybrid | partial | stale | repair-needed>
Evidence: <content-free status/error fields>
Next command: <one exact command>
Side effects: <network and local writes>
Why this command: <short rationale>
Not doing yet: <operations that still need authorization>
```

Do not reproduce tokens, raw bodies, raw queries, complete JSON responses, or user-local storage paths.

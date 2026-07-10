# Error Codes

qgh machine-readable output uses the `qgh.v1` envelope for success and failure. No-result query responses are successful: `ok: true` with `data.results: []`.

Stable error families:

- `config.*`: profile and TOML configuration failures.
- `validation.*`: CLI/schema/argument validation failures.
- `freshness.*`: local snapshot freshness failures.
- `auth.*`: token source failures.
- `github.*`: GitHub request failures outside structured backoff state.
- `embedding.*`: local embedding preparation and source-snapshot failures.
- `source.*`: missing or tombstoned source lookups.
- `purge.*`: fail-closed purge, retry, publication, and read/write-fence failures.
- `publication.*`: retrieval snapshot CAS, provenance, and artifact-readiness failures.
- `storage.*`: SQLite or local filesystem storage failures.
- `index.*`: Tantivy index failures.
- `internal.*`: unexpected internal failures.

Common codes include `config.no_matching_profile`, `config.ambiguous_profile`,
`config.invalid_repo_policy`, `validation.cli`, `validation.mcp`,
`validation.unsupported_filter`, `validation.batch_size`, `freshness.stale`,
`auth.token_unavailable`, `source.not_found`, `source.tombstoned`,
`source.outside_effective_scope`, `purge.failed`, `purge.retry_failed`,
`purge.read_fenced`, and `purge.write_fenced`.

When a confirmed lifecycle or explicit allowlist-removal purge is incomplete,
the affected source, issue, or repository remains fail closed. Retrieval may
return `purge.read_fenced`, mutation may return `purge.write_fenced`, and the
next otherwise-valid `sync` retries qgh-managed cleanup before any GitHub
request. A retry that remains incomplete returns `purge.retry_failed` with only
aggregate target/trigger kinds and coarse stage names; it does not include
source bodies, queries, tokens, or raw transport errors. `purge.successor_*`
codes mean qgh could not publish the required clean lexical successor snapshot.
`purge.successor_repair_required` blocks query fallback from opening an old
index after purge invalidated the publication pointer; the next valid `sync`
repairs that pointer before token resolution or a GitHub request.
Post-purge activation additionally requires the current durable
`purge_successor` snapshot and a real validated Tantivy artifact; reserved-only,
missing, stale-epoch, or corrupt generations remain unpublished and leave
successor repair pending.

`embed --force` returns `embedding.source_snapshot_missing` instead of creating
a synthetic provenance id when no completed remote or purge-successor source
snapshot exists. Run a successful `sync` first; the failed embed attempt does
not publish vectors or a retrieval generation.

`query`/`search` and `status` may return `freshness.stale` when the local
snapshot violates a fail-mode freshness policy or `--require-fresh` is passed.
The error details include the same local-only `freshness` block and triggered
warning objects.

`query`/`search` may return `validation.invalid_query` when the query text or
query arguments are invalid, such as `--limit 0`.
`query`/`search --issue` and `sync issue` may return
`validation.invalid_issue_number` when the requested issue number is less than
one.

`init` may additionally return:

- `config.no_git_worktree`: `qgh init` was run outside a git worktree.
- `config.git_remote_unavailable`: no usable `origin` remote was configured and `--repo` was omitted.
- `config.unsupported_git_remote`: `origin` was malformed or not a supported GitHub remote URL.
- `config.repo_policy_exists`: `.qgh.toml` already exists and `--force` was omitted.
- `validation.invalid_repo`: explicit repo or profile allowlist validation failed before writing `.qgh.toml`.
- `validation.missing_init_value`: `qgh init --yes`/`-y` was missing a required non-interactive value.
- `validation.init_cancelled`: interactive `qgh init` was canceled by EOF before writing files.
- `validation.invalid_token_source`: token source was not `github_cli` or `env`, or an env var name was invalid.

`init repo` success without an explicit profile includes a warning object with
code `config.profile_not_checked`; later `status/query` commands still perform
normal profile resolution and allowlist checks.

`get` may additionally return `validation.batch_size` when more than 20
`source_id` values are passed. In `get` batch output, source-local
`source.not_found`, `source.tombstoned`, and `source.outside_effective_scope`
failures are represented as item-level errors without failing the whole batch.

`sync` may additionally return `validation.window_requires_recent` when
`--window` is used without `--reconcile recent`, `validation.backfill_conflicts`
when `--backfill` is combined with live-sync modifiers,
`validation.requires_backfill` when backfill budget flags are used without
`--backfill`, and `validation.repo_required` when `sync issue` cannot resolve a
single target repo.

Human output and JSON output share exit-code classes. Human diagnostics go to stderr; JSON envelopes go to stdout.

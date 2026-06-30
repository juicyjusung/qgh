# Error Codes

qgh machine-readable output uses the `qgh.v1` envelope for success and failure. No-result query responses are successful: `ok: true` with `data.results: []`.

Stable error families:

- `config.*`: profile and TOML configuration failures.
- `validation.*`: CLI/schema/argument validation failures.
- `auth.*`: token source failures.
- `github.*`: GitHub request failures outside structured backoff state.
- `source.*`: missing or tombstoned source lookups.
- `storage.*`: SQLite or local filesystem storage failures.
- `index.*`: Tantivy index failures.
- `internal.*`: unexpected internal failures.

Common codes include `config.no_matching_profile`, `config.ambiguous_profile`, `config.invalid_repo_policy`, `validation.cli`, `validation.mcp`, `validation.unsupported_filter`, `validation.batch_size`, `auth.token_unavailable`, `source.not_found`, `source.tombstoned`, and `source.outside_effective_scope`.

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

Human output and JSON output share exit-code classes. Human diagnostics go to stderr; JSON envelopes go to stdout.

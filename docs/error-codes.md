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

Common codes include `config.no_matching_profile`, `config.ambiguous_profile`, `config.invalid_repo_policy`, `validation.cli`, `validation.mcp`, `validation.unsupported_filter`, `auth.token_unavailable`, `source.not_found`, and `source.tombstoned`.

`init` may additionally return:

- `config.no_git_worktree`: `qgh init` was run outside a git worktree.
- `config.git_remote_unavailable`: no usable `origin` remote was configured and `--repo` was omitted.
- `config.unsupported_git_remote`: `origin` was malformed or not a supported GitHub remote URL.
- `config.repo_policy_exists`: `.qgh.toml` already exists and `--force` was omitted.
- `validation.invalid_repo`: explicit repo or profile allowlist validation failed before writing `.qgh.toml`.

`init` success without an explicit profile includes a warning object with code
`config.profile_not_checked`; later `status/query` commands still perform normal
profile resolution and allowlist checks.

Human output and JSON output share exit-code classes. Human diagnostics go to stderr; JSON envelopes go to stdout.

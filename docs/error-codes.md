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

Common codes include `validation.cli`, `validation.unsupported_filter`, `auth.token_unavailable`, `source.not_found`, and `source.tombstoned`.

Human output and JSON output share exit-code classes. Human diagnostics go to stderr; JSON envelopes go to stdout.

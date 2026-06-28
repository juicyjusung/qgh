# Versioned JSON Error Envelope

qgh machine-readable CLI output and MCP structured content use one versioned envelope with `schema_version`, `ok`, `data`, `error`, `warnings`, and `meta`.

No-result query responses are successful empty result sets. Validation, config, auth, GitHub rate-limit, source-not-found, tombstoned source, storage/index, and internal failures use stable namespaced error codes such as `config.missing_profile` or `source.tombstoned`.

CLI `--json` prints the envelope to stdout for both success and failure; logs and human diagnostics go to stderr. MCP tool errors set `isError: true` and carry the same envelope in structured content.

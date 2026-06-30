# Versioned JSON Error Envelope

qgh machine-readable CLI output uses one versioned envelope with `schema_version`, `ok`, `data`, `error`, `warnings`, and `meta`. MCP structured content mirrors this same envelope as an adapter contract.

No-result query responses are successful empty result sets. Validation, config, auth, GitHub rate-limit, source-not-found, tombstoned source, storage/index, and internal failures use stable namespaced error codes such as `config.no_matching_profile` or `source.tombstoned`.

CLI `--json` prints the envelope to stdout for both success and failure; logs and human diagnostics go to stderr. Agents can complete `query -> get -> cite` through CLI JSON alone. MCP tool errors set `isError: true` and carry the same envelope in structured content.

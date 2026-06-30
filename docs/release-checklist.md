# Release Checklist

This release artifact is for the qgh MVP contract. It does not define new product behavior.

## Contract Surface

- CLI commands: `init`, `sync`, `query`, `search`, `get`, `status`, `doctor`, `mcp`.
- Product contract source of truth: CLI args, `qgh.v1` JSON schemas, and
  local SQLite/Tantivy retrieval behavior.
- Canonical CLI workflow: `init -> sync -> query -> get -> cite -> status`.
- Agents can perform the workflow without MCP via `qgh query --json`,
  `qgh get --json`, and `qgh status --json`.
- `search` is a CLI alias for `query`.
- CLI-only commands: `init`, `sync`, `doctor`.
- `qgh init` preview: detected repo/host/profile/token/config/repo-policy/DB
  defaults before write; Enter/`Y` accepts, `n` customizes, EOF cancels with
  `validation.init_cancelled` and no files changed.
- `qgh init --yes` and `qgh init -y` apply the inferred preset without preview
  or prompts.
- MCP role: optional read-only thin adapter over the CLI JSON/local retrieval contract.
- MCP tools: `query`, `get`, `status`.
- MCP read-only tools only: no `init`, `sync`, `doctor`, `eval`, mutation, hosted-provider, or write-back tools.
- Machine output schema version: `qgh.v1`.
- Human output: default successful CLI stdout is a command summary; `--json`
  keeps the stable machine envelope.
- Release artifact schema version: `qgh.release.v1`.
- Supported MVP token sources: `github_cli`, `env`; `credential_store` is
  post-MVP and fails with `validation.invalid_token_source`.

## MVP Gate Snapshot

Included MVP gates: AC-01 through AC-28 except AC-13 and AC-20.

Excluded or post-MVP gates:

- AC-13: vector/hybrid search is post-MVP; the MVP gate is the Tantivy BM25-only path.
- AC-20: GHES remains best-effort and is not a release gate.

## Verification Matrix

| Area | Release check |
| --- | --- |
| Tantivy BM25-only path | `sync`, `query`, `get`, and `status` pass without vector, model, GPU, or hosted provider dependencies. |
| strict schema/envelope | CLI JSON and MCP structured content use `qgh.v1`; unknown CLI/MCP adapter/config parameters fail with structured errors. |
| human CLI summaries | Non-json `init`, `sync`, `query`/`search`, `get`, `status`, and `doctor` output explains profile/repo/path/source/next-step state for people, while `--json` keeps the schema-compatible envelope. |
| init output | top-level `init` is CLI-only first-run profile/repo bootstrap with preset preview/custom fallback, `--yes`/`-y` bypass prompts, `init repo` is repo-policy-only, both emit `docs/schemas/init-output.schema.json`, and neither appears in MCP `tools/list`. |
| MCP adapter parity smoke | `tools/list` exposes only `query`, `get`, and `status`, each with `readOnlyHint: true`, and MCP structured content mirrors the CLI JSON envelope. |
| stdout cleanliness | MCP stdio writes only protocol JSON messages to stdout; CLI JSON envelopes go to stdout and human diagnostics go to stderr. |
| privacy no-egress | Default behavior sends data only to the configured GitHub host for sync, `get` lifecycle checks, and explicit `doctor`; no hosted provider path is enabled. |
| DB/index permissions | SQLite profile data, Tantivy generation directories, cache, and logs are single-user where the platform supports it. |
| doctor output | `doctor` is CLI-only and reports config, file permissions, SQLite/Tantivy consistency, GitHub reachability, and rate-limit headers in the same envelope. |
| search eval result | `docs/search-quality-eval.md` records the 24-query synthetic fixture result and `recalibration_requires_prd_adr_update=false`. |

## Residual Risks

- Wiki is post-MVP and must not be presented as MVP behavior.
- vector retrieval is post-MVP and must not be presented as required for MVP quality.
- shared server, org-wide discovery, and ACL handling are post-MVP product decisions.
- write-back and mutation behavior are outside the read-only MVP.
- user-facing eval is not an MVP CLI or MCP command; it remains a release/test harness.
- GHES compatibility is best-effort until a dedicated compatibility pass.

## Release Inputs

- PRD: `qgh-prd.md`
- Product brief: `qgh-product-brief.md`
- CLI/JSON contract: `docs/cli-json-contract.md`
- Privacy contract: `docs/privacy.md`
- Search eval result: `docs/search-quality-eval.md`
- Release artifact: `docs/release-artifact.json`

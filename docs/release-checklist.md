# Release Checklist

This release artifact is for the qgh MVP contract. It does not define new product behavior.

## Contract Surface

- CLI commands: `init`, `sync`, `embed`, `model`, `query`, `search`, `get`,
  `status`, `doctor`, `schedule`, `mcp`.
- Product contract source of truth: CLI args, `qgh.v2` JSON schemas, and
  local SQLite/Tantivy retrieval behavior.
- Canonical CLI workflow: `init -> sync -> query -> get -> cite -> status`.
- Agents can perform the workflow without MCP via `qgh query --json`,
  `qgh get --json`, and `qgh status --json`.
- `search` is a CLI alias for `query`.
- CLI-only commands: `init`, `sync`, `embed`, `model`, `doctor`, `schedule`.
- `qgh init` preview: detected repo/host/profile/token/config/repo-policy/DB
  defaults before write; Enter/`Y` accepts, `n` customizes, EOF cancels with
  `validation.init_cancelled` and no files changed.
- `qgh init --yes` and `qgh init -y` apply the inferred preset without preview
  or prompts; without explicit `--profile`/`QGH_PROFILE`, selection happens
  from the latest config snapshot under its mutation lock and ambiguous
  candidates fail closed.
- Init lowercases host identity, never combines a selected host with endpoints
  from a different origin host, preserves unchanged existing endpoint defaults,
  and reports the actual CLI/Git-remote repo provenance.
- Existing-profile init previews the stored token source, omits a misleading
  customization prompt, and rejects explicit credential-source conflicts.
- Init repo-policy publication revalidates at apply time, creates without
  replacement, atomically replaces only with `--force`, and rejects final
  symlink/non-regular entries.
- `qgh get <source_id>... --json` supports up to 20 source ids; single-source
  output stays backward compatible, batch output uses input-ordered item
  success/error entries, and lifecycle checks run only with
  `--verify-lifecycle`.
- Batch cap violations fail command-level with `validation.batch_size`.
- MCP role: optional read-only thin adapter over the CLI JSON/local retrieval contract.
- MCP tools: `query`, `get`, `status`.
- MCP read-only tools only: no `init`, `sync`, `schedule`, `embed`, `model`, `doctor`,
  `eval`, mutation, hosted-provider, or write-back tools.
- Machine output schema version: `qgh.v2`.
- Human output: default successful CLI stdout is a command summary; `--json`
  keeps the stable machine envelope.
- Release artifact schema version: `qgh.release.v1`.
- Primary install channel: `brew install juicyjusung/tap/qgh` from the
  existing public `juicyjusung/homebrew-tap` repository.
- Release artifacts originate from GitHub Releases and are produced by
  `cargo-dist`.
- Day-one release target matrix: macOS Apple Silicon and Linux x86_64. macOS
  Intel is excluded because the pinned ONNX Runtime release has no matching
  prebuilt runtime.
- Release trigger: explicit `Cargo.toml` version bump commit plus matching
  `vX.Y.Z` tag push.
- Release config: `dist-workspace.toml` pins `cargo-dist` 0.32.0, enables
  `homebrew`, builds release binaries with `fastembed-provider`, uses `sha256`
  checksums, and enables GitHub Artifact Attestations. The workflow runs
  `./homebrew-smoke` after announcement.
- Release workflow: `.github/workflows/release.yml` builds local artifacts,
  global artifacts, checksum-backed installers, GitHub Release uploads, and
  Homebrew formula publication.
- Homebrew smoke workflow: `.github/workflows/homebrew-smoke.yml` validates the
  generated formula's versioned GitHub Release URL and Homebrew `sha256`, then
  links the checked-out tap into Homebrew's tap directory, installs
  `juicyjusung/tap/qgh` without a GitHub token, and runs `qgh --version` plus
  `qgh help`.
- Tap publication uses repo secret `HOMEBREW_TAP_TOKEN`, scoped to contents
  write on only `juicyjusung/homebrew-tap`.
- Supported MVP token sources: `github_cli`, `env`; `credential_store` is
  post-MVP and fails with `validation.invalid_token_source`.

## MVP Gate Snapshot

Included MVP gates: AC-01 through AC-31 except AC-13 and AC-20.

Not required by the original MVP acceptance gate:

- AC-13: optional local vector/hybrid search is shipped and hardened, but it is
  not a prerequisite for the complete Tantivy BM25-only MVP path.
- AC-20: GHES remains best-effort and is not a release gate.

Issue #47 adds release gates for the optional Qwen/hybrid capability without
turning that capability into a BM25 dependency: explicit model acquisition,
generation/publication integrity, fail-closed purge, multilingual live
evaluation, and safe BM25 fallback.

## Verification Matrix

| Area | Release check |
| --- | --- |
| Tantivy BM25-only path | `sync`, `query`, `get`, and `status` pass without vector, model, GPU, or hosted provider dependencies. |
| optional Qwen/hybrid path | New fastembed-capable configs select `qwen3-embedding-0.6b`; weights remain a separate explicit download, `lexical_guard_v1` protects the BM25 top five, and an unavailable or invalid model never breaks BM25-only retrieval. |
| optional reranker | The local Qwen reranker is separately installed, per-query opt-in, off by default, bounded to ten candidates, and cannot add a source. |
| purge and publication safety | Pending purge blocks query/get immediately, removes owned source/version/chunk/vector/Tantivy generations, survives partial failure for retry, and preserves unrelated repositories. Concurrent query/sync uses one pinned publication snapshot. |
| strict schema/envelope | CLI JSON and MCP structured content use `qgh.v2`; released schema object shapes are closed except documented envelope `data` and error `details` extension points; unknown CLI/MCP adapter/config parameters fail with structured errors. `qgh.v1` consumers must migrate because closed `sync`/`status` payloads and the strict schedule lifecycle contract changed incompatibly. |
| human CLI summaries | Non-json `init`, `sync`, `embed`, `model`, `query`/`search`, `get`, `status`, `doctor`, and `schedule` output explains profile/repo/path/source/next-step state for people, while `--json` keeps the schema-compatible envelope. Machine actions include a JSON-preserving command, and TTY decoration never changes authoritative source bodies. |
| init output | top-level `init` is CLI-only first-run profile/repo bootstrap with preset preview/custom fallback, `--yes`/`-y` bypass prompts, `init repo` is repo-policy-only, both emit `docs/schemas/init-output.schema.json`, and neither appears in MCP `tools/list`. |
| get batch output | `get` preserves single-source JSON shape, accepts 2-20 `source_id` values for CLI batch retrieval, preserves input order, records item-level source errors, and documents opt-in lifecycle checks. |
| MCP adapter parity smoke | `tools/list` exposes only `query`, `get`, and `status`, each with `readOnlyHint: true`; MCP `get` rejects lifecycle verification parameters, and MCP structured content mirrors the CLI JSON envelope. |
| stdout cleanliness | MCP stdio writes only protocol JSON messages to stdout; CLI JSON envelopes go to stdout and human diagnostics go to stderr. |
| privacy no-egress | Retrieval and local model inference have no hosted-provider path. Sync and explicit lifecycle/doctor probes use only the validated GitHub API origin; `model install` is the separate explicit weights-download path and never sends repository content. |
| DB/index permissions | SQLite profile data, Tantivy generation directories, cache, and logs are single-user where the platform supports it. |
| doctor output | `doctor` is CLI-only and reports config, file permissions, SQLite/Tantivy consistency, GitHub reachability, and rate-limit headers in the same envelope. |
| single-flight sync and rate-budget observation | Concurrent profile writers return retryable `sync.busy`; normal exit and forced termination release the lease. Success, `304`, and backoff response headers update a content-free best-effort budget snapshot, and `status` reads it without network access. |
| bounded explicit-profile schedule coordinator | `schedule run` requires unique explicit profiles, plans locally, serializes each host, checks a shared gate before every send, permits one unknown-budget probe request and one profile per unknown-start pass, preserves the 20% reserve, retains a content-free per-host write-ahead guard across uncertain/backoff outcomes, rotates a durable cursor, isolates profile failures, and never hides bootstrap/backfill/reconciliation/model work. |
| LaunchAgent and systemd user timer lifecycle | `schedule start/status/stop` is idempotent and CLI-only; private macOS LaunchAgent and Linux systemd user artifacts use direct argv, catch-up/jitter, no overlap, rollback, `github_cli` credentials, no GitHub lifecycle egress, and no cron fallback. CI runs `cargo test --lib schedule_lifecycle` and `cargo test --test issue_body_tracer schedule_` on Ubuntu and macOS. The `schedule-manager-gate` workflow then runs `scripts/verify-schedule-manager.sh` against real dedicated user managers, followed by the physical resume observation in `docs/scheduling.md`. |
| search eval result | `docs/search-quality-eval.md` keeps the synthetic contract gate; `docs/search-quality-live-model-eval.md` records the public 80-query multilingual live run, resource diagnostics, and the Qwen/`lexical_guard_v1` decision. |
| one-command install | `brew install juicyjusung/tap/qgh` installs a self-contained `qgh` binary that can run `qgh --version`, `qgh help`, `qgh init`, and local diagnostic commands. |
| cargo-dist release automation | `cargo dist plan` and `cargo dist build` pass for macOS Apple Silicon and Linux x86_64 release targets. |
| Homebrew formula smoke | `.github/workflows/homebrew-smoke.yml` checks the generated formula for versioned GitHub Release artifact URLs and Homebrew `sha256` values, then runs the installed `qgh` binary with `qgh --version` and `qgh help`. |
| release integrity | Release artifacts include checksums and Homebrew `sha256`; separate `cosign`/`minisign` signing is not required for this release gate. |

## Residual Risks

- Wiki is post-MVP and must not be presented as MVP behavior.
- optional vector retrieval must not be presented as required for BM25 MVP
  correctness or as answer confidence.
- Qwen quality evidence uses a previously opened split and an incomplete large
  resource protocol; it supports the user-approved default and regression
  guard, not a fresh-blind general promotion claim.
- Qwen weights are not bundled. Users must explicitly run `qgh model install
  qwen3-embedding-0.6b`; peak RSS for the canonical 80-query adapter artifact
  remains unmeasured.
- shared server, org-wide discovery, and ACL handling are post-MVP product decisions.
- write-back and mutation behavior are outside the read-only MVP.
- user-facing eval is not an MVP CLI or MCP command; it remains a release/test harness.
- GHES compatibility is best-effort until a dedicated compatibility pass.
- Linux ARM64, Windows packages, and `homebrew/core` submission are later
  distribution targets.
- Public unauthenticated Homebrew installs require the `juicyjusung/qgh`
  repository and release assets to remain public.
- Live dogfood against `juicyjusung/qgh` is a manual first-use checklist item,
  not a blocking CI gate.
- Scheduled Linux execution requires an active systemd user manager. qgh does
  not change lingering policy or install a cron fallback.

## Release Inputs

- PRD: `qgh-prd.md`
- Product brief: `qgh-product-brief.md`
- CLI/JSON contract: `docs/cli-json-contract.md`
- Privacy contract: `docs/privacy.md`
- Scheduled sync: `docs/scheduling.md`
- Search eval result: `docs/search-quality-eval.md`
- Live multilingual eval: `docs/search-quality-live-model-eval.md`
- Qwen adapter evidence: `docs/search-quality-qwen-production-adapter-eval.md`
- Release artifact: `docs/release-artifact.json`
- Install docs: `README.md`

# CLI JSON Contract

## Envelope

Machine-readable CLI output uses one versioned `qgh.v1` envelope on stdout
when `--json` is passed. Without `--json`, successful CLI commands print
human-readable summaries on stdout. Diagnostics and human-readable failures go
to stderr.

The product contract is CLI-first. CLI args, the `qgh.v1` JSON envelope,
released schema snapshots, and local SQLite/Tantivy retrieval behavior are the
source of truth for new features. Agents can use `qgh query --json`, `qgh get
--json`, and `qgh status --json` without MCP. MCP is a read-only thin adapter
over the same local retrieval contract.

`sync` without `--json` emits human-readable progress diagnostics to stderr so
long GitHub fetch/index runs do not look stalled, then prints a final human
summary to stdout. `sync --json` and `sync --quiet` suppress progress
diagnostics. Progress lines and human summaries are not a stable
machine-readable API; use `--json` for automation.
`sync --if-stale --json` returns `data.sync_state: "skipped_fresh"` when the
local snapshot is still within the effective max-age and no network sync runs.
A completed source sync publishes its newly built lexical generation even when
configured local embedding refresh fails; the response keeps the embedding
warning and reports the actual active lexical generation. A partial sync or
backfill interrupted by backoff does not fabricate a source-snapshot id or
publish its partial corpus, and reports
`publication.incomplete_snapshot_deferred` instead.

`sync issue <number>` is the explicit targeted refresh path for one issue and
its complete per-issue comment list. Its `sync` envelope includes `target`,
`lifecycle`, and comment diff counts (`added`, `updated`, `deleted`) in addition
to the normal sync/index summary fields. Transfer, delete, and permission-loss
lifecycle changes are reported as distinct reason codes.

Success:

- `schema_version`: `qgh.v1`
- `ok`: `true`
- `data`: command-specific payload
- `warnings`: array of `{code, severity, message}` objects. Freshness warnings
  use the severity ladder `fail > warn_strong > warn`; all triggered warnings
  are listed even when the envelope decision uses the maximum severity.
- `meta`: object

Failure:

- `schema_version`: `qgh.v1`
- `ok`: `false`
- `error`: structured error object
- `warnings`: array
- `meta`: object

Released schema snapshots:

- `docs/schemas/envelope.schema.json`: common success/error envelope.
- `docs/schemas/error.schema.json`: stable error taxonomy and exit-code classes.
- `docs/schemas/init-output.schema.json`: CLI-only `init` data payload.
- `docs/schemas/sync-output.schema.json`: `sync` data payload.
- `docs/schemas/model-output.schema.json`: CLI-only `model install` data payload.
- `docs/schemas/query-result.schema.json`: `query`/`search` data payload.
- `docs/schemas/get-output.schema.json`: `get` data payload.
- `docs/schemas/status-output.schema.json`: `status` data payload.
- `docs/schemas/doctor-output.schema.json`: CLI-only `doctor` data payload.

Released command payload schemas are closed by default: object schemas either
set `additionalProperties: false` or use a bounded map value schema. The
reusable envelope keeps `data` as the command-specific payload slot, and
structured errors keep `details` as the error-code-specific diagnostic
extension point.

MCP uses the same envelope in structured tool content to mirror CLI behavior.
Tool-level validation failures set `isError: true`; JSON-RPC protocol errors
are reserved for malformed protocol messages or server faults.

No-result query responses are successful envelopes with `data.results: []`.

`query`/`search` and `status` include `data.freshness`:

- `decision`: `fresh`, `stale_warn`, `stale_fail`, or `never_synced`.
- `remote_checked`: always `false`; freshness is computed from local sync
  metadata only.
- `snapshot_age_seconds`: seconds since the last successful sync, or `null`
  when no sync has completed.
- `max_age_seconds`: the effective max-age after applying precedence.

Freshness precedence is `--max-age` flag > repo policy `[query].max_age` >
profile `query_max_age` > built-in `7d`. `--require-fresh` is a per-run
stale-to-fail override. Profile config accepts duration strings such as `90s`,
`30m`, `7d`, and `12mo` for `query_max_age`, `active_issue_max_age`, and
`reconcile_after`; legacy `reconcile_after_days` remains a deprecated alias.
Open issue results apply `min(query_max_age, active_issue_max_age)` when the
active issue max age is configured.

CLI command envelopes and MCP structured tool content include Effective Scope
metadata when resolution has run:

- `meta.profile_id`: resolved profile id.
- `meta.profile_source`: `cli`, `env`, `single_match`, or `get_args`.
- `meta.repo`: effective `owner/repo` scope, or `null` when the command has no repo scope.
- `meta.repo_source`: `cli`, `repo_policy`, `git_remote`, `command` for MCP tool arguments, or `null`.
- `meta.repo_policy_path`: current worktree repo policy path when a repo policy supplied scope, otherwise `null`.

`status` also includes `data.resolution` with the same resolved profile and
repo-scope fields. Its read-only `data.purge` block reports only aggregate,
content-free purge state: pending count, whether lexical-successor repair is
required, whether retrieval is fenced, target/trigger kinds, and coarse
current/failure stages.
CLI-only `doctor`
includes the same diagnostics and is the explicit command that may run probes.
Its purge block also states that user-created filesystem backups and snapshots
outside qgh-managed generation paths are not deleted by qgh. Neither `status`
nor `doctor` retries or starts a purge. MCP exposes `status`, but not `doctor`.
CLI-only top-level `init` bootstraps profile config plus repo scope. `init repo`
creates tracked repo policy only. Neither command is exposed to MCP.

## Human Output

Human output is generated from the same command data as the JSON envelope, but
it is optimized for a person reading the terminal:

- `init`: profile id/action, repo allowlist action, token source reference,
  config path, repo policy action/path, and next commands.
- `sync`: synced repo scope, fetched/upserted issue and comment counts, targeted
  comment diff counts when present, backoff state, active index generation, and
  next query command.
- `query`/`search`: source-candidate list, not answers. It states that snippets
  are previews, not citation evidence, reports local snapshot freshness, and
  shows `qgh get <source_id> --profile-id <profile_id>` for each result.
- `get`: full source body, canonical URL, source version/staleness metadata,
  and lifecycle check status. Default `get` is local-only and reports
  `lifecycle_check.status=not_checked` with `reason=not_requested`; pass
  `--verify-lifecycle` to opt in to a GitHub lifecycle check. Batch get
  summaries include requested/returned/failed counts and per-item success or
  error state.
- `status`: selected profile, local snapshot freshness, effective repo scope
  and repo source, DB path, Tantivy index path, source counts, default sync
  scope, content-free pending-purge state, optional local embedding coverage,
  and `qgh sync --all` guidance.
- `doctor`: failed checks first with actionable hints, then all checks, pending
  purge state, the unmanaged-backup deletion boundary, and MCP exposure status.

Human output is deliberately not schema-stable. `--json` remains the contract
source for agents, scripts, MCP parity checks, and release schema validation.

## Init Output

Top-level `init` is the first-run wizard. It reads the current git worktree
`origin` remote, builds a preset from GitHub.com or GHES host defaults,
default profile id `work`, token source `github_cli`, XDG config/profile DB
paths, and the default-on `.qgh.toml` repo policy path. Interactive `qgh init`
prints that preview before writing. Enter/`Y` applies the preset; `n` enters
the customize prompts; EOF cancels with `validation.init_cancelled` and no files
changed. It stores token source references only, never literal token values.

`init repo` creates or overwrites only the current git worktree root `.qgh.toml`
repo policy. It never creates profile config, token source config, profile store
paths, arbitrary DB paths, or user-local absolute paths.

Top-level `init --json` returns:

- `profile_config_path`: created or updated profile config path.
- `profile_id`: resolved or selected profile id.
- `profile_action`: `created` or `updated`.
- `repo`: effective `owner/repo` scope.
- `repo_allowlist_action`: `added` or `already_present`.
- `repo_policy_action`: `created`, `overwritten`, `already_exists`, or `skipped`.
- `repo_policy_path`: `.qgh.toml` path when written or already present, otherwise `null`.
- `token_source.kind`: `github_cli` or `env`.
- `next_steps`: short command suggestions.

`init --yes` and `init -y` are the non-interactive automation paths. They apply
the inferred preset without preview or prompts. Missing required values fail
with structured validation errors instead of falling back to prompts.

`init repo --json` returns:

- `path`: created or overwritten `.qgh.toml` path.
- `repo`: generated `owner/repo` policy scope.
- `repo_source`: `cli` when `--repo owner/repo` was used, or `git_remote` when
  inferred from a supported GitHub `origin` remote.
- `overwritten`: whether an existing policy was replaced with `--force`.
- `profile_validation`: `validated` with `profile_id` and `profile_source`
  when `--profile` or `QGH_PROFILE` was provided, otherwise `not_checked`.

When no profile is explicit, `init repo` may still create repo policy, but the
success envelope includes a `config.profile_not_checked` warning. Commands that
use the policy later still apply normal profile resolution and allowlist checks.

## Model Install Output

`qgh model install <preset> --json` is CLI-only and resolves before profiles.
Its closed payload records `model`, `purpose`, immutable upstream `model_id`
and `resolved_revision`, `action`, verified artifact count and bytes,
`manifest_hash`, and `weights_bundled: false`. It never includes a cache path,
token, repository content, query text, or downloaded file contents. Model
management is not exposed through MCP.

## Query Results

`query` and `search` return source candidates, not answers. Each result identifies a GitHub Issue or issue comment that can be fetched through `get`.
`--limit` must be greater than zero; invalid values fail with a structured
validation error instead of silently returning an empty result set.
`--issue` must also be greater than zero.
`ranking.lexical_score` is populated for BM25 results and for hybrid results
with lexical evidence, otherwise `null`. `ranking.vector_distance` is populated
for internal vector-ranked results and for hybrid results with vector evidence,
otherwise `null`; lower distance is closer. Hybrid results also include
`ranking.rrf_rank_score` and `ranking.final_order_score`; in hybrid v1,
`final_order_score` is the retrieval-stage ordering signal after RRF fusion and
before optional bounded reranking.

When `--rerank` or MCP `"rerank": true` is requested, `data.rerank` reports
whether the fixed top-10 local stage was applied. Applied BM25, vector, and
hybrid head results add `ranking.rerank_score` and
`ranking.pre_rerank_rank` together. `rerank_score` is the raw local
cross-encoder ordering logit used within that bounded head; it is not a
probability or calibrated confidence. Exact locator results bypass reranking
and never contain those fields. Results below the fixed head keep their
retrieval order and do not contain rerank metadata.
Ranking fields are not confidence or probability. Vector-only ranking is not a
user-facing CLI or MCP mode.

Every result includes:

- `source_id`: stable qgh URI for the source.
- `entity_type`: `issue` or `issue_comment`.
- `canonical_url`: GitHub URL for the source.
- `snippet`: short local preview text. The snippet is a preview, not citation evidence.
- `get_args`: arguments that must round-trip through `get`, including the
  profile store that produced the result.
- `parent_issue`: issue context for comments, or `null` for issue bodies.
- `source_version`: body hash, GitHub updated timestamp, indexed timestamp, sync run, and lifecycle state.
- `ranking`: typed ordering evidence. `lexical_score`, `vector_distance`,
  `rrf_rank_score`, `final_order_score`, and optional `rerank_score` are ranking
  signals, not confidence or probability.

Query results intentionally omit `body`. Use the `get` response when source text, canonical URL, and source identity are needed for a citation.

## Citation Flow

1. Run `qgh query --json` to find source candidates.
2. Run `qgh get --json` with the result's `get_args.source_id` and
   `get_args.profile_id`. For CLI automation, pass `get_args.profile_id` as
   `get --profile-id <profile_id>`; for MCP, pass it as the `profile_id`
   argument.
3. Use the `get` response `source.source_id`, `source.canonical_url`, and source text for the final citation.

Citation example from a `get` response:

- Source identity: `qgh://github.com/issue/I_kwDOISSUE1`
- Canonical URL: `https://github.com/owner/repo/issues/42`

If a local index hit cannot be resolved through `get`, qgh filters it out of successful results and reports it in `data.result_filtering.unresolvable_hits`.

## Get Output

Single-source `qgh get <source_id> --json` remains backward compatible and
returns:

- `profile_id`: profile store used to resolve the source.
- `source`: full authoritative source object with `source_id`, `entity_type`,
  `canonical_url`, `body`, `source_version`, and `lifecycle_check`.

Batch `qgh get <source_id> <source_id> ... --json` returns:

- `profile_id`: profile store used for every item.
- `summary.requested`: number of input source ids.
- `summary.returned`: number of successful source loads.
- `summary.failed`: number of item-level failures.
- `summary.batch_size_cap`: maximum accepted batch size, currently `20`.
- `lifecycle_check_policy.verify_lifecycle`: whether the command opted in to
  GitHub lifecycle verification.
- `lifecycle_check_policy.mode`: `not_requested` by default, or `sequential`
  when `--verify-lifecycle` is passed. Verified batch lifecycle REST probes run
  in input order with at most one in-flight request.
- `lifecycle_check_policy.profile_max_in_flight_requests`: the selected
  profile's configured sync/request cap for visibility.
- `lifecycle_check_policy.hard_cap`: global hard cap, currently `16`.
- `items`: one item per input source id, in input order. Successful items carry
  `ok: true` and `source`; source-local failures carry `ok: false` and a
  structured `error`.

Source-local `source.not_found`, `source.tombstoned`, and
`source.outside_effective_scope` failures are item-level batch errors and do not
stop the remaining items. Malformed CLI arguments, profile conflicts, and
`summary.batch_size_cap` violations are command-level structured errors.

MCP `get` is local-only and read-only. It rejects `verify_lifecycle` as an
unknown parameter; use CLI `qgh get --verify-lifecycle` when a lifecycle check
may probe GitHub and tombstone local sources.

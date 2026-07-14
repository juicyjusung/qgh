# Error Codes

qgh machine-readable output uses the `qgh.v2` envelope for success and failure. No-result query responses are successful: `ok: true` with `data.results: []`.

Stable error families:

- `config.*`: profile and TOML configuration failures.
- `validation.*`: CLI/schema/argument validation failures.
- `freshness.*`: local snapshot freshness failures.
- `auth.*`: token source failures.
- `github.*`: GitHub request failures outside structured backoff state.
- `sync.*`: sync lease/backoff, page-commit, and confirmed issue-transfer-chain failures.
- `schedule.*`: foreground coordinator and user scheduler lifecycle failures.
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

`sync.backoff` is the retryable, exit-`5` result for a confirmed GitHub primary
or secondary rate-limit interruption. qgh persists only content-free backoff
metadata for `status`. Its details name the profile and report whether the
active lexical publication is actually query-ready; a historical successful
sync row alone does not claim retrieval availability. `status` remains the
local diagnostic path, while `get` availability can remain source-dependent.
Retry the explicit `sync` at or after the absolute retry/reset time. qgh does
not turn this condition into a successful sync or silently retry network work
from a read command. `retry_action.command` is the human retry and
`retry_action.json_command` is the machine-safe equivalent; legacy
`retry_command` remains for compatibility.

`sync.busy` is the retryable, exit-`5` result when another process owns the
same profile writer lease. Details contain only `profile_id`; wait for the
active process or its OS-released crash lease, then retry. qgh never deletes a
stable lock file to recover this condition.

`schedule` validation and lifecycle may additionally return:

- `validation.duplicate_profile`: an explicit schedule list repeated a profile.
- `validation.schedule_profile_boundary`: global `--profile` was combined with schedule's explicit profile list.
- `validation.schedule_interval`: v1 received an interval other than `1h`.
- `validation.schedule_profiles`: no explicit start profiles were supplied through a programmatic call.
- `schedule.credentials_unsupported`: `schedule start` selected an `env` token source instead of `github_cli`.
- `schedule.busy`: another `schedule start` or `schedule stop` owns the stable lifecycle lease; retry after it finishes.
- `schedule.credentials_unavailable`: `HOME`, `gh`, or scheduled GitHub CLI credentials are unavailable.
- `schedule.binary_unavailable`: the invoked qgh executable cannot be pinned as an existing executable absolute file.
- `schedule.environment_invalid`: a required manager environment value/path cannot be encoded safely.
- `schedule.platform_unsupported`: the OS has no macOS LaunchAgent or Linux systemd user adapter; no cron fallback is installed.
- `schedule.manager_unsupported`: the supported OS lacks an active user manager session; manual `schedule run` remains available.
- `schedule.manager_failed`: launchctl/systemctl failed while applying a user schedule; qgh restores the previous artifact/state best-effort.
- `schedule.platform_mismatch`: registration state belongs to another platform adapter.
- `schedule.storage_failed`: private scheduler artifact/state publication or removal failed.
- `schedule.state_invalid`: strict local registration state is malformed or unsupported.
- `schedule.ownership_ambiguous`: the fixed user-manager identity is active, orphaned, or has legacy state whose exact artifact owner cannot be proven; qgh makes no lifecycle mutation.

These errors omit tokens, source content, repository names, local DB paths, and
raw manager stderr. `schedule.partial_failure` is a warning in a successful
foreground pass, not an error envelope code.

Local model acquisition and prepared-snapshot publication fail closed with
stable, content-free errors:

- `embedding.acquisition_artifact_mismatch`: materialized model artifacts do
  not match the pinned acquisition or declared manifest.
- `embedding.acquisition_pin_busy`: another pin mutation is active, or its
  bounded lock could not yet be safely reclaimed.
- `embedding.acquisition_pin_invalid`: the persisted acquisition request does
  not satisfy its contract or local-store confinement rules.
- `embedding.acquisition_pin_lock_failed`: qgh could not create the local lock
  required to serialize pin mutation.
- `embedding.acquisition_pin_mismatch`: the acquisition pin changed or went
  missing before publication or retirement completed.
- `embedding.acquisition_pin_retire_failed`: a completed acquisition pin could
  not be removed durably.
- `embedding.acquisition_pin_unlock_failed`: the acquisition mutation lock
  could not be released durably.
- `embedding.acquisition_staging_cleanup_failed`: a failed acquisition's
  staging state could not be safely removed.
- `embedding.atomic_replace_cleanup_failed`: cleanup after a failed atomic
  local-state replacement did not complete.
- `embedding.hf_cache_invalid`: a downloaded Hugging Face artifact could not be
  resolved as a confined local-cache file.
- `embedding.hf_revision_mismatch`: resolved artifacts do not match the pinned
  Hugging Face revision.
- `embedding.prepared_alias_publish_failed`: the verified prepared-snapshot
  alias could not be published durably.
- `embedding.tokenizer_artifact_too_large`: one tokenizer artifact or the
  cumulative tokenizer snapshot exceeds qgh's bounded local resource limit.

These descriptions intentionally omit local paths, tokens, model bytes,
queries, and source content. Resolve the local acquisition state and retry
preparation; qgh does not accept a mismatched artifact as validated. Separately,
`embedding.vector_integrity_failed` is a content-free warning, not an error
envelope code: hybrid vector use is skipped and BM25 results are returned.

Explicit local Qwen installation may return `model.not_installed`,
`model.snapshot_invalid`, `model.artifact_missing`, or
`model.artifact_invalid` when the pinned snapshot is absent or fails strict
tree, size, or SHA-256 validation. `model.download_failed` and
`model.install_failed` distinguish network acquisition from atomic local
publication, while `model.provider_unavailable` means the binary was built
without local model support. `model.unknown` is reserved for unsupported
programmatic preset requests; CLI spelling errors remain `validation.cli`.

Qwen embedding initialization may return `embedding.model_not_installed` or a
content-free `embedding.qwen_*` code for snapshot, device, tokenizer, runtime,
or inference failure. These failures never authorize a Qwen model download
during `sync`, `embed`, `query`, `get`, `status`, `doctor`, or MCP query
handling.
Absent or invalid Qwen embedding and reranker snapshots expose content-free
install actions. The install command reuses a valid pinned snapshot or
quarantines and replaces an invalid one; repository content is never sent to
the model provider.
`embedding.pooling_unsupported` rejects a pooling contract the selected runtime
cannot execute.

Typed GitHub lifecycle adapters may return `github.invalid_issue_json` or
`github.invalid_comment_json` when a successful response cannot be decoded.
`sync.commit_page_failed` and `validation.lifecycle_failed` are content-free
fallbacks for local fetch-checkpoint and lifecycle-candidate validation
failures.
Targeted issue refresh may return `sync.transfer_cycle` or
`sync.transfer_chain_too_long`; confirmed transitions observed before either
terminal failure are queued for purge before the error is surfaced. The
`github.confirmed_lifecycle_requires_typed_handling` guard is reserved for
internal legacy adapters and is not emitted by current CLI command paths.

When a confirmed lifecycle or explicit allowlist-removal purge is incomplete,
the affected source, issue, or repository remains fail closed. Retrieval may
return `purge.read_fenced`, mutation may return `purge.write_fenced`, and the
next otherwise-valid `sync` retries qgh-managed cleanup before any GitHub
request. A retry that remains incomplete returns `purge.retry_failed` with only
aggregate target/trigger kinds and coarse stage names; it does not include
source bodies, queries, tokens, or raw transport errors. `purge.successor_*`
codes mean qgh could not publish the required clean lexical successor snapshot.
`purge.allowlist_reconciliation_required` means stored repository state no
longer matches the configured allowlist and must be reconciled by `sync` before
reads resume.
`purge.successor_repair_required` blocks query fallback from opening an old
index after purge invalidated the publication pointer; the next valid `sync`
repairs that pointer before token resolution or a GitHub request.
Post-purge activation additionally requires the current durable
`purge_successor` snapshot and a real validated Tantivy artifact; reserved-only,
missing, stale-epoch, or corrupt generations remain unpublished and leave
successor repair pending.

Retrieval publication is fail closed when its durable provenance cannot be
validated:

- `publication.source_snapshot_incomplete`: active source state has no complete
  snapshot identity at the current source epoch.
- `publication.source_snapshot_changed`: the source epoch or snapshot identity
  changed before activation or retrieval.
- `publication.source_inventory_mismatch`: the stored lexical generation count
  or inventory digest does not match the captured source snapshot.
- `publication.embedding_snapshot_mismatch`: lexical and embedding generations
  do not share the same fully validated source snapshot and identity fields.

These failures do not activate or query an unvalidated generation. Run a
successful `sync` to publish a coherent successor; when purge successor repair
is pending, the next otherwise-valid `sync` performs that repair first.
`publication.tantivy_artifact_not_ready` is an index/storage-state failure and
therefore exits with class `6`, not CLI-validation class `2`.

`query`/`search` and `status` may return `freshness.stale` when the local
snapshot violates a fail-mode freshness policy or `--require-fresh` is passed.
The error details include the same local-only `freshness` block and triggered
warning objects.

`query`/`search` may return `validation.invalid_query` when the query text or
query arguments are invalid, such as `--limit 0`.
`query`/`search --issue` and `sync issue` may return
`validation.invalid_issue_number` when the requested issue number is less than
one.
Label-filtered retrieval may return `validation.stale_index_label_filter` when
the local lexical index predates label-filter support; run `qgh sync` to rebuild
the index before retrying that filter.

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
`--backfill`, `validation.max_age_requires_if_stale` when `--max-age` is used
without `--if-stale`, and `validation.repo_required` when `sync issue` cannot
resolve a single target repo. `sync issue` also rejects parent sync scope,
lifecycle, freshness, and backfill options with `validation.cli` instead of
silently ignoring them.

Human output and JSON output share exit-code classes. Human diagnostics go to stderr; JSON envelopes go to stdout.

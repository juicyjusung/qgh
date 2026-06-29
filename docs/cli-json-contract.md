# CLI JSON Contract

## Envelope

Machine-readable CLI output uses one versioned `qgh.v1` envelope on stdout.
Diagnostics and human-readable failures go to stderr.

Success:

- `schema_version`: `qgh.v1`
- `ok`: `true`
- `data`: command-specific payload
- `warnings`: array
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
- `docs/schemas/query-result.schema.json`: `query`/`search` data payload.
- `docs/schemas/sync-output.schema.json`: `sync` data payload.
- `docs/schemas/get-output.schema.json`: `get` data payload.
- `docs/schemas/status-output.schema.json`: `status` data payload.
- `docs/schemas/doctor-output.schema.json`: CLI-only `doctor` data payload.

MCP uses the same envelope in structured tool content. Tool-level validation
failures set `isError: true`; JSON-RPC protocol errors are reserved for malformed
protocol messages or server faults.

No-result query responses are successful envelopes with `data.results: []`.

CLI command envelopes and MCP structured tool content include Effective Scope
metadata when resolution has run:

- `meta.profile_id`: resolved profile id.
- `meta.profile_source`: `cli`, `env`, `single_match`, or `get_args`.
- `meta.repo`: effective `owner/repo` scope, or `null` when the command has no repo scope.
- `meta.repo_source`: `cli`, `repo_policy`, `command` for MCP tool arguments, or `null`.
- `meta.repo_policy_path`: current worktree repo policy path when a repo policy supplied scope, otherwise `null`.

`status` also includes `data.resolution` with the same resolved profile and
repo-scope fields. CLI-only `doctor` includes the same diagnostics and is the
explicit command that may run probes. MCP exposes `status`, but not `doctor`.

## Query Results

`query` and `search` return source candidates, not answers. Each result identifies a GitHub Issue or issue comment that can be fetched through `get`.

Every result includes:

- `source_id`: stable qgh URI for the source.
- `entity_type`: `issue` or `issue_comment`.
- `canonical_url`: GitHub URL for the source.
- `snippet`: short local preview text. The snippet is a preview, not citation evidence.
- `get_args`: arguments that must round-trip through `get`, including the
  profile store that produced the result.
- `parent_issue`: issue context for comments, or `null` for issue bodies.
- `source_version`: body hash, GitHub updated timestamp, indexed timestamp, sync run, and lifecycle state.
- `ranking`: typed ordering evidence. `lexical_score` is a BM25 ordering signal, not confidence or probability.

Query results intentionally omit `body`. Use the `get` response when source text, canonical URL, and source identity are needed for a citation.

## Citation Flow

1. Run `query` to find source candidates.
2. Run `get` with the result's `get_args.source_id` and
   `get_args.profile_id`. For CLI automation, pass `get_args.profile_id` as
   `get --profile-id <profile_id>`; for MCP, pass it as the `profile_id`
   argument.
3. Use the `get` response `source.source_id`, `source.canonical_url`, and source text for the final citation.

Citation example from a `get` response:

- Source identity: `qgh://github.com/issue/I_kwDOISSUE1`
- Canonical URL: `https://github.com/owner/repo/issues/42`

If a local index hit cannot be resolved through `get`, qgh filters it out of successful results and reports it in `data.result_filtering.unresolvable_hits`.

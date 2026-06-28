# CLI JSON Contract

## Query Results

`query` and `search` return source candidates, not answers. Each result identifies a GitHub Issue or issue comment that can be fetched through `get`.

Every result includes:

- `source_id`: stable qgh URI for the source.
- `entity_type`: `issue` or `issue_comment`.
- `canonical_url`: GitHub URL for the source.
- `snippet`: short local preview text. The snippet is a preview, not citation evidence.
- `get_args`: arguments that must round-trip through `get`.
- `parent_issue`: issue context for comments, or `null` for issue bodies.
- `source_version`: body hash, GitHub updated timestamp, indexed timestamp, sync run, and lifecycle state.
- `ranking`: typed ordering evidence. `lexical_score` is a BM25 ordering signal, not confidence or probability.

Query results intentionally omit `body`. Use the `get` response when source text, canonical URL, and source identity are needed for a citation.

## Citation Flow

1. Run `query` to find source candidates.
2. Run `get` with the result's `get_args.source_id`.
3. Use the `get` response `source.source_id`, `source.canonical_url`, and source text for the final citation.

Citation example from a `get` response:

- Source identity: `qgh://github.com/issue/I_kwDOISSUE1`
- Canonical URL: `https://github.com/owner/repo/issues/42`

If a local index hit cannot be resolved through `get`, qgh filters it out of successful results and reports it in `data.result_filtering.unresolvable_hits`.

# Search Quality Eval

The MVP search-quality eval is a release/test harness, not a user-facing CLI or MCP command.

## Scope

- Fixture: synthetic `owner/repo` GitHub Issues and issue comments served by the integration test fake GitHub server.
- Harness: `tests/search_quality_eval.rs`.
- Workflow under test: `sync -> query -> get`.
- Wiki is post-MVP and excluded from the MVP eval fixture.
- Vector, hosted embedding, rerank, and GPU/model availability are outside this gate.

## Labels

Each query has:

- Gold source_id set: one or more qgh source ids expected to answer the query.
- Labeler: `qgh synthetic fixture maintainer`.
- Labeling rule: Gold source_id is the single issue or issue comment whose fixture body answers the query.
- Ambiguous query exclusion rule: exclude ambiguous queries when more than one active source is a plausible gold answer.

Negative queries use an empty Gold source_id set and pass only when the result set is empty.

## Metrics

| Query class | Metric | Gate |
| --- | --- | --- |
| exact lookup | top-1 hit rate | >= 0.95 |
| keyword/body/comment | top-5 hit rate | >= 0.80 |
| CJK/mixed | top-5 hit rate | >= 0.70 |
| negative | abstention rate | >= 0.80 |
| all top-k results | `get` round-trip success | 1.00 |

The CJK/mixed class exercises the Tantivy tokenizer baseline plus the CJK n-gram fallback field. It does not use hosted providers.

## Result Record

The harness records class rates, top failures, `get` round-trip failures, and:

```json
{"recalibration_requires_prd_adr_update": false}
```

If any metric misses its gate, the test failure includes `recalibration_requires_prd_adr_update=true` and the top failures. Recalibration requires a PRD or ADR update before changing thresholds.

Current synthetic fixture result:

```json
{
  "query_count": 24,
  "exact_top1": 1.0,
  "keyword_top5": 1.0,
  "cjk_top5": 1.0,
  "negative_abstention": 1.0,
  "get_round_trip": 1.0,
  "top_failures": [],
  "recalibration_requires_prd_adr_update": false
}
```

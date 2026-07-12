# Search Quality Eval

The MVP search-quality eval is a release/test harness, not a user-facing CLI or MCP command.

## Scope

- Fixture: synthetic `owner/repo` GitHub Issues and issue comments served by the integration test fake GitHub server.
- Harness: `tests/search_quality_eval.rs`.
- Workflow under test: `sync -> query -> get`.
- Wiki is post-MVP and excluded from the MVP eval fixture.
- Hosted embedding, rerank, GPU/model availability, and live model downloads are outside this gate.
- Hybrid coverage and H4b model A/B use deterministic local eval vectors so the gate does not download a model or create network egress.
- The source vectors and query vectors are authored as separate topic-axis fixtures; query vectors are not generated from Gold source_id labels.

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
| semantic/paraphrase | top-5 hit rate | report hybrid target >= 0.70 |
| cross-language | top-5 hit rate | report hybrid target >= 0.60 |
| all top-k results | hard filter violations | 0 |
| all top-k results | `get` round-trip success | 1.00 |

The CJK/mixed class exercises the Tantivy tokenizer baseline plus the CJK n-gram fallback field. It does not use hosted providers.

The semantic/paraphrase and cross-language classes run as a BM25-only vs hybrid A/B report over the same fixture. The initial semantic thresholds are directional: a miss records `section_8_3_triggers` for rerank/fusion review instead of weakening the hard release gate.

Hybrid eval rows also gate `ranking.kind=hybrid` coverage. Exact locator queries and negative abstention queries are excluded from this path gate because they intentionally bypass ranked hybrid retrieval or return no results.

## Result Record

The harness records class rates, top failures, `get` round-trip failures, and:

```json
{"recalibration_requires_prd_adr_update": false}
```

If any metric misses its gate, the test failure includes `recalibration_requires_prd_adr_update=true` and the top failures. Recalibration requires a PRD or ADR update before changing thresholds.

Current synthetic fixture result:

```json
{
  "bm25_regression_query_count": 24,
  "semantic_query_count": 20,
  "exact_top1": 1.0,
  "keyword_top5": 1.0,
  "cjk_top5": 1.0,
  "negative_abstention": 1.0,
  "hybrid_regression_path_queries": "15/15",
  "semantic_bm25_top5": 0.92,
  "semantic_hybrid_top5": 0.92,
  "semantic_hybrid_delta": 0.0,
  "semantic_hybrid_target": 0.7,
  "semantic_hybrid_path_queries": "20/20",
  "cross_language_bm25_top5": 0.5,
  "cross_language_hybrid_top5": 0.88,
  "cross_language_hybrid_delta": 0.38,
  "cross_language_hybrid_target": 0.6,
  "hard_filter_violations": 0,
  "get_round_trip": 1.0,
  "section_8_3_triggers": [],
  "top_failures": [],
  "recalibration_requires_prd_adr_update": false
}
```

## H4b Model A/B Report

`model_ab_report` is report-only. All rows use the same
`search-quality-eval` fixture and same H4a protocol. Each candidate uses
candidate-specific deterministic source and query vectors so the harness
compares model behavior without live model downloads. The report's
default/baseline row remains the historical Snowflake synthetic control in this
fixture. That label is not the current product default: ADR-0016 selects the separately
installed `qwen3-embedding-0.6b` preset for newly created fastembed-capable
configs. Product-default changes still require a PRD/ADR-backed human decision.

This table is a deterministic contract test, not live-model evidence. It uses
synthetic vectors and synthetic immutable revisions for non-default candidates,
so it says nothing about artifact availability, multilingual quality, resource
gates, or promotion eligibility. In particular, the synthetic Dragonkue score
does not contradict a missing ONNX artifact at its real pinned revision. Live
decisions are recorded in `docs/search-quality-live-model-eval.md`.

The A/B path gives every synthetic candidate an explicit immutable fixture
revision, switches the configured model before each non-default candidate,
and verifies `embedding.fingerprint_mismatch` keeps BM25 fallback active before
query inference. It then runs `qgh embed --force --json` through the debug test
embedding provider to atomically replace the active embedding generation and
retrieval publication before rerunning the same hybrid eval. The quality gate
asserts public `status`, `query`, and `get` behavior; generation row and pointer
invariants remain covered by store-level tests.

Current deterministic fixture result:

| Candidate | Configured model id | Regression hybrid path | Semantic hybrid top-5 | Semantic delta vs BM25 | Cross-language hybrid top-5 | Cross-language delta vs BM25 | Section 8.3 triggers |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| arctic-embed-l-v2.0 | `Snowflake/snowflake-arctic-embed-l-v2.0` | 15/15 | 0.92 | 0.00 | 0.88 | 0.38 | `[]` |
| dragonkue-ko | `dragonkue/snowflake-arctic-embed-l-v2.0-ko` | 15/15 | 0.92 | 0.00 | 0.88 | 0.38 | `[]` |
| gte-modernbert-base | `Alibaba-NLP/gte-modernbert-base` | 15/15 | 0.92 | 0.00 | 0.62 | 0.12 | `[]` |

Additional checks:

```json
{
  "candidate_specific_vectors": true,
  "reembedding_route": "qgh embed --force --json",
  "fingerprint_reembedding_checks": "2/2",
  "hard_filter_violations": 0,
  "combined_get_round_trip": 1.0,
  "recalibration_requires_prd_adr_update": false
}
```

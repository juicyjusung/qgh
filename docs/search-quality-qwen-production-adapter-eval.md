# Qwen 0.6B production-adapter regression evaluation

## Decision

The native Rust/Candle Qwen embedding adapter and `qgh.context.v1` improve the
previously opened 80-query multilingual split as a BM25 complement. This run is
implementation and regression evidence, not fresh model-selection or promotion
evidence. `promotion_eligible=false`.

`qwen3-embedding-0.6b` therefore remains an experimental opt-in. It is not
promoted as the light or quality preset. BM25 remains the complete production
default, and `production_v1` remains the lexical profile. The optional native
reranker passed contract tests and a live relevance-order smoke test, but this
run did not evaluate it against the production qrels; the earlier Python
reranker screening remains screening evidence only.

## Run identity

| Field | Value |
| --- | --- |
| Evaluation state | `production_adapter_previously_opened_heldout` |
| Promotion eligible | `false` |
| Evidence HEAD / dirty | `d524a94f2ae2bfe2e6594a9b23d808c857747bfd` / `false` |
| Artifact schema / SHA-256 | `qgh.qwen_runtime_eval.v1` / `5ba26157c356d95e3c92adb43dc10f6f8948a6778886482ad85b54dcfd6a915b` |
| Runtime | release build; `metal_f16` |
| Model | `qwen3-embedding-0.6b`; revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`; output dimension 384 |
| Model manifest / verified bytes | `e0915f9f5946dc0b6309e9923e5d319b81de1e54985b7c00f9f23957e2c46af4` / 1,203,010,848 |
| Corpus | 566 public sources; 823 chunks; SHA-256 `992b375ef47f31f36caef54d2798c5315d734754146528cd60a78ce5e7153ef0` |
| Held-out qrels | 80 previously opened queries; SHA-256 `1a639489b0d19f5f31a3f7065335ab34815af3b3a58d1de1b04047bc497c7c2c` |
| Provenance | SHA-256 `b1a775572250df9bad69c05b3c4472328ca2e413d8d1665ca7cafdff2866a7d9` |
| Retrieval contract | `production_v1`; RRF `k=60`; equal lexical/dense weights; candidate window 80; return limit 20 |
| Context contract | `qgh.context.v1`; chunker `markdown-token-v2:644ddfed944292cc768b74f8cba01395560121e1ef6e97a4da7735753b00f94e` |

The fixture is an unauthenticated public GitHub snapshot. The same qrels were
already inspected during earlier model work, so this run cannot choose or
promote a model without a new blind split.

## Protocol and scope

The harness rebuilt and searched a production Tantivy index with
`production_v1`. It asserted exact parity with the frozen BM25 aggregate and
class Recall values before comparing hybrid retrieval. Qwen inference used the
production tokenizer and query instruction, last-token pooling, L2
normalization, and Matryoshka truncation to 384 dimensions.

Context used the production format: `github.com/owner/repo`, issue number and
title for issues, and parent issue number and plain parent title for comments.
Each source received the maximum score from its chunks. Equal-weight RRF used
the production tie-break, while exact issue numbers and URLs bypassed semantic
ordering.

This is a retrieval-path regression, with two important boundaries:

- dense scoring uses normalized-vector brute force inside the evaluation
  harness, not a published sqlite-vec generation;
- `get`, stale/delete behavior, purge, and concurrent publication are covered
  by separate integration gates rather than inferred from these metrics.

## Quality result

| Path | Weighted nDCG@10 | Weighted MRR@10 | Macro Recall@5 / @10 / @20 | Exact top-1 | Negative non-empty rate |
| --- | ---: | ---: | ---: | ---: | ---: |
| BM25 | 0.7244 | 0.7024 | 0.7800 / 0.8200 / 0.8333 | 1.00 | 0.80 |
| Qwen hybrid | **0.8862** | **0.8559** | **0.9267 / 1.0000 / 1.0000** | 1.00 | 1.00 |

Among 75 positive queries, BM25 had 16 top-5 misses and 13 top-10 misses. The
hybrid path rescued 11 at top 5 and all 13 at top 10. It harmed zero existing
BM25 hits at either depth and preserved 59 top-5 and 62 top-10 hits.

| Class | BM25 Recall@5 | Hybrid Recall@5 | Hybrid Recall@10 / @20 | Frozen Recall@5 gate | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| English semantic | 0.95 | 1.00 | 1.00 / 1.00 | >= 0.75 | pass |
| Korean semantic | 0.87 | 1.00 | 1.00 / 1.00 | >= 0.65 | pass |
| Korean query -> English source | 0.20 | 1.00 | 1.00 / 1.00 | >= 0.60 | pass |
| English query -> Korean source | 0.50 | 0.50 | 1.00 / 1.00 | >= 0.60 | **fail** |
| Exact/identifier | 1.00 | 1.00 | 1.00 / 1.00 | top-1 >= 0.95 | pass |
| Comment-only | 0.90 | 0.90 | 1.00 / 1.00 | report | unchanged at @5 |
| Long/context-dependent | 1.00 | 1.00 | 1.00 / 1.00 | report | pass |

The run recorded zero hard-filter violations and zero source-identity
round-trip failures. The five negative queries have no relevant source and are
excluded from the weighted aggregate. Their non-empty rate increased from
0.80 to 1.00, so Qwen does not establish abstention and must not be presented as
answer confidence.

## Resource diagnostics

| Metric | Result | Evidence level | Qualification use |
| --- | ---: | --- | --- |
| Model load | 360.0 ms | one release process | not five-process cold p95 |
| Embedding throughput | 1.835 chunks/s | embedding-only upper bound | below the 3 chunks/s quality threshold; not a 50k publication run |
| Query embedding p50 / p95 | 41.3 / 51.9 ms | encoder-only | not full CLI latency |
| BM25+dense+RRF diagnostic p50 / p95 | 42.9 / 54.3 ms | in-process harness | not sqlite-vec/CLI end-to-end latency |
| Verified snapshot | 1,203,010,848 bytes | complete manifest | usable model-size evidence |
| Peak RSS | not measured | missing | blocker |
| Five-process cold p95 | not measured | missing | blocker |
| Three-run warm latency | not measured | missing | blocker |
| 50k backfill and publication | not measured | missing | blocker |
| Vector DB bytes per chunk | not measured | missing | blocker |

The resource protocol is diagnostic only. In particular, the measured
throughput cannot substitute for tokenization, persistence, publication,
checkpointing, vec0 coverage, or integrity validation at 50k chunks.

## Privacy and contract evidence

- Only the unauthenticated public fixture and local model snapshots were used.
- No hosted embedding or reranking service was used.
- The artifact contains query IDs/hashes, classes, and public source identities,
  but no raw query, title, body, token, authorization header, or absolute user
  path.
- Exact top-1, hard filters, and source identity passed in this run.
- CLI/MCP strictness, `query -> get`, stale/delete, fail-closed purge, and
  concurrent publication are verified by the final branch integration suite.
- Native reranker smoke tests verify model ordering and its bounded top-10
  fail-open contract; they are not production-qrels quality evidence.

## Promotion blockers

The artifact records all five active blockers:

1. the held-out split was previously opened;
2. a fresh blind qualification was not run;
3. the resource protocol is incomplete and diagnostic only;
4. English-query-to-Korean-source Recall@5 is below 0.60; and
5. embedding throughput is below 3 chunks/s even as an upper bound.

Consequently, no embedding preset or lexical profile is promoted. Qwen may be
released only as an explicitly experimental, separately downloaded opt-in.
The optional reranker also stays experimental and off by default.

## Reproduction

Run from a clean integrated checkout with the pinned public fixture and
prepared Qwen snapshot already available locally:

```sh
QGH_QWEN_RUNTIME_EVAL=1 \
QGH_QWEN_PREPARED_MODELS=<prepared-model-root> \
QGH_QWEN_RUNTIME_EVAL_FIXTURE_ROOT=<frozen-fixture-root> \
QGH_QWEN_RUNTIME_EVAL_OUTPUT=target/qgh-eval/qwen-runtime/qwen-production-adapter-eval.json \
cargo test --release --locked --all-features \
  --test live_model_eval \
  qwen_production_adapter_runs_canonical_window_multilingual_qrels \
  -- --ignored --exact --nocapture
```

The machine artifact remains ignored under `target/qgh-eval/`. Verify its
SHA-256, clean Git identity, and redaction before using its aggregate values.

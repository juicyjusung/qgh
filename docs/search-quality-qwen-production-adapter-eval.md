# Qwen 0.6B production-adapter regression evaluation

## Decision

The optimized native Rust/Candle Qwen embedding adapter and `qgh.context.v1`
improve the previously opened 80-query multilingual split as a BM25 complement.
This run is implementation and regression evidence, not fresh model-selection
or promotion evidence. `promotion_eligible=false`.

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
| Evidence HEAD / dirty | `34dd69c8cde4d9f97630241dc569c316e94f5063` / `false` |
| Artifact schema / SHA-256 | `qgh.qwen_runtime_eval.v1` / `d9c5f6801a0c72d7cf17fb3abdf4ddb22db2f4b6300b0d0bb07abd71f9e9ec0a` |
| Runtime | release build; `metal_f16` |
| Model | `qwen3-embedding-0.6b`; revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`; output dimension 384 |
| Model manifest / verified bytes | `e0915f9f5946dc0b6309e9923e5d319b81de1e54985b7c00f9f23957e2c46af4` / 1,203,010,848 |
| Corpus | 566 public sources; 631 chunks; SHA-256 `992b375ef47f31f36caef54d2798c5315d734754146528cd60a78ce5e7153ef0` |
| Held-out qrels | 80 previously opened queries; SHA-256 `1a639489b0d19f5f31a3f7065335ab34815af3b3a58d1de1b04047bc497c7c2c` |
| Provenance | SHA-256 `b1a775572250df9bad69c05b3c4472328ca2e413d8d1665ca7cafdff2866a7d9` |
| Retrieval contract | `production_v1`; RRF `k=60`; equal lexical/dense weights; candidate window 80; return limit 20 |
| Context contract | `qgh.context.v1`; chunker `markdown-token-v3:832a2bede22d7931c62a67aaf2fc2e147b7dce6f3b6b92c1226d66e8cbf2dfa6` |

The fixture is an unauthenticated public GitHub snapshot. The same qrels were
already inspected during earlier model work, so this run cannot choose or
promote a model without a new blind split.

## Protocol and scope

The harness rebuilt and searched a production Tantivy index with
`production_v1`. It asserted exact parity with the frozen BM25 aggregate and
class Recall values before comparing hybrid retrieval. Qwen inference used the
production tokenizer and query instruction, last-token pooling, L2
normalization, and Matryoshka truncation to 384 dimensions.

On Apple Metal F16, documents are stably ordered by token length, inputs longer
than 128 tokens use singleton micro-batches, and output vectors are restored to
their source order. Sequences longer than eight tokens use Candle's fused Metal
SDPA; CPU F32, Metal F32 reranking, and shorter sequences retain the published
generic path. The Metal adapter revision is part of the embedding fingerprint,
so an older generation cannot be reused after this numerical change.

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

## Optimization result

The previous adapter run used 823 chunks, processed 1.835 chunks/s, spent
448.5 seconds in embedding, and finished the test in 455.1 seconds. The
optimized run used 631 chunks and processed 5.562 chunks/s: 23.33% fewer
chunks, 3.03 times higher per-chunk throughput, a 3.95-times shorter embedding
phase, and a 3.81-times shorter complete evaluation. Search quality remained
at the same reported aggregate and complement values.

The gains come from two independent fixes: the v3 chunker no longer re-enters
a short prefix before the same fenced block one token at a time, and the Metal
adapter avoids materializing generic attention score tensors for supported F16
sequences. A separate deterministic probe at 912 tokens measured 2.200
chunks/s; its mixed 64/256/900-token workload measured 6.192 chunks/s. All
probe vectors were finite, cosine stayed above 0.99999, and rankings were
unchanged.

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
| Model load | 392.3 ms | one release process | not five-process cold p95 |
| Embedding throughput | 5.562 chunks/s | embedding-only upper bound | passes the 3 chunks/s diagnostic threshold; not a 50k publication run |
| Embedding phase / full test wall time | 113.5 / 119.6 s | one release process | regression evidence for this 631-chunk corpus |
| Query embedding p50 / p95 | 35.5 / 45.0 ms | encoder-only | not full CLI latency |
| BM25+dense+RRF diagnostic p50 / p95 | 37.1 / 47.2 ms | in-process harness | not sqlite-vec/CLI end-to-end latency |
| Verified snapshot | 1,203,010,848 bytes | complete manifest | usable model-size evidence |
| Peak RSS | 1,530,167,296 bytes (1.425 GiB) | `/usr/bin/time -l`, whole release test process | single-process diagnostic |
| Five-process cold p95 | not measured | missing | blocker |
| Three-run warm latency | not measured | missing | blocker |
| 50k backfill and publication | not measured | missing | blocker |
| Vector DB bytes per chunk | not measured | missing | blocker |

The resource protocol is diagnostic only. The
`qgh.qwen_runtime_process_resources.v1` sidecar (SHA-256
`82ee1f31373fe30f904739efc07ca0d87a8ca7dc2623297233c5135b98d8c47f`)
records the M4 Pro host (48 GiB), 119.86-second wall time, maximum RSS above,
and a 2,100,790,304-byte peak memory footprint. In particular, the measured
throughput cannot substitute for persistence, publication, checkpointing,
vec0 coverage, or integrity validation at 50k chunks.

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
- The vendored fastembed source is checksum-verified from crates.io, retains
  its Apache-2.0 license, and changes only the Qwen attention implementation.

## Promotion blockers

The artifact records four active blockers:

1. the held-out split was previously opened;
2. a fresh blind qualification was not run;
3. the resource protocol is incomplete and diagnostic only;
4. English-query-to-Korean-source Recall@5 is below 0.60.

The former throughput blocker is resolved for this diagnostic corpus, but that
does not turn an opened split or incomplete resource protocol into promotion
evidence.

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

# Qwen 0.6B production-adapter regression evaluation

## Decision

Qwen3-Embedding-0.6B is qgh's user-approved default semantic preset for newly
created fastembed-capable configs. It remains a separate, explicit model
download and never becomes a dependency of the complete BM25-only path.

Production hybrid ordering uses `lexical_guard_v1`, not equal-weight RRF. The
policy keeps the first five BM25 candidates in their exact lexical order, then
uses weighted RRF (`k=60`, lexical weight `2`, dense weight `1`, dense window
`80`) below that head. The reproducible 80-query regression recorded zero
observed BM25-hit harm at ranks 5 and 10 and three BM25-miss rescues at rank 10.
The lexical profile stays `production_v1`; `metadata_boost_v1` is not promoted.

This is a product-default and regression decision, not fresh-blind model
promotion evidence. The split had already been opened and the large resource
qualification protocol is incomplete, so the machine artifact correctly keeps
`promotion_eligible=false`. Optional reranking remains separately installed,
per-query opt-in, and off by default.

## Reproducible run identity

| Field | Value |
| --- | --- |
| Evaluation state | `production_adapter_previously_opened_heldout` |
| Evidence HEAD / dirty | `ac61192938b24e4bd1fe35fdfead7cfb7241ad15` / `false` |
| Machine artifact SHA-256 | `50481bce5077a92c9ad411f24d0c0a676085e3b3a19bcb2750523d1a86dcf8be` |
| Runtime | release build; `metal_f16` |
| Model | `qwen3-embedding-0.6b`; revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`; output dimension 384 |
| Model manifest / verified bytes | `e0915f9f5946dc0b6309e9923e5d319b81de1e54985b7c00f9f23957e2c46af4` / 1,203,010,848 |
| Corpus | 154 unauthenticated public sources; 165 chunks; SHA-256 `c80b1e20e342e71055a08d46402a905dff757c787cb964fbf15fbbc060cf183c` |
| Test qrels | 80 previously opened queries; SHA-256 `f279b5c1cf3eebcbc43cf4b2f3684661335160a780e851a7d67cd889963b1c43` |
| Provenance | SHA-256 `c5cddd847da9ca66a81711cc19059a9c63fcdc56deaf9a2395a1319c2f992899` |
| Retrieval contract | `production_v1`; `lexical_guard_v1`; return limit 20 |
| Context contract | `qgh.context.v1`; chunker `markdown-token-v3:832a2bede22d7931c62a67aaf2fc2e147b7dce6f3b6b92c1226d66e8cbf2dfa6` |

The committed 154-source fixture is the canonical reproducible corpus for this
adapter regression. Older reports against a later 566-source snapshot remain
historical evidence, but that external snapshot is not present in the current
checkout and is not represented as a reproducible result here.

## Protocol

The harness rebuilt and searched the production Tantivy lexical profile, first
asserted exact frozen BM25 aggregate and class parity, then ran the production
Qwen tokenizer, query instruction, last-token pooling, L2 normalization, and
384-dimensional Matryoshka output. Issue and comment inputs used
`qgh.context.v1`; the stored body and snippet were not prefixed or mutated.

Every fusion profile consumed the same BM25 and dense branch observations from
one inference pass. Production and evaluation call the same content-free
fusion module, so the qrels result cannot silently drift from the CLI policy.
Exact issue locators bypass semantic ordering. Filters are applied before both
candidate generators.

Dense scoring in this harness is normalized-vector brute force, not a published
sqlite-vec generation. Actual publication, `query -> get`, stale/delete,
purge, and concurrent snapshot behavior are verified by separate integration
gates.

## Why equal RRF was rejected

Equal weights improved aggregate nDCG and MRR but violated the intended role of
Qwen as a BM25 complement.

| Policy | Weighted nDCG@10 | Weighted MRR@10 | Macro Recall@5 / @10 | BM25 hit harm @5 / @10 | Rescue @5 / @10 | Comment Recall@5 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| BM25 | 0.5913 | 0.5974 | 0.7578 / 0.8089 | n/a | n/a | 1.00 |
| Equal RRF diagnostic | **0.7052** | **0.6901** | **0.7667** / 0.8467 | 5 / 4 | 6 / 5 | **0.00** |
| `lexical_guard_v1` | 0.6216 | 0.6023 | 0.7578 / **0.8667** | **0 / 0** | 0 / 3 | **1.00** |

With `k=60`, a vector-only rank 1 candidate scores `1/61`, which can beat a
BM25-only rank 3 candidate at `1/63`; overlap in both branch tails compounds the
problem. Protecting the lexical top five removes that failure mode. The tradeoff
is explicit: semantic retrieval does not rescue a top-5 miss unless the user
also requests the optional bounded reranker, while rank 6-10 still gains useful
semantic recall without observed lexical harm.

`rrf_rank_score` reports the weighted fusion evidence. `final_order_score`
reports reciprocal post-policy result rank before optional reranking. Neither
is confidence or probability.

## Quality result

| Class | BM25 Recall@5 / @10 | Guarded hybrid Recall@5 / @10 | Result |
| --- | ---: | ---: | --- |
| English semantic (20) | 0.7417 / 0.7833 | 0.7417 / 0.8500 | top-5 preserved; top-10 improved |
| Korean semantic (15) | 0.8667 / 0.9333 | 0.8667 / 1.0000 | top-5 preserved; top-10 improved |
| Korean query -> English source (10) | 0.2000 / 0.3000 | 0.2000 / 0.4000 | top-10 improved; weak class remains |
| English query -> Korean source (10) | 0.8000 / 0.8000 | 0.8000 / 0.9000 | top-10 improved |
| Exact/identifier (10) | 1.0000 / 1.0000 | 1.0000 / 1.0000 | hard gate passed |
| Comment-only (5) | 1.0000 / 1.0000 | 1.0000 / 1.0000 | regression prevented |
| Long/context-dependent (5) | 0.8000 / 1.0000 | 0.8000 / 1.0000 | preserved |

Among 75 positive queries, BM25 had 14 misses at rank 5 and 11 at rank 10.
The guarded path preserved all 61 BM25 hits at rank 5 and all 64 at rank 10,
and rescued three of the rank-10 misses. Exact top-1 was `1.00`; hard-filter
violations and source-map resolution failures were both zero. Actual `get`
round-trip is recorded by the separate CLI lifecycle smoke below.

The five negative queries have no relevant source and are excluded from the
weighted aggregate. Their hybrid non-empty rate is `1.00`, so Qwen does not
establish abstention and must not be presented as answer confidence.

## Actual CLI lifecycle smoke

A separate isolated profile synced the public `juicyjusung/qgh` repository
through the normal release binary and installed Qwen snapshot. After the first
complete publication, a no-change foreground sync finished in about 3.1
seconds and reported `total=468`, `staged=0`, `reused=468`, `missing=0`, and
`embedded=0`; 447 source entities were present. The following one-shot query
returned 10 results with no warning, genuine lexical and vector evidence, and
`final_order_score=1.0` for the protected first result. Its returned `get_args`
round-tripped to the same active source identity.

This smoke verifies normal GitHub sync, durable vector reuse, sqlite-vec
publication, guarded CLI fusion, and `query -> get` together. It is not folded
into the in-process latency numbers below.

After chunk-manifest attestation and cross-run batch resume hardening, the
installed-model release integration repeated `sync -> status -> hybrid query ->
get -> no-change sync -> hybrid query` successfully. The whole ignored test,
including first-sync inference, completed in 9.26 seconds. Its no-change sync
used the pinned contract and Store-owned attestations without model payload
hashing; semantic query initialization still performed full snapshot
verification.

## Resource diagnostics

| Metric | Result | Boundary |
| --- | ---: | --- |
| Verified snapshot | 1,203,010,848 bytes | model remains separately downloaded |
| Cold adapter load | 332.3 ms | one release process, not five-process p95 |
| Corpus embedding throughput | 5.923 chunks/s | 165 public chunks; not 50k publication |
| Query embedding p50 / p95 | 34.0 / 35.9 ms | encoder-only |
| BM25+dense+fusion p50 / p95 | 36.0 / 38.3 ms | in-process brute-force diagnostic |
| Full test wall time | 33.73 s | one clean release run |
| Peak RSS | not measured in this run | historical single-process evidence is not substituted |

The resource protocol remains diagnostic. Five-process cold p95, three warm
CLI repetitions, peak RSS for this exact run, 50k backfill/publication, and
vector DB bytes per chunk are still unmeasured. Normal sync mitigates practical
cost by embedding only missing chunks in bounded durable batches; a no-change
sync performs zero inference and uses trigger-invalidated source chunk
manifests plus deep generation validation without reading the model payload.
Changed or unattested chunks, `doctor`, inference, and semantic query runtime
initialization still verify the complete snapshot hash.

## Privacy and product boundaries

- Only the unauthenticated public fixture and a locally installed pinned model
  snapshot were used.
- No hosted embedding or reranking service, Python runtime, or production
  subprocess was used.
- The normal machine artifact contains query hashes and public source
  identities, not raw queries, titles, bodies, tokens, authorization headers,
  or absolute user paths.
- Qwen weights are not bundled. Only `qgh model install` may download them;
  `init`, `sync`, `query`, MCP, and runtime initialization never do.
- BM25-only remains a complete `sync -> query -> get -> cite -> status` path.
- MCP remains read-only `query`, `get`, and `status`; reranking is an optional
  parameter on the existing query tool, not a new write or model tool.

## Remaining risks

1. The 80-query split was previously opened, so a fresh blind multilingual
   qualification is still needed to estimate generalization.
2. Korean-query-to-English-source retrieval improves only at rank 10 and
   remains weak at rank 5.
3. Negative-query abstention is not implemented or demonstrated.
4. The exact large-corpus resource and persistence protocol is incomplete.
5. Apple Metal is the only GPU backend; other systems use the slower CPU F32
   embedding path, and CUDA is not implemented.

## Reproduction

```sh
QGH_QWEN_RUNTIME_EVAL=1 \
QGH_QWEN_PREPARED_MODELS=<prepared-model-root> \
QGH_QWEN_RUNTIME_EVAL_FIXTURE_ROOT=tests/fixtures/live-model-eval \
QGH_QWEN_RUNTIME_EVAL_OUTPUT=target/qgh-eval/qwen-runtime/qwen-production-adapter-eval.json \
cargo test --release --locked --all-features \
  --test live_model_eval \
  qwen_production_adapter_runs_canonical_window_multilingual_qrels \
  -- --ignored --exact
```

Machine artifacts remain ignored under `target/qgh-eval/`. Verify their hash,
clean Git identity, and redaction before using the aggregate values.

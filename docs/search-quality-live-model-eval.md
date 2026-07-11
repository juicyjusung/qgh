# Live multilingual model evaluation

## 2026-07-11 lightweight BM25-rescue follow-up

The follow-up compared three smaller multilingual models under the same public 154-source corpus, 40-query dev split, reused 80-query screening/regression split, equal RRF `k=60`, batch 8, and four ORT intra-op threads. The 80-query split was blind for the original Arctic/GTE decision, but it had already been examined before this follow-up. It is therefore no longer independent held-out evidence for choosing or promoting another model. The follow-up objective was changed from standalone dense quality to complementarity: rescue BM25 misses while preserving existing BM25 hits.

`dragonkue-koen-e5-tiny` is the best experimental screening candidate. It has the highest reused-test nDCG/MRR, the only positive top-5 rescue-minus-harm result, the smallest snapshot, and the lowest cold-start cost. It is not promoted to the production preset: Korean-query-to-English-source Recall@5 still fails, the 50k backfill crossed the 2.5 GiB quality RSS limit before completion, and a fresh blind multilingual split has not confirmed the result. BM25 remains the production default and `production_v1` remains the lexical profile.

| Candidate | Reused-test nDCG / MRR | BM25 miss rescue@5 / hit harm@5 | Rescue@10 / harm@10 | Snapshot | 50k resource result |
| --- | ---: | ---: | ---: | ---: | --- |
| Granite 97M multilingual R2 FP32 | 0.6978 / 0.6824 | 6 / 7 | 6 / 3 | 415.3 MB | stopped at 3.157 GB RSS; incomplete |
| Dragonkue KoEn E5 Tiny FP32 | **0.7235 / 0.7351** | **5 / 4** | **6 / 1** | **152.7 MB** | stopped at 2.690 GB RSS; incomplete |
| multilingual-E5-small FP32 | 0.6517 / 0.6891 | 5 / 5 | 4 / 2 | 487.4 MB | stopped at 2.907 GB RSS; incomplete |

All three passed exact/identifier, hard-filter, stale-leakage, `query -> get`, context, and redaction contracts. Granite failed English and Korean-query-to-English Recall@5. KoEn Tiny failed Korean-query-to-English Recall@5. multilingual-E5-small failed both English and Korean-query-to-English Recall@5. The resource watchdog stops a candidate after the existing quality RSS ceiling is exceeded; therefore the recorded 50k values are conclusive failures, not completed throughput measurements. The report schema retains the historical `held_out_metrics` field name for compatibility, but the follow-up values in that field are screening evidence only. A fresh blind split is required before any production promotion.

The CPU run is bound to clean HEAD `9f0b61915dfdeb99ec2d1eac1c7aba531dee2cd8`, release binary SHA-256 `2352a8671430c2645f2b93b8359453510d0dcf0307e97a7de992750962527be9`, and report `qgh.live_model_eval_report.v5` SHA-256 `18eacadadb510c4c2424577ddefe074859e7384623bdf950326db8694286e962`. It completed in 839.02 seconds with no sensitive-payload or absolute-path violation. The frozen config SHA-256 is `a8d6f93c21a1d926f363a27669a3002e026c3c0688f87d58b0e5e9957264f46d`; gate bundle SHA-256 is `0f8b1e12501f63dc812585093f391398f154f9ac40797e931af9d86559d1ad79`; model-preparation provenance SHA-256 is `3e9ab1290831db8d5e543d095ff9715c9707ef987870886dba9a11093550741d`.

### CoreML CPU+GPU probe

The best experimental candidate was also loaded through ONNX Runtime's CoreML `CPUAndGPU` execution provider. Vector parity passed, but CoreML was slower for qgh's small batch and short-query shape, so CPU remains the recommended execution provider.

| Runtime | Init | Warm batch-8 p50 / p95 |
| --- | ---: | ---: |
| ORT CPU, four intra-op threads | 264 ms | 14.80 / 15.45 ms |
| CoreML CPU+GPU, CPU fallback allowed | 2,440 ms | 38.31 / 43.69 ms |

CoreML p50 speedup was `0.386x` (about 2.59 times slower); minimum CPU/CoreML vector cosine was `0.999999999999`. The machine artifact is `qgh.coreml_model_eval.v3`, SHA-256 `ec3a11da98837ad4e4b166229dfb2cbb11e8effd5e59ffad2527f8d7009748b4`. It is bound to clean HEAD `dd8ec32a57972d7a76056194b2da8513cc716ae7`, canonical manifest SHA-256 `250de41ef56e2454c1dcc437043646487879398d32227244c0862749fc38d837`, verified captured-payload SHA-256 `93ff0e3fb474494ca47cbf10d6c96c0fb14748152969d7faa5b2f8acdb81eaf8`, five identities derived from the captured bytes used by both engines, and test-binary SHA-256 `fdf3618e68e28a4928c7dfeeb35d6dc627f1cecfb051b39bdfa21876e45b0b03`. Successful EP registration allows CPU+GPU execution but does not prove that every graph node ran on the GPU, so this result must not be described as a pure-GPU benchmark.

### Preset integrity correction

The pinned Granite `model_quint8_avx2.onnx` artifacts contain `DynamicQuantizeLinear` nodes even though the previous preset names declared static INT8. They are also AVX2-targeted and unsuitable as the Apple ARM64 default. The mislabeled presets were removed, the Granite presets now point to pinned FP32 `onnx/model.onnx`, and prepared ONNX graphs containing dynamic quantization fail closed when their manifest declares `none` or `static`.

## Original Arctic/GTE decision

The integrated live run completed against public GitHub Issues and comments with real local model artifacts. It selected neither a light nor a quality candidate, so no embedding preset is promoted. The existing optional `Snowflake/snowflake-arctic-embed-l-v2.0` default remains unchanged as a compatibility control; this evaluation does not newly approve it as a resource-qualified preset.

`metadata_boost_v1` is also not promoted. It improved dev weighted nDCG@10, but both lexical profiles failed the Korean Recall@5 quality gate. `production_v1` therefore remains the production lexical profile.

These model decisions do not widen MVP scope. BM25-only remains the complete default path, and no hosted embedding, reranker, ANN, sparse retriever, or new MCP tool was added.

## Run identity

The final canonical rerun is bound to a clean integrated HEAD. Machine artifacts remain ignored under `target/qgh-eval/`; only this evidence summary is committed.

| Field | Value |
| --- | --- |
| Run status | `completed_not_eligible`; blocker `no_passing_candidate`; 4942.73 s |
| Run finished | `2026-07-11T02:29:37Z` |
| Git HEAD | `3cf8c1fef972fd03b6d8cc7a6b3fe8e52942e14c` |
| Release binary SHA-256 | `70d65f5856796e5aa28826fd783d75924a71f63974efb17ca57c71025fc5e47e` |
| Final report | `qgh.live_model_eval_report.v4`, SHA-256 `a810596c9caae6f88e8b8d332eb6f43c667d97dfd8b93bdb95971a038b60b923` |
| Frozen config | `qgh.live_model_eval_config.v5`, SHA-256 `20948e812eeeb41ef4e0a870bdd8abcfe2e1aec555ccbc3afc39e8f838b1eb54` |
| Gate bundle | `qgh.live_model_eval_verified_gate_bundle.v3`, SHA-256 `17eceb6c8d24e6462320ae948ba498fecce4ffb1f0473220738259b8dfd36966` |
| Model preparation provenance | SHA-256 `7513f1fca7488bfe1eb191851b042f69d08722133234cc1d55f0d517e3ef9b7f` |
| Reference host | `Mac16,8`, Apple M4 Pro 14-core, 48 GB, macOS 26.5.1, AC power, Low Power Mode off |
| Runtime | fastembed `5.17.2`, ORT `2.0.0-rc.12` |

The host identity and power conditions matched the frozen reference protocol and recorded no host-protocol failures. The candidate runtime protocol did not match: both runnable models used batch 16 instead of required batch 8 and did not expose required intra-op threads 4.

## Public fixture and provenance

The snapshot contains only unauthenticated public `juicyjusung/qgh` Issues and issue comments acquired from GitHub REST at `2026-07-10T08:20:22Z`. It has 154 sources: 71 issues and 83 comments. Pull requests, empty bodies, operational loop issues #18/#19, secret-like payloads, comments containing absolute local user-home paths, and ambiguous candidates without a second adjudication are excluded.

| Artifact | Contract |
| --- | --- |
| Corpus | 154 sources; SHA-256 `c80b1e20e342e71055a08d46402a905dff757c787cb964fbf15fbbc060cf183c` |
| Dev qrels | 40 queries; SHA-256 `7e4daa6376fff4f013b088596c4b98ce99aa52340cc7df76046f82ed1d555494` |
| Held-out qrels | 80 queries; SHA-256 `f279b5c1cf3eebcbc43cf4b2f3684661335160a780e851a7d67cd889963b1c43` |

Held-out classes are English semantic 20, Korean semantic 15, Korean-query-to-English-source 10, English-query-to-Korean-source 10, exact/identifier 10, comment-only 5, long/context-dependent 5, and negative 5. An issue and its comments never cross the split. Twelve held-out queries have manually pooled alternate sources, and every held-out record names two adjudicators.

## Frozen protocol and contracts

All dev runs finish before `frozen-config.json` is written; held-out JSONL is parsed only after that write. The harness revalidates the clean Git HEAD, release binary, gate bundle and result hashes, model-preparation provenance, prepared model snapshots, BM25 database/Tantivy snapshot, candidate schema fingerprints, tokenizer identity, and tokenizer-derived chunker fingerprint immediately before and after held-out evaluation, before and after each 50k run, and immediately before final-report publication.

Production retrieval uses `qgh.context.v1`, equal RRF with `k=60`, query limit 20, and candidate window 80. The candidate-specific chunk fingerprints are:

- Arctic: `markdown-token-v2:6fd8b725028a0a80cc71108a6c6babf5ea5af534436bacd0ae762ad4d33e8d6e`
- GTE ModernBERT: `markdown-token-v2:e4a55c171b717a1c4b518f83495258b7818885d90bd8e0ffc291c25d29538b48`

Six release-profile gates are bound to the same Git and release-binary identity and each observed exactly one passing test. The `bm25_search_quality` gate additionally requires and executes the exact canonical `target/release/qgh` binary; the other five gates exercise their owning release-profile test contracts without claiming a binary witness:

1. `edit_reconciliation`
2. `delete_and_stale_exclusion`
3. `purge_pending_retry`
4. `parent_context_invalidation`
5. `concurrent_publication_snapshot`
6. `bm25_search_quality`

Each prepared candidate also passed `qgh.candidate_hybrid_filter_contract.v2`: seven competing sources and seven embedded chunks, four filtered queries, 17 hybrid results with both lexical and vector branches, and two exact issue-filter queries. Arctic checked 71 issue and 83 comment context rows; GTE checked 86 issue-chunk and 83 comment rows. Both reported zero context-hash mismatches.

The redaction audit scans captured diagnostics, generated JSON/JSONL, partial/fragment/canary files, gate artifacts, manifests, and preparation provenance. Immutable third-party model payloads and tokenizer vocabularies are inputs rather than qgh-authored output and are excluded. Structured `repo_policy_path` values are allowed only at the four CLI/MCP JSON pointers defined by the public output contract; the same path anywhere else in stdout, stderr, or an artifact still fails. Raw query/body values are never serialized into normal events or failure records.

The final audit checked 9,888 captured stdout streams, 43 stderr streams, 59 artifact files, and six path markers. It recorded zero violation artifacts; sensitive-payload, path-privacy, and combined redaction status all passed. `raw_query_or_body_logged=false` and `absolute_path_logged=false`.

## Lexical profile result

| Dev profile | Weighted nDCG@10 | Weighted MRR@10 | Exact top-1 | Round-trip | Quality gate failure | Decision |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| `production_v1` | 0.5830 | 0.5063 | 1.00 | 1.00 | `korean_recall_at_5` | keep |
| `metadata_boost_v1` | 0.7227 | 0.6800 | 1.00 | 1.00 | `korean_recall_at_5` | reject |

The frozen dev selection is `production_v1`; held-out confirmation therefore records `promotion_eligible=false` with blocker `dev_selection_is_production_v1`. Production held-out weighted nDCG@10 is 0.5913 and MRR@10 is 0.5974.

## Live quality result

All rows use the same frozen lexical profile and real `query -> get` execution. Exact top-1 and `get` round-trip are 1.00; hard-filter violations, stale leakage, and duplicate crowding are zero.

| Candidate | Dev nDCG@10 | Dev MRR@10 | Held-out nDCG@10 | Held-out MRR@10 | Held-out hybrid coverage | Held-out quality failures |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| BM25-only | 0.5830 | 0.5063 | 0.5913 | 0.5974 | n/a | English Recall@5; Korean-to-English Recall@5 |
| Arctic | 0.8302 | 0.8052 | 0.7220 | 0.7069 | 65/65 | English Recall@5; Korean-to-English Recall@5 |
| GTE ModernBERT | 0.5658 | 0.4875 | 0.6378 | 0.6770 | 65/65 | English Recall@5; Korean-to-English Recall@5 |

Held-out class detail. Each cell is `nDCG@10 / MRR@10 / Recall@5 / Recall@10`:

| Class | BM25 | Arctic | GTE |
| --- | ---: | ---: | ---: |
| English semantic | 0.6335 / 0.7017 / 0.7417 / 0.7833 | 0.7498 / 0.7592 / 0.6667 / 0.8667 | 0.7457 / 0.8688 / 0.6583 / 0.8000 |
| Korean semantic | 0.8310 / 0.7984 / 0.8667 / 0.9333 | 0.9128 / 0.8833 / 1.0000 / 1.0000 | 0.7052 / 0.6528 / 0.8000 / 0.8667 |
| Korean query -> English source | 0.1377 / 0.0875 / 0.2000 / 0.3000 | 0.2964 / 0.2643 / 0.3000 / 0.4000 | 0.0431 / 0.0250 / 0.1000 / 0.1000 |
| English query -> Korean source | 0.6000 / 0.5333 / 0.8000 / 0.8000 | 0.8351 / 0.7850 / 0.9000 / 1.0000 | 0.8049 / 0.7417 / 0.9000 / 1.0000 |
| Exact/identifier | 1.0000 / 1.0000 / 1.0000 / 1.0000 | 1.0000 / 1.0000 / 1.0000 / 1.0000 | 1.0000 / 1.0000 / 1.0000 / 1.0000 |
| Comment-only | 0.4861 / 0.3167 / 1.0000 / 1.0000 | 0.6117 / 0.4983 / 0.6000 / 1.0000 | 0.6262 / 0.5667 / 0.8000 / 0.8000 |
| Long/context-dependent | 0.6236 / 0.5000 / 0.8000 / 1.0000 | 0.8524 / 0.8000 / 1.0000 / 1.0000 | 0.8524 / 0.8000 / 1.0000 / 1.0000 |

Negative-query top-result rate was 0.80 for BM25 and 1.00 for both hybrid candidates, so no candidate demonstrated reliable abstention on this five-query slice.

## Model and resource result

| Candidate | Snapshot | Cold p95 | Warm p50 / p95 (n) | Peak RSS | Quality-corpus embed / DB growth | 50k result | Stable blocker |
| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| Arctic | 2.285 GB | 11.522 s | 125.5 / 142.0 ms (240) | 6.753 GB | 154 / 68.61 s / 2.24 chunks/s / 33,885 B/chunk | tokenization did not produce the required public 900-token chunk | `eval.resource_failed @ 50k_tokenize` |
| GTE ModernBERT | 600.0 MB | 2.936 s | 105.4 / 122.0 ms (240) | 15.075 GB | 169 / 50.16 s / 3.37 chunks/s / 25,012 B/chunk | 900 raw / 926 contextual tokens; embed exited after 4215.29 s without a completed 50k generation | `eval.resource_failed @ 50k_embed` |
| Dragonkue Korean | unavailable | n/a | n/a | n/a | n/a | pinned revision lacks the required ONNX artifact | `eval.model_artifact_missing_at_immutable_revision @ preparation_provenance` |

Arctic cold samples were 11,522.1, 10,365.1, 10,256.3, 10,216.0, and 10,040.2 ms. GTE samples were 2,935.6, 2,679.9, 2,654.2, 2,653.9, and 2,662.5 ms. The prepared snapshots were already local, so download transfer bytes were zero.

Arctic exceeds the light and quality cold-start and RSS limits; its snapshot also exceeds the light limit. GTE passes both cold-start limits but exceeds both RSS limits and the light snapshot limit. Both pass the warm-latency limits, use effective batch 16 instead of required batch 8, and do not expose required intra-op threads 4. Neither produced complete 50k throughput, DB-growth, publication, vec0, or backfill-integrity evidence, so the remaining backfill limits are unmeasured rather than passed. `synthetic_substitution=false` for every candidate.

## Promotion and conditional follow-up

- Lexical profile: keep `production_v1`; do not promote `metadata_boost_v1`.
- Embedding preset: promote none; selected light and quality candidates are both `null`.
- Existing optional Arctic default: retain unchanged for compatibility, without a new resource-readiness claim.
- Korean lexical follow-up: the dev Korean Recall@5 miss activates investigation of the existing NFC/ngram path. Lindera is only an ADR candidate after that cheaper tuning is measured; it is not implemented here.
- Reranker: not triggered because the observed failures are recall failures, not passing recall with deficient top-rank precision/MRR.
- Late chunking: not triggered; the long/context class did not identify context loss as the dominant failure.
- ANN: not triggered; the 50k runs failed before a valid brute-force latency/throughput result existed.
- Sparse retriever: not triggered; the run did not establish a repeated dense-plus-BM25 lexical-expansion failure.

## Reproduction

Prepare already pinned artifacts without network access:

```sh
python3 tests/support/prepare_live_model_eval_models.py \
  --output-root target/qgh-eval/models \
  --offline
```

Run the canonical release evaluation from a clean integrated HEAD:

```sh
QGH_LIVE_MODEL_EVAL=1 \
QGH_LIVE_MODEL_EVAL_ROOT=target/qgh-eval \
cargo test --release --locked --all-features \
  --test live_model_eval live_model_runtime_evaluation \
  -- --ignored --exact --nocapture
```

The authoritative ignored artifacts are `live-model-eval-report.json`, `frozen-config.json`, `lexical-profile-ab-report.json`, `contract-gate-bundle.json`, the six `contract-gates/*.json` files, per-candidate event files, and prepared model provenance/manifests.

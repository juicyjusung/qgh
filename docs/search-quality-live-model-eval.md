# Live multilingual model evaluation

## Status

The committed fixture and harness are ready for the integrated live run. No model or preset is selected by this lane. Numbers produced from this branch alone are pre-integration harness validation, not final quality or promotion evidence: the prepared manifests still declare `qgh.context.none.v1`, while embedding generations use `qgh.context.v1`. The full model, held-out, and 50,000-chunk run must execute once on the Lane D+A+B integrated SHA after the context builder, provider inputs, hashes, and validation all agree on `qgh.context.v1`.

An earlier mixed-repository Arctic run was terminated before its resource phase. It is excluded from evidence. The corrected qgh-only runtime smoke did execute the release binary through sync, MCP query, and real `get`; it reported `get_round_trip=1.0`, no raw query/body logging, and artifact SHA-256 `76d00230c3a79a53622126322ea1fbd147690c899fbe5f9b7db15d56b8cd54e1` under ignored `target/qgh-eval/`.

## Public fixture

The snapshot contains only public `juicyjusung/qgh` Issues and issue comments acquired without authentication from GitHub REST at `2026-07-10T08:20:22Z`. Pull requests, empty bodies, operational loop issues #18/#19, secret-like payloads, and ambiguous candidates without a second adjudication are excluded. No private content or second repository is present.

| Artifact | Contract |
| --- | --- |
| Corpus | 156 qgh issue/comment sources; SHA-256 `8333e4a5291d911fcafcf23319b30445e18e526c93f0a929ef898240131c5df1` |
| Dev qrels | Exactly 40; SHA-256 `7f56373c239c3fe37d04edb90cecc4b13972209dc168c2b515b5b4a10b3efecc` |
| Held-out qrels | Exactly 80; SHA-256 `1fd49bc903cb6c0287ef3627993b05eda9eeccad09ec1b1ca0e8281c0ca5299d` |

Held-out counts are English semantic 20, Korean semantic 15, Korean-query-to-English-source 10, English-query-to-Korean-source 10, exact/identifier 10, comment-only 5, long/context-dependent 5, and negative 5. Each gold record has a grade, source identity, rationale, and labeler. The English semantic items were adjudicated from source-body acceptance or normative sections; title-only paraphrases are not allowed. An issue and all of its comments remain in one split.

## Frozen evaluation protocol

All BM25 and candidate dev runs finish before `frozen-config.json` is written. The held-out set is opened only after that write. Repeated held-out passes are permitted only for latency under the same frozen production configuration; the final pass supplies the one scored ranking and real qgh Store/`get` round-trip.

Production fusion is equal RRF with `k=60`. The production query limit is 20 and `HYBRID_OVERFETCH_FACTOR=4`, so the actual candidate window is 80. qgh exposes no runtime seam for either value. A dev-only offline diagnostic attempts `k={20,60,100}` and windows `{40,80,100}` from a separate `limit=100` query. A cell is marked incomplete when the fused MCP response does not expose enough lexical and vector branch candidates; missing candidates are never treated as absent or zero, and this diagnostic cannot select a deployable configuration.

Reports include nDCG@10, MRR@10, Recall@5/@10/@20, exact top-1, hard-filter violations, real `get` round-trip, duplicate crowding, and negative top-result rate. The live corpus does not inject a stale source, so it records stale leakage as unverified rather than a misleading zero. Promotion additionally requires a passed result hash from:

```sh
cargo test --all-features --test issue_body_tracer full_reconciliation_tombstones_deleted_comments_and_updates_status -- --exact
```

That focused contract gate passed on this lane (`1 passed`, 116 filtered out), but it must run again on the integrated SHA and be supplied to the live report.

## Models and resource protocol

The candidates are pinned to:

- `Snowflake/snowflake-arctic-embed-l-v2.0@ac6544c8a46e00af67e330e85a9028c66b8cfd9a`
- `dragonkue/snowflake-arctic-embed-l-v2.0-ko@55ec6e9358a56d56af759bc8372e970caf8c305f`
- `Alibaba-NLP/gte-modernbert-base@e7f32e3c00f91d699e8c43b53106206bcc72bb22`

Dragonkue currently has no `onnx/model.onnx` in the pinned public revision. The existing fastembed `UserDefinedEmbeddingModel` path therefore returns a real `embedding.hf_download_failed` blocker; no synthetic replacement is allowed.

The reference host is `Mac16,8 / Apple M4 Pro 14-core / 48 GB / macOS 26.5.1`, on AC power with Low Power Mode off. The manifest records sanitized `system_profiler`, `sw_vers`, current power mode, rustc/cargo, fastembed `5.17.2`, and ORT `2.0.0-rc.12`; a mismatch blocks promotion.

The resource run starts from exactly 50,000 repeated public qgh chunks verified as 900 tokens by the candidate tokenizer. Seed insertion finishes first. The harness then verifies the exact count, checkpoints/truncates WAL, records normalized main-DB-plus-WAL bytes, and only then times production `qgh embed --force`. It checkpoints again before computing vector DB growth, so seed body bytes and seed time are excluded. Peak RSS is isolated per candidate. Warm query latency intentionally includes the current end-to-end manifest path, including the current artifact read and SHA-256 validation before the runtime-cache lookup on each request; the harness does not normalize that cost away. The required protocol is batch 8 and intra-op threads 4; the current runtime hard-codes batch 16 and does not expose intra-op threads, so those mismatches remain promotion blockers even when effective-runtime measurements complete.

## Required integrated run

After context-v1 integration, build the release binary, prepare the pinned manifests, rerun the stale contract gate, and launch the ignored live test with its result SHA-256 in the two `QGH_LIVE_MODEL_EVAL_STALE_GATE_*` variables. Machine artifacts stay under ignored `target/qgh-eval/`. The run must complete BM25, all reachable model profiles, five fresh-process cold samples, warmup plus three measured passes, and the corrected 50,000-chunk profile before any candidate can be considered.

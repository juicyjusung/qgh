# Live multilingual model evaluation

## Status

The fixture and harness are ready for the integrated live run, but this lane selects no model. Eligibility is derived from recorded context, quality, resource, host, stale, hard-filter, judgment-pool, and redaction evidence; it is not hard-coded.

Prepared manifests now declare `qgh.context.v1`. A small real GTE run over one public issue thread proved that the current branch still hashes raw chunks: all 3 stored generation context hashes disagreed with the deterministic issue/comment metadata inputs, so the probe derived `blocked_context_contract`. The probe also captured the post-embed `d768` candidate database fingerprint `77e5b4fb0f7242357c5b123079c245727dd88a3092aee827c8b2940e293431d4`; its ignored artifact SHA-256 is `ca2874bf433b3e8c24a908a09036d6c5d6241abed0589d2fdd3f2b67a8d4a650`. The same probe must return zero mismatches after the Lane D context builder is integrated, before held-out or 50,000-chunk evidence can be eligible.

The corrected qgh-only runtime smoke executed the release binary through sync, MCP query, and real `get`; it reported `get_round_trip=1.0`, verified stderr/artifact redaction, and captured database and Tantivy schema fingerprints. Artifact SHA-256: `15ae5036085ce34371dfa14a1331cd865b864dcab0ff8cea250a2ab8544ea5b3` under ignored `target/qgh-eval/`.

## Public fixture

The snapshot contains only public `juicyjusung/qgh` Issues and issue comments acquired without authentication from GitHub REST at `2026-07-10T08:20:22Z`. Pull requests, empty bodies, operational loop issues #18/#19, secret-like payloads, two comments containing absolute local user-home paths, and ambiguous candidates without a second adjudication are excluded. No private content or second repository is present.

| Artifact | Contract |
| --- | --- |
| Corpus | 154 qgh issue/comment sources; SHA-256 `c80b1e20e342e71055a08d46402a905dff757c787cb964fbf15fbbc060cf183c` |
| Dev qrels | Exactly 40; SHA-256 `7e4daa6376fff4f013b088596c4b98ce99aa52340cc7df76046f82ed1d555494` |
| Held-out qrels | Exactly 80; SHA-256 `f279b5c1cf3eebcbc43cf4b2f3684661335160a780e851a7d67cd889963b1c43` |

Held-out counts are English semantic 20, Korean semantic 15, Korean-query-to-English-source 10, English-query-to-Korean-source 10, exact/identifier 10, comment-only 5, long/context-dependent 5, and negative 5. Twelve held-out queries have manually pooled alternate qgh sources with graded relevance, including the overlapping product contract for `test-001`; every record names two adjudicators. The pool is complete for the defined snapshot/body-overlap review, not a claim of exhaustive semantic truth outside that bounded pool. Title-only paraphrases are forbidden, and an issue and all comments remain in one split.

## Frozen evaluation protocol

All BM25 and candidate dev runs finish before `frozen-config.json` is written. The held-out JSONL is not parsed until after that write. Frozen evidence includes corpus/qrels, BM25 database-schema, Tantivy-schema, model-manifest, chunker, context, fusion, and lexical-profile fingerprints. Each candidate report additionally records its own post-embed database and Tantivy fingerprints so dimension-specific vector tables are part of that model's evidence.

Production fusion is equal RRF with `k=60`. The production query limit is 20 and `HYBRID_OVERFETCH_FACTOR=4`, so the actual candidate window is 80. qgh exposes no runtime seam for either value. A dev-only offline diagnostic attempts `k={20,60,100}` and windows `{40,80,100}` from a separate `limit=100` query. Cells lacking provable lexical/vector branch coverage are incomplete; missing candidates are never zero-filled, and the diagnostic cannot select a deployable configuration.

Reports include nDCG@10, MRR@10, Recall@5/@10/@20, exact top-1, hard-filter violations, real `get` round-trip, duplicate crowding, and negative top-result rate. Weighted quality uses English `.50`, Korean `.20`, Korean-query-to-English-source `.15`, English-query-to-Korean-source `.10`, comment `.025`, and long/context `.025`. Every model-scored semantic/comment/long query must expose a real hybrid-ranked result; a BM25 fallback fails the quality gate. The light tier first keeps candidates within 0.02 weighted nDCG@10 of the best passing model, then selects the smallest snapshot. The quality tier keeps candidates within 0.005 weighted nDCG@10 of the best, then uses weighted MRR@10 and finally snapshot size as tie-breakers.

The live corpus does not inject stale state and its single-repository filters are insufficient as standalone hard-filter evidence. Promotion therefore requires passed result hashes for both external gates:

```sh
cargo test --all-features --test issue_body_tracer full_reconciliation_tombstones_deleted_comments_and_updates_status -- --exact
cargo test --all-features --test live_model_eval production_hard_filter_contract_excludes_competing_sources -- --exact
```

The hard-filter gate syncs seven same-sentinel sources: the target issue, wrong-label, wrong-state, and wrong-author issues, a fully matching different issue, the target issue's comment, and the same issue number in a competing repository. It first verifies the production BM25 path, then uses the built-in pinned `arctic-l-v2-fp32` local preset with debug-only deterministic document/query vectors to publish a real sqlite-vec generation without model download or hosted egress. Repository, label, state, author, and issue-only source-type policy filters run through both candidate generators; all 17 reachable hybrid results must report `ranking.kind=hybrid` with both `lexical_score` and `vector_distance`, while closer vector-dominant competitors remain excluded. The current product contract resolves any explicit `--issue` filter through the exact-locator early return before candidate generation, so two issue-filter combinations are separately required to report `ranking.kind=exact`; integration must retain the owning Store/Index issue-prefilter unit coverage.

Normal-run stderr is retained only in memory for comparison against every raw query/body. Generated event/report/frozen artifacts are also scanned. The report derives its redaction status from that audit and blocks promotion on any violation.

## Models and resource protocol

The candidates are pinned to:

- `Snowflake/snowflake-arctic-embed-l-v2.0@ac6544c8a46e00af67e330e85a9028c66b8cfd9a`
- `dragonkue/snowflake-arctic-embed-l-v2.0-ko@55ec6e9358a56af759bc8372e970caf8c305f`
- `Alibaba-NLP/gte-modernbert-base@e7f32e3c00f91d699e8c43b53106206bcc72bb22`

Dragonkue currently has no `onnx/model.onnx` in the pinned public revision. The existing fastembed `UserDefinedEmbeddingModel` path therefore reports `embedding.hf_download_failed`; no synthetic replacement is allowed.

The reference host is `Mac16,8 / Apple M4 Pro 14-core / 48 GB / macOS 26.5.1`, on AC power with Low Power Mode off. The manifest records sanitized hardware/OS/power fields, rustc/cargo, fastembed `5.17.2`, and ORT `2.0.0-rc.12`; a mismatch blocks promotion.

The resource run starts from exactly 50,000 repeated public qgh chunks verified as 900 tokens by the candidate tokenizer. Seed insertion, exact count, and WAL checkpoint finish before the timer and storage baseline. Production `qgh embed --force` alone supplies throughput; a second checkpoint normalizes main-DB-plus-WAL growth. Warm latency includes current end-to-end artifact read and SHA-256 validation before runtime-cache lookup. Required batch 8 and intra-op 4 remain blockers because the current runtime uses batch 16 and does not expose intra-op.

## Required integrated run

After context-v1 integration, rebuild qgh, prepare the pinned manifests, run both external contract gates, and pass their status/result hashes through the documented `QGH_LIVE_MODEL_EVAL_{STALE,FILTER}_GATE_*` variables. Machine artifacts stay under canonical ignored `target/qgh-eval/`; lookalike, parent-traversal, and symlink escape paths are rejected. Only then run BM25, all reachable model profiles, five fresh-process cold samples, warmup plus three measured passes, and the corrected 50,000-chunk profile.

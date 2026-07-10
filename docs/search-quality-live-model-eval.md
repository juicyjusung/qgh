# Live multilingual model evaluation

## Status

The fixture and harness are ready for the integrated live run, but this lane selects no model. Eligibility is derived from recorded context, quality, resource, host, lifecycle, hard-filter, judgment-pool, and redaction evidence; it is not hard-coded.

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

All BM25 and candidate dev runs finish before `frozen-config.json` is written. The held-out JSONL is not parsed until after that write. The frozen file records the integrated git HEAD, a required clean-worktree flag, release-binary SHA-256, canonical gate-bundle SHA-256, model-preparation-provenance SHA-256, every candidate's dev state and dev metric/diagnostic hashes, manifest hash and file hash, manifest artifact-set hash, complete prepared-snapshot hash/bytes/file count, corpus/qrels, BM25 database-schema, Tantivy-schema, chunker, context, fusion, and lexical-profile state. Each candidate report additionally records its own post-embed database and Tantivy fingerprints so dimension-specific vector tables are part of that model's evidence.

The harness re-hashes the frozen file, release binary, gate bundle and gate result files, preparation provenance, and every complete candidate snapshot and re-checks git HEAD/worktree cleanliness immediately before opening held-out qrels and again before each 50,000-chunk run. A changed input fails the phase instead of mixing evidence from different builds.

Production fusion is equal RRF with `k=60`. The primary dev metric uses the production query limit 20 and `HYBRID_OVERFETCH_FACTOR=4`, so its actual candidate window is 80. qgh exposes no runtime seam for either value. A separate dev-only offline diagnostic attempts `k={20,60,100}` and windows `{40,80,100}` from a distinct `limit=100` query. Cells lacking provable lexical/vector branch coverage are incomplete; missing candidates are never zero-filled, and no diagnostic cell can select a deployable configuration.

The lexical profile is frozen as a typed `production_v1` state. `metadata_boost_v1` remains `pending_integrated_lane_d_ab` with `may_select=false`; the live V1-versus-boost A/B and any promotion decision are a TODO for the integrated Lane D revision, not this isolated lane.

Reports include nDCG@10, MRR@10, Recall@5/@10/@20, exact top-1, hard-filter violations, real `get` round-trip, duplicate crowding, and negative top-result rate. Weighted quality uses English `.50`, Korean `.20`, Korean-query-to-English-source `.15`, English-query-to-Korean-source `.10`, comment `.025`, and long/context `.025`. Every model-scored semantic/comment/long query must expose a real hybrid-ranked result; a BM25 fallback fails the quality gate. The light tier first keeps candidates within 0.02 weighted nDCG@10 of the best passing model, then selects the smallest snapshot. The quality tier keeps candidates within 0.005 weighted nDCG@10 of the best, then uses weighted MRR@10 and finally snapshot size as tie-breakers.

The live corpus does not inject lifecycle races and its single-repository filters are insufficient as standalone hard-filter evidence. Promotion therefore requires the following seven gates. The ignored live harness executes these exact argument vectors directly through `cargo` after it verifies the integrated clean HEAD and release-binary identity; no shell or user-authored pass artifact is involved:

```sh
cargo test --all-features --test issue_body_tracer sync_issue_refreshes_target_issue_and_reconciles_comment_diff -- --exact
cargo test --all-features --test issue_body_tracer full_reconciliation_tombstones_deleted_comments_and_updates_status -- --exact
cargo test --all-features store::tests::purge_retry_finishes_idempotently_and_clears_pending -- --exact
cargo test --all-features embedding::tests::parent_issue_title_change_invalidates_comment_context_hash -- --exact
cargo test --all-features --test issue_body_tracer concurrent_cli_sync_and_mcp_reads_keep_index_queryable -- --exact
cargo test --all-features --test search_quality_eval
cargo test --all-features --test live_model_eval production_hard_filter_contract_excludes_competing_sources -- --exact
```

The names are fixed as `edit_reconciliation`, `delete_and_stale_exclusion`, `purge_pending_retry`, `parent_context_invalidation`, `concurrent_publication_snapshot`, `bm25_search_quality`, and `hard_filter_exclusion`. After each successful command, the harness derives the passed-test count from captured cargo output and atomically publishes `target/qgh-eval/contract-gates/<name>.json` with strict schema `qgh.live_model_eval_gate_result.v1`, name, integrated git SHA, release-binary SHA-256, exact command, exit status `0`, observed test count exactly `1`, a SHA-256 of the captured stdout/stderr bytes, and result `passed`. Captured command output is not written to disk. A cargo invocation that fails or matches zero or multiple tests stops the run and is not evidence. Only after all seven gates pass does the harness atomically publish `target/qgh-eval/contract-gate-bundle.json`; strict schema `qgh.live_model_eval_gate_bundle.v1` repeats the git/binary identity and records each exact name/command/status plus the confined result-artifact path and SHA-256 of its actual bytes. The harness then reads the files, verifies the hashes and identities, rejects missing/extra/reordered gates, path traversal and symlinks, freezes the bundle hash, and revalidates the chain before held-out evaluation, before and after every 50,000-chunk run, and immediately before final-report publication. Environment status or SHA strings are not evidence.

The hard-filter gate syncs seven same-sentinel sources: the target issue, wrong-label, wrong-state, and wrong-author issues, a fully matching different issue, the target issue's comment, and the same issue number in a competing repository. It first verifies the production BM25 path, then uses the built-in pinned `arctic-l-v2-fp32` local preset with debug-only deterministic document/query vectors to publish a real sqlite-vec generation without model download or hosted egress. Repository, label, state, author, and issue-only source-type policy filters run through both candidate generators; all 17 reachable hybrid results must report `ranking.kind=hybrid` with both `lexical_score` and `vector_distance`, while closer vector-dominant competitors remain excluded. The current product contract resolves any explicit `--issue` filter through the exact-locator early return before candidate generation, so two issue-filter combinations are separately required to report `ranking.kind=exact`; integration must retain the owning Store/Index issue-prefilter unit coverage.

Normal-run stderr is retained only in memory for comparison against every raw query/body. Every generated JSON/JSONL artifact, the gate bundle and gate results, and partial/fragment/canary files are scanned recursively. Failure records contain only a stable code and phase; raw error text is not serialized. The report derives its redaction status from that audit and blocks promotion on any violation.

## Models and resource protocol

The candidates are pinned to:

- `Snowflake/snowflake-arctic-embed-l-v2.0@ac6544c8a46e00af67e330e85a9028c66b8cfd9a`
- `dragonkue/snowflake-arctic-embed-l-v2.0-ko@55ec6e9358a56af759bc8372e970caf8c305f`
- `Alibaba-NLP/gte-modernbert-base@e7f32e3c00f91d699e8c43b53106206bcc72bb22`

Dragonkue currently has no `onnx/model.onnx` in the pinned public revision. The existing fastembed `UserDefinedEmbeddingModel` path therefore reports `embedding.hf_download_failed`; no synthetic replacement is allowed. `prepare_live_model_eval_models.py` writes strict ignored `models/preparation-provenance.json` with per-artifact source (`curl`, local cache, or existing snapshot), source bytes, actual curl transfer bytes, manifest hash, complete snapshot hash, and aggregate byte counts. The harness validates it before dev and freezes its hash.

The reference host is `Mac16,8 / Apple M4 Pro 14-core / 48 GB / macOS 26.5.1`, on AC power with Low Power Mode off. The manifest records sanitized hardware/OS/power fields, rustc/cargo, fastembed `5.17.2`, and ORT `2.0.0-rc.12`; a mismatch blocks promotion.

The resource run starts from exactly 50,000 repeated public qgh chunks verified as 900 raw-body tokens by the candidate tokenizer; it separately records the tokenizer count after the deterministic issue metadata context is applied. Seed insertion, exact count, and WAL checkpoint finish before the timer and storage baseline. Production `qgh embed --force` alone supplies throughput; a second checkpoint normalizes main-DB-plus-WAL growth. Success additionally requires one active publication whose embedding generation declares total/completed 50,000 and has exactly 50,000 generation-chunk rows, vector mappings, and joined dimension-specific vec0 rows. Warm latency includes current end-to-end artifact read and SHA-256 validation before runtime-cache lookup. Required batch 8 and intra-op 4 remain blockers because the current runtime uses batch 16 and does not expose intra-op.

If held-out scoring succeeds but a resource phase fails, the candidate report preserves held-out metrics plus all numeric resource evidence collected so far and the exact failing phase. It does not replace those metrics with `null`; the blocker remains only `{code, phase}` and the candidate cannot be promoted.

## Required integrated run

After context-v1 integration, commit the integrated worktree, build the release binary, prepare the pinned manifests, execute all seven external gates against that exact HEAD/binary, and create their canonical result files and bundle. Machine artifacts stay under canonical ignored `target/qgh-eval/`; lookalike, parent-traversal, and symlink escape paths are rejected. Only then run BM25, all reachable model profiles, five fresh-process cold samples, warmup plus three measured passes, and the corrected 50,000-chunk profile. Do not use the old `QGH_LIVE_MODEL_EVAL_{STALE,FILTER}_GATE_*` environment variables; they are intentionally unsupported.

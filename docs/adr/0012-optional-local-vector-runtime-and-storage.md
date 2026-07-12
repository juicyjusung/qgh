# Optional Local Vector Runtime and SQLite-Vec Storage

Status: Accepted, with the default-model, model-delivery, fusion, and reranker
clauses superseded by ADR-0016 and ADR-0017. The storage, fingerprint,
fail-closed fallback, citation, and BM25 capability boundaries remain
authoritative.

qgh adds vector/hybrid retrieval only as an optional local capability behind `[embedding]`. The v1 runtime boundary is `EmbeddingProvider`; new fastembed-capable configs select the separately installed `qwen3-embedding-0.6b` preset from ADR-0016. Its pinned runtime contract uses last-token pooling, a 384-dimensional output, an explicit query instruction, and a device-specific fingerprint. Historical Snowflake prepared-ONNX configs remain supported and are never silently migrated.

Vector storage uses `sqlite-vec` stable `vec0` tables behind the optional `vector-search` build capability. `Store::open()` opens and migrates only the base SQLite store. Embedding-enabled commands explicitly enable vector capability, which registers sqlite-vec on that connection after the base migration and before the additive, idempotent vector-schema migration. Existing SQLite stores must not require drop/resync, and embedding tables are created only on that explicit vector path. ANN alpha paths are out of scope until a later ADR changes that decision.

Search indexes chunks but still returns source candidates. Chunk hits are deduped to source-level results, the best chunk represents each source, and citations continue to use the existing `query -> get -> cite` source contract rather than chunk-level citation. Production hybrid ranking uses ADR-0017 `lexical_guard_v1`: preserve the BM25 top five, then apply weighted RRF (`k=60`, lexical `2`, dense `1`, dense window `80`) below that protected head. Repo, label, state, author, issue, and other hard filters are applied before each retriever creates candidates and must not be relaxed by fusion or rerank.

Embedding failures are never fatal to the required retrieval path. Runtime init failure, missing model files, fingerprint mismatch, corrupt vector state, and partial coverage produce structured warnings or errors as appropriate, then fall back to BM25 where a source-safe result can still be returned. `status` may expose local embedding coverage and fingerprint state, but it must not load models or probe the network.

Hosted or HTTP embedding providers, normalized weighted fusion, and chunk-level citations are reserved follow-up capabilities. The pinned Qwen reranker is available only as a separately installed, per-query opt-in stage over at most ten retrieved candidates; it remains off by default and cannot add a source. MCP v1 stays read-only and does not expose sync, embed, write, delete, update, or provider-management tools.

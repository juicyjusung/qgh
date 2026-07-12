# Qwen Experimental Local Adapter and Optional Reranking

## Status

Accepted as an experimental, opt-in local search-quality path. This is not a
light or quality preset promotion. BM25 remains the complete production
default.

## Context

The multilingual evaluation found a useful gap between the required BM25 path
and semantic retrieval, especially when a Korean query seeks an English
source. The follow-up [Qwen screening](../search-quality-qwen-screening.md)
identified Qwen 0.6B embedding as a promising BM25 complement and Qwen 0.6B
reranking as a strong quality ceiling. The same evidence also showed why
neither model should be bundled or enabled by default: artifact and runtime
costs are material, and reranking can be too slow as an unconditional
interactive stage. The later
[native production-adapter regression](../search-quality-qwen-production-adapter-eval.md)
validated the implemented path but did not qualify it for preset promotion.

qgh must preserve its local-first privacy boundary, strict model identity,
read-only MCP surface, exact-locator behavior, and a complete model-free
`sync -> query -> get -> cite -> status` workflow.

## Decision

### Required path and acquisition boundary

- BM25-only remains complete and is the default when `[embedding]` is absent.
- Qwen embedding and reranking are explicit opt-in capabilities. Their weights
  are not included in the qgh binary, Homebrew artifact, or release bundle.
- For the two Qwen presets in this ADR, `qgh model install` is the only
  model-network path. It downloads pinned Qwen files without sending repository
  content, source metadata, chunks, embeddings, or queries. No other CLI or MCP
  path downloads these Qwen snapshots.
- This Qwen boundary does not change the legacy prepared-ONNX preset: its
  existing explicit `qgh embed --force` acquisition behavior remains supported.
- Model management remains CLI-only. MCP v1 continues to expose only the
  read-only `query`, `get`, and `status` tools.
- Hosted providers and Python subprocess/runtime adapters are not supported.
  Production inference is local and native to the qgh process.

### Embedding preset

The fixed experimental preset ID is `qwen3-embedding-0.6b`, backed by
`Qwen/Qwen3-Embedding-0.6B` at revision
`97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`. Its manifest fixes the tokenizer
and query instruction contract, last-token pooling, L2 normalization, and a
384-dimensional Matryoshka output. These values are part of the embedding
fingerprint; users cannot override them piecemeal.

Installation stages files outside the active snapshot, verifies the pinned
content/hash manifest, and publishes the complete snapshot atomically under
the qgh XDG cache. A missing file, unexpected file, checksum mismatch, or
symlink/path escape prevents snapshot publication. Runtime incompatibility
prevents the installed snapshot from being used and keeps hybrid retrieval or
reranking disabled.
Re-running the same install for an invalid snapshot quarantines it and
publishes a replacement only after the complete replacement passes
verification; a failed repair never publishes a partial snapshot.

Automatic device mode resolves to Apple Metal F16 on supported Apple Silicon
and to CPU F32 elsewhere. The resolved runtime profile is part of the
embedding-generation fingerprint. qgh does not silently move an active
generation across devices or precision profiles; an unavailable or mismatched
runtime keeps hybrid retrieval off until a compatible generation is
published. Device selection must not permit non-finite or dimensionally
invalid vectors to be published.

For this Qwen runtime, GPU acceleration means Apple Metal only; CUDA/NVIDIA
backends are not supported.

### Optional reranking

The only v1 reranker preset is `qwen3-reranker-0.6b`, backed by
`Qwen/Qwen3-Reranker-0.6B` at revision
`e61197ed45024b0ed8a2d74b80b4d909f1255473`. Reranking runs only when the user
explicitly requests it with CLI `--rerank` or MCP `"rerank": true` and the
configured local snapshot is ready.

The stage reranks a fixed maximum of the first 10 retrieval candidates. Each
query/candidate pair uses the preset's deterministic maximum 384-token view.
The depth and token view are not configurable. The stage runs after BM25 or
hybrid RRF retrieval and:

- never adds a candidate;
- never relaxes repo, label, state, author, issue, or other hard filters;
- never changes source identity, `get_args`, citation, or staleness rules; and
- bypasses issue-number and URL exact-locator results.

Reranking is atomic with respect to order. If configuration or model files are
missing, the snapshot is corrupt, device/runtime initialization fails, any
score is non-finite, or output is partial or malformed, qgh discards the
entire rerank attempt. It returns the original retrieval order together with a
typed warning and a non-applied rerank status. It never exposes a partially
reranked list.

Automatic reranker mode resolves only to Apple Metal F32. It does not silently
fall back to CPU. Explicit CPU mode is an experimental slow path and emits a
typed warning; it does not become an always-on or release-readiness
dependency. An unavailable required runtime makes reranking non-applied and
preserves the original order.

### Configuration surface

The preset fixes rerank depth, embedding dimension, pooling, normalization,
task instructions, and model revisions. qgh does not add user-controlled
depth, fusion-weight, rerank-weight, score-threshold, or metadata-boost knobs.
Unknown config keys and unsupported model/device values remain strict errors.

Model snapshots live in the single-user qgh XDG cache and are treated as
sensitive, integrity-critical local data. Their manifests record immutable model
identity and every required artifact's relative path, byte size, and SHA-256.
Source content and query text are never written into model manifests or model
installation logs.

## Consequences

- A release remains useful without a model download, vector feature, GPU, or
  reranker.
- Users who choose Qwen accept a separate download, local cache footprint, and
  initial embedding publication step.
- Metal can accelerate supported Apple systems, while the explicitly selected
  CPU embedding path keeps hybrid retrieval portable; neither backend is a
  dependency of BM25.
- Reranking can improve ordering only inside the retrieved top-10 pool. It
  cannot recover a source that BM25/vector retrieval did not produce.
- The explicit flag and all-or-original fallback make reranker latency and
  failure visible without weakening retrieval correctness.
- Qwen embedding and reranking remain experimental opt-ins; neither changes
  BM25 release readiness or establishes hybrid quality qualification.

This decision extends ADR-0012's optional local vector design. It replaces
implicit model acquisition and the earlier rerank deferral only for these
strict opt-in local presets; ADR-0012's BM25, filter, source-level result, and
read-only MCP guardrails remain unchanged.

# Qwen as the Default Semantic Preset for New Fastembed Configs

## Status

Accepted. This decision supersedes ADR-0015 only where it describes Qwen
embedding as a manually selected config preset that is never enabled by
default. ADR-0015 remains authoritative for the pinned model/runtime contract,
local-only acquisition boundary, evaluation evidence, unresolved quality and
resource risks, and optional reranker behavior.

## Context

The native Qwen production adapter materially improved multilingual retrieval
over the BM25 baseline when used through the BM25-protected
`lexical_guard_v1` policy. On the reproducible 80-query public fixture it
preserved every observed BM25 hit at ranks 5 and 10 and rescued three BM25
misses at rank 10. Its Metal runtime is practical for local indexing and
interactive hybrid retrieval. The product decision is therefore to use
`qwen3-embedding-0.6b` for every newly created fastembed-capable config, while
retaining BM25 as a complete model-free capability.

This is a default-selection decision, not a claim that every Qwen promotion
gate has passed. The existing evidence still records an English-to-Korean
Recall@5 miss, the need for a fresh blind qualification, an incomplete large
resource protocol, and weak negative-query abstention. Those risks remain
visible and must not be rewritten as successful evaluation results.

The selection also must not create hidden network access, mutate existing
profiles, make model code part of BM25-only builds, or turn the experimental
reranker into an unconditional query stage.

## Decision

When `qgh init` creates a new global config in a binary compiled with the
`fastembed-provider` feature, it writes this top-level semantic selection:

```toml
[embedding]
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
```

The following boundaries are normative:

- `[embedding]` absence remains the BM25 capability seam. A build without
  `fastembed-provider` creates a model-free config without this table.
- Bootstrap defaults apply only when the global config file does not exist.
  Adding a repo or profile to an existing config preserves that config's exact
  embedding choice, including no embedding, Arctic, or a custom manifest.
- qgh never silently migrates an existing config to Qwen.
- Qwen weights are not included in the binary, Homebrew package, or release
  archive. `qgh model install qwen3-embedding-0.6b` remains the only Qwen model
  download path, and `init`, `sync`, `query`, MCP, and model initialization do
  not auto-download them.
- A configured but uninstalled, corrupt, stale, or incompatible Qwen snapshot
  produces the existing typed diagnostic and preserves the safe BM25 path.
  Hybrid retrieval starts only after a complete compatible generation is
  atomically published.
- Normal foreground sync reuses vectors only from a fully validated compatible
  generation, infers only missing chunks in bounded batches, persists each
  batch before continuing, and resumes validated staged work after interruption.
  A no-change sync performs zero inference and does not initialize or mmap the
  inference runtime. A new CLI process still hashes the complete installed
  snapshot, including the 1.2 GB weights, before trusting it.
- Qwen inputs are explicitly fitted to the 1,024-token runtime window before
  inference. Metadata context at the beginning is retained, authoritative
  bodies/snippets are unchanged, and the input-adapter revision is part of the
  generation fingerprint.
- Hybrid retrieval uses the fixed `lexical_guard_v1` policy from ADR-0017:
  preserve the BM25 top five, then apply weighted RRF (`k=60`, lexical `2`,
  dense `1`, dense window `80`) to the remaining candidate pool. Production
  and evaluation share this implementation, and users cannot configure its
  weights, window, or protected head.
- Content-free progress belongs on stderr for human foreground sync. qgh does
  not add a background embedding daemon, TUI dependency, or MCP sync/write tool.
- New configs do not contain `[reranker]`. Reranking remains separately
  configured, per-query opt-in, bounded to the existing candidate depth, and
  all-or-original on failure.
- Hosted embedding and reranking remain unsupported defaults; repository
  content, derived chunks, embeddings, and queries stay local.

## Consequences

- New users of fastembed-capable release binaries receive one consistent
  semantic preset instead of having to choose among historical candidates.
- They must still explicitly install the pinned model snapshot before hybrid
  search can run, so the first download remains visible and user-controlled.
- Existing BM25-only and non-Qwen users do not experience config churn or an
  implicit model download.
- Initial indexing cost remains visible, while repeated and interrupted syncs
  avoid full-corpus recomputation. Apple Silicon `auto` uses Metal F16; other
  supported systems use CPU F32, with the resolved runtime in the fingerprint.
- Cross-process snapshot verification continues to hash the complete model.
  A persistent `(path, size, mtime)` stamp is not an integrity proof and is not
  adopted. A future zero-read no-change path must first prove a reusable
  generation against the pinned contract, source/context inventory, vector
  dimensions and checksums, sqlite-vec mappings, and publication epoch; any
  missing or changed chunk must still enter the fully verified runtime path.
- No-default-feature binaries remain model-free and keep the complete
  `sync -> query -> get -> cite -> status` workflow.
- Qwen runtime, lifecycle, cross-language quality, resource, and abstention
  risks remain tracked even though the default semantic selection is fixed.
- The reranker remains optional and off by default because its latency and
  candidate-only role have not changed. When explicitly requested it may
  reorder the protected retrieval head; it still cannot introduce a source.

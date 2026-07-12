# Local Qwen models

Qwen3-Embedding-0.6B is qgh's default semantic preset when a
`fastembed-provider` binary creates a new config. BM25 remains a complete
model-free path whenever `[embedding]` is absent, including no-default-feature
builds and existing BM25-only configs. Existing configs are never silently
rewritten. Model weights are not bundled or automatically downloaded, and the
Qwen reranker remains separately configured and off by default.

The [Qwen screening](search-quality-qwen-screening.md) found that Qwen
embedding is a promising multilingual BM25-rescue path and that reranking can
substantially improve ordering. It also found enough model and runtime cost to
keep model installation explicit and reranking off unless individually
requested.
The later [native production-adapter regression](search-quality-qwen-production-adapter-eval.md)
validated the implemented embedding path. The product now fixes Qwen as the
default semantic selection while preserving the report's unresolved quality
and resource risks rather than treating them as passed promotion gates.
The reranker figures in the screening document are not a production top-10
qrels result; the implemented stage remains experimental and individually
requested.

## Choose the smallest path that fits

| Path | Local model needed | What changes |
| --- | --- | --- |
| BM25 | None | Complete lexical retrieval when `[embedding]` is absent; full `query -> get -> cite` workflow |
| Hybrid | Qwen embedding | Default semantic selection for a new fastembed config; preserves the BM25 top five and adds semantic candidates below that lexical head after explicit model installation and publication |
| Hybrid or BM25 with reranking | Qwen reranker, plus embedding only if hybrid is desired | Reorders at most the first 10 retrieved candidates when explicitly requested |

Reranking does not replace retrieval. It cannot add a source that BM25 or
hybrid retrieval did not already find.

## Install model snapshots explicitly

For the Qwen presets documented here, model installation is CLI-only and
`qgh model install` is the only command allowed to contact their model host.
Install only the capability you plan to use:

```sh
qgh model install qwen3-embedding-0.6b
qgh model install qwen3-reranker-0.6b
```

Add `--json` when a `qgh.v1` machine-readable install result is required.

The installer downloads the pinned model revision into a staging directory,
verifies the complete artifact hash manifest, and atomically publishes it in
`${XDG_CACHE_HOME:-~/.cache}/qgh`. It does not upload repository content,
source metadata, chunks, embeddings, or queries.

Treat the qgh cache as sensitive, integrity-critical single-user data. Do not
edit model files or manifests in place. Re-running `qgh model install` for an
invalid snapshot quarantines it and atomically publishes a replacement only
after full verification.

No other CLI or MCP path downloads these Qwen snapshots. This Qwen-only
boundary does not change the legacy prepared-ONNX preset's existing explicit
embed-time acquisition behavior. A configured but uninstalled Qwen model
produces a typed diagnostic and keeps the safe retrieval path available.

## Configure Qwen embedding

A fresh `qgh init` run from a fastembed-capable binary writes this preset to
the strict qgh config at
`${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`:

```toml
[embedding]
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
```

Existing configs, including configs without `[embedding]` or with Arctic or a
custom manifest, are not migrated. Add the same table manually only when you
intend to change an existing config's semantic selection.

The preset fixes the immutable upstream revision, query instruction,
last-token pooling, L2 normalization, and 384-dimensional output. These fields
cannot be overridden individually.

After installation and configuration, normal sync chunks the active snapshot,
embeds only missing or changed chunks, validates the generation, and publishes
it with the lexical snapshot:

```sh
qgh --profile PROFILE sync
```

The foreground command reports only content-free counts and timing on stderr:
staged/reused/missing chunks, completed chunks, throughput, and ETA. A repeated
sync with no content or context changes reuses the validated vectors, performs
zero inference, and does not initialize or mmap the 1.2 GB inference runtime.
A new CLI process still reads and hashes the complete installed snapshot before
trusting it. Interrupted builds resume from validated staged batches. `qgh
embed --force` remains available for an explicitly requested full rebuild; no
background daemon or MCP write tool is required.

The full hash is intentional until qgh can prove a reusable embedding
generation from Store-owned source/context inventory and vector mappings. File
size, path, and timestamp alone are not accepted as a persistent verification
cache because they do not prove artifact contents.

Until a complete embedding generation is validated and atomically published,
queries keep using BM25. Missing, stale, corrupt, partial, or incompatible
vector state never becomes a partial hybrid result.

Hybrid ordering uses the fixed `lexical_guard_v1` policy. The first five BM25
source candidates keep their exact order; the rest of the candidate pool uses
weighted reciprocal-rank fusion with `k=60`, lexical weight `2`, dense weight
`1`, and at most 80 dense candidates. This intentionally spends semantic
capacity below the reliable lexical head: on the reproducible public qrels it
preserved all observed BM25 hits at ranks 5 and 10 and rescued three misses at
rank 10. There is no user-facing fusion weight, window, or protected-head
setting.

## Enable optional reranking

Configure the experimental local reranker:

```toml
[reranker]
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
```

Then request it for an individual query:

```sh
qgh --profile PROFILE query "why was this behavior changed?" --rerank
```

MCP uses the existing read-only `query` tool with `"rerank": true`. There is
no MCP model-install or other model-management tool.

Reranking is deliberately bounded and predictable:

- it considers at most the first 10 BM25 or protected-hybrid candidates;
- each query/candidate pair uses a deterministic maximum 384-token view;
- there is no user-controlled depth, score threshold, rerank weight, fusion
  weight, or metadata-boost knob;
- it never adds candidates or loosens hard filters;
- it preserves source identity, `get_args`, citation, and staleness checks;
- issue-number and full-URL exact lookups bypass reranking; and
- it runs only when explicitly requested.

If configuration or model files are missing, the snapshot is corrupt, runtime
or device initialization fails, a score is non-finite, or output is partial,
qgh discards the whole rerank attempt. The original retrieval order is returned
with a typed warning and a rerank status explaining why it was not applied.

## Device behavior

Embedding `device = "auto"` resolves to Apple Metal F16 on supported Apple
Silicon and CPU F32 elsewhere. The resolved device and precision are part of
the embedding-generation fingerprint. qgh never silently reuses one device's
generation from another runtime profile; if the compatible runtime is
unavailable, query falls back to BM25 until a compatible generation is
published.

The Metal F16 embedding adapter groups only short inputs, processes longer
inputs as singletons, and uses fused Metal attention for supported sequences.
Before inference it explicitly fits input to Qwen's 1,024-token window at a
complete token boundary. Repository and issue context at the beginning is
preserved; only trailing body text is shortened, and the stored authoritative
body and snippet remain unchanged. The input and Metal adapter revisions are
fingerprinted, so incompatible generations are rebuilt instead of reused. CPU
F32 batching and Metal F32 reranking keep their existing execution paths.

Interactive query runtime skips the multi-document batch-comparability smoke
that indexing and `doctor` still run. The pinned artifact identity, runtime
fingerprint, query-vector dimension, and active generation validation remain
fail closed. Long-lived MCP processes also reuse the already loaded runtime;
one-shot CLI queries still pay snapshot verification and model-load cost.

For reranking, `device = "auto"` resolves only to Apple Metal F32. It does not
silently fall back to CPU. `device = "cpu"` enables an experimental slow path
and emits a typed warning. If the required reranker runtime is unavailable,
reranking is not applied and the original retrieval order is preserved.

Apple Metal is the only supported GPU backend for this Qwen runtime;
CUDA/NVIDIA acceleration is not implemented. On other systems embedding
`auto` uses CPU F32, while reranker `auto` is non-applied unless the user
explicitly selects the experimental CPU slow path.

CPU keeps embedding available on non-Metal systems, but first-time embedding
and experimental CPU reranking can be noticeably slower than Metal. BM25
remains unaffected on every device. Selecting Qwen as the default semantic
preset does not make BM25 depend on the runtime or erase the documented hybrid
quality and resource risks.

## Privacy and failure boundaries

- No hosted embedding or hosted reranking provider is used.
- No Python environment or subprocess is required by the production runtime.
- Raw query text and private source content are not written to model install
  logs or model manifests.
- Model/runtime failures do not turn into empty or partly reordered search
  results when the original retrieval result remains safe.
- Every returned result must still satisfy the same hard filters and
  `query -> get -> cite` round-trip contract.

See [ADR-0015](adr/0015-qwen-local-quality-preset-and-optional-reranking.md)
for the fixed model/runtime contract and historical evaluation evidence, and
[ADR-0016](adr/0016-qwen-default-semantic-preset.md) for the default semantic
selection and compatibility boundaries.

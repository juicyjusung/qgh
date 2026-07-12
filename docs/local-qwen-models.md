# Local Qwen models

qgh can use experimental local Qwen 0.6B models to complement BM25. Both are
explicit opt-ins: BM25 remains the complete production default, neither Qwen
model is a promoted light or quality preset, and model weights are not bundled
with qgh.

The [Qwen screening](search-quality-qwen-screening.md) found that Qwen
embedding is a promising multilingual BM25-rescue path and that reranking can
substantially improve ordering. It also found enough model and runtime cost to
keep both capabilities explicit rather than always on.
The later [native production-adapter regression](search-quality-qwen-production-adapter-eval.md)
validated the implemented embedding path without making it promotion-eligible.
The reranker figures in the screening document are not a production top-10
qrels result; the implemented stage remains experimental and individually
requested.

## Choose the smallest path that fits

| Path | Local model needed | What changes |
| --- | --- | --- |
| BM25 | None | Default lexical retrieval; full `query -> get -> cite` workflow |
| Hybrid | Qwen embedding | Adds semantic candidates and combines them with BM25 using RRF |
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

## Enable Qwen embedding

Add the opt-in preset to the strict qgh config at
`${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`:

```toml
[embedding]
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
```

The preset fixes the immutable upstream revision, query instruction,
last-token pooling, L2 normalization, and 384-dimensional output. These fields
cannot be overridden individually.

After installation and configuration, publish embeddings for the current
profile:

```sh
qgh --profile PROFILE embed --force
```

Until a complete embedding generation is validated and atomically published,
queries keep using BM25. Missing, stale, corrupt, partial, or incompatible
vector state never becomes a partial hybrid result.

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

- it considers at most the first 10 BM25 or RRF candidates;
- each query/candidate pair uses a deterministic maximum 384-token view;
- there is no user-controlled depth, score threshold, rerank weight, fusion
  weight, or metadata-boost knob;
- it never adds candidates or loosens hard filters;
- it preserves source identity, `get_args`, citation, and staleness checks;
- issue-number and full-URL exact lookups bypass reranking; and
- it runs only when explicitly requested.

If configuration or model files are missing, the snapshot is corrupt, runtime
or device initialization fails, a score is non-finite, or output is partial,
qgh discards the whole rerank attempt. The original BM25/RRF order is returned
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
Its adapter revision is also fingerprinted, so upgrading from the earlier
adapter makes the old Metal generation incompatible and requires `qgh embed`
to publish a replacement before hybrid search resumes. CPU F32 batching and
Metal F32 reranking keep their existing execution paths.

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
remains unaffected on every device. Neither experimental Qwen path changes
BM25 release readiness or establishes hybrid quality qualification.

## Privacy and failure boundaries

- No hosted embedding or hosted reranking provider is used.
- No Python environment or subprocess is required by the production runtime.
- Raw query text and private source content are not written to model install
  logs or model manifests.
- Model/runtime failures do not turn into empty or partly reordered search
  results when the original BM25/RRF result remains safe.
- Every returned result must still satisfy the same hard filters and
  `query -> get -> cite` round-trip contract.

See [ADR-0015](adr/0015-qwen-local-quality-preset-and-optional-reranking.md)
for the fixed product and runtime contract.

# Qwen 0.6B embedding and reranker screening

## Decision

`Qwen/Qwen3-Embedding-0.6B` is a promising multilingual BM25-rescue candidate
within this standalone screening protocol, especially on Korean-to-English
retrieval. On the reused 80-query screening split, the MPS run raised weighted nDCG@10 from
`0.7244` to `0.8894`, rescued 13 of 16 BM25 top-5 misses, harmed zero existing
BM25 top-5 hits, and preserved the exact/identifier top-1 hard gate. Its
selected Matryoshka dimension was 384 and its selected dense RRF weight was
`1.0`.

This is screening evidence, not promotion evidence. The 80-query split had
already been opened during the preceding lightweight-model decision, qgh does
not yet have a production adapter for Qwen's last-token pooling and task
instruction contract, and a 50k-chunk backfill/publication run was not
performed. The production preset therefore remains unchanged and BM25-only
remains the complete default path.

`Qwen/Qwen3-Reranker-0.6B` establishes a high quality ceiling but is not
practical as an always-on local CLI stage under this shared-union protocol.
Reranking the union of BM25 top 20 and hybrid top 20 raised weighted nDCG@10 to
`0.9896` on MPS, but per-query pool latency was 2.81 seconds p50 and 12.91
seconds p95. CPU was 59.45 seconds p50 and 318.92 seconds p95. A future
reranker experiment would need a shallower, selective trigger and a fresh
blind split; this benchmark does not add one.

## Evaluation identity and scope

The benchmark reused the frozen public, unauthenticated GitHub snapshot from
the 2026-07-11 fresh evaluation. It did not read private repositories or use a
hosted embedding/reranking service. Raw query text and source bodies are absent
from normal event and report artifacts; events contain query IDs, query hashes,
classes, and source identities only.

| Field | Value |
| --- | --- |
| Evaluation state | `screening_only_previously_opened_heldout` |
| Evidence HEAD | `023fbda8d09bd4ab5dedbe987728ea78707b9007` (clean at both starts) |
| Corpus | 566 sources; SHA-256 `992b375ef47f31f36caef54d2798c5315d734754146528cd60a78ce5e7153ef0` |
| Dev qrels | 40 queries; SHA-256 `7e4daa6376fff4f013b088596c4b98ce99aa52340cc7df76046f82ed1d555494` |
| Screening qrels | 80 queries; SHA-256 `1a639489b0d19f5f31a3f7065335ab34815af3b3a58d1de1b04047bc497c7c2c` |
| Embedding | `Qwen/Qwen3-Embedding-0.6B`; revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3` |
| Reranker | `Qwen/Qwen3-Reranker-0.6B`; revision `e61197ed45024b0ed8a2d74b80b4d909f1255473` |
| Runtime | Python 3.12; PyTorch 2.13.0; Transformers 5.13.1; Sentence Transformers 5.6.0 |
| Hardware | Apple M4 Pro, MPS and CPU runs |

The model revisions are immutable inputs. Both models are Apache-2.0 according
to their official model cards. The benchmark follows the model-specific query
instruction pattern described by the [Qwen3 Embedding repository](https://github.com/QwenLM/Qwen3-Embedding),
uses last-token pooling, and L2-normalizes embeddings. The embedding grid
compared Matryoshka dimensions 384 and 1024 with dense RRF weights `0.25`,
`0.5`, `0.75`, and `1.0`. The dev split selected 384 and `1.0` before the
screening split was scored. Exact/identifier queries retained the BM25 route.

This harness retrieves only the top 20 dense candidates before RRF. The
canonical qgh live protocol uses a candidate window of 80 before returning 20
results. Consequently, the Qwen numbers are not directly comparable with the
canonical Tiny/Granite model-selection table and cannot establish a new model
winner. They are a trigger for a future canonical-window qualification run.

Sources were tokenized into 900-token chunks with 135-token overlap. A source
score is the maximum of its chunk scores. Embedding used batch 8; query latency
used batch 1. The reranker used batch 4 and scored the union of each path's
top-20 candidates once for each query, both over BM25 and over the selected
hybrid retrieval. Both stages capped model input at 1,024 tokens. The task
instruction was specific to retrieving relevant GitHub issue or comment
passages.

## Quality results

The quality table uses the MPS run and applies only to this window-20 screening
protocol. CPU produced the same hard-gate and class-level Recall@5 results;
small floating-point ordering differences are described below.

| Retrieval path | Weighted nDCG@10 | Weighted MRR@10 | BM25 rescue@5 / harm@5 | Exact top-1 | Comment-gold Recall@5 |
| --- | ---: | ---: | ---: | ---: | ---: |
| BM25 | 0.7244 | 0.7024 | n/a | 1.00 | 0.80 |
| Qwen dense diagnostic | 0.9386 | 0.9250 | 16 / 0 | 1.00 | 1.00 |
| Qwen embedding hybrid | 0.8894 | 0.8605 | 13 / 0 | 1.00 | 0.80 |
| BM25 + Qwen reranker | 0.8313 | 0.8325 | 4 / 0 | 1.00 | 0.80 |
| Qwen hybrid + Qwen reranker | **0.9896** | **0.9875** | **16 / 0** | 1.00 | **1.00** |

Recall@5 by query class:

| Class | BM25 | Qwen embedding hybrid | BM25 + reranker | Qwen hybrid + reranker |
| --- | ---: | ---: | ---: | ---: |
| English semantic | 0.95 | 1.00 | 1.00 | 1.00 |
| Korean semantic | 0.87 | 1.00 | 1.00 | 1.00 |
| Korean query -> English source | 0.20 | **1.00** | 0.30 | **1.00** |
| English query -> Korean source | 0.50 | 0.70 | 0.50 | **1.00** |
| Exact/identifier | 1.00 | 1.00 | 1.00 | 1.00 |
| Comment-only | 0.90 | 0.90 | 0.90 | 1.00 |
| Long/context-dependent | 1.00 | 1.00 | 1.00 | 1.00 |

All measured paths recorded zero hard-filter violations. The Qwen embedding hybrid
also rescued all 13 BM25 misses at top 10 and harmed zero BM25 top-10 hits.
BM25-only and Qwen hybrid preserved 59 existing BM25 top-5 hits; hybrid plus
reranking rescued all 16 misses and preserved all 59 hits.

The dense-only row is a diagnostic derived from the committed redacted ranking
events, not a proposed user-facing vector-only mode. It achieved Recall@5
`1.00` in every positive class and outperformed the selected RRF hybrid. This
shows that the current equal-scale RRF fusion leaves three top-5 rescues on the
table even at the grid's maximum dense weight. A fresh qualification should
therefore tune a BM25-preserving fusion/trigger contract rather than assume the
current `1.0` endpoint is optimal.

The five negative queries are excluded from the weighted aggregate because
they have no relevant source. Their non-empty top-result rate was `0.80` for
BM25 and BM25 plus reranking, and `1.00` for dense, hybrid, and hybrid plus
reranking. Qwen therefore does not provide abstention and slightly worsens this
slice. A production adapter needs an explicit score/empty-result policy; the
high positive-query metrics must not be interpreted as negative-query safety.

Within this screen, the result supports investigating Qwen for complementing
BM25 rather than replacing it. The embedding addresses the largest observed
BM25 gap, Korean queries seeking English sources, without weakening exact
lookup. Reranking BM25 alone cannot recover sources absent from its candidate
pool, which is why its Korean-to-English gain is much smaller.

KoEn E5 Tiny remains the canonical lightweight candidate. Qwen's 1.207 GB
snapshot is about 7.9 times Tiny's 152.7 MB snapshot. Quality and runtime-memory
rankings between them are not valid from these runs: Tiny used candidate window
80 and qgh's ORT production path, while Qwen used window 20 and
PyTorch/Sentence Transformers.

## Resource results

Each model snapshot is about 1.207 GB; enabling both requires about 2.415 GB of
model files before runtime/cache overhead. Resource evidence covers the
566-source quality corpus and 639 generated chunks. It does not satisfy the
release 50k backfill gate.

| Stage | Device | Load | Throughput | Query/pool p50 | Query/pool p95 | Max sampled RSS | Max sampled MPS driver |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Embedding | MPS | 998 ms | 7.96 chunks/s | 30.7 ms | 49.4 ms | 1.083 GB | 4.509 GB |
| Embedding | CPU | 265 ms | 0.374 chunks/s | 203.7 ms | 317.8 ms | 1.881 GB | n/a |
| Reranker, top-20 shared union | MPS | 1,060 ms | 5.92 pairs/s | 2.81 s | 12.91 s | 1.860 GB | 4.581 GB |
| Reranker, top-20 shared union | CPU | 247 ms | 0.264 pairs/s | 59.45 s | 318.92 s | 2.397 GB | n/a |

MPS improved embedding indexing throughput by about 21.3 times and warm-query
p50 by about 6.6 times. It improved reranker pair throughput by about 22.4
times, but the reranker remained too slow for an unconditional interactive
stage. The CPU run took 11,609.8 seconds overall, including 1,707.7 seconds for
embedding and 9,872.9 seconds for reranking. The MPS run took 530.2 seconds.

The evidence-run harness sampled memory after major operations rather than
using a continuous monitor or OS high-water counter. Its legacy
`peak_rss_bytes` and `quality_rss_gate_passed` fields can miss transient peaks;
the values above are therefore maximum samples and do not pass the 2.5 GiB
resource gate. MPS driver samples reached about 4.58 GB, which is material on
smaller unified-memory Macs. The corrected harness now uses the OS process
high-water RSS for future reruns, but these published artifact hashes remain
the original screening evidence and are not retroactively upgraded.

The shared reranker union exceeded 20 sources on 5 of 80 screening queries and
reached a maximum of 23. Its latency is not the cost of reranking one fixed
20-source list.

A linear extrapolation of measured embedding throughput gives roughly 105
minutes for 50k chunks on MPS and 37 hours on CPU. This is an estimate, not a
completed 50k result, and excludes tokenization, vector persistence,
publication, checkpointing, and integrity verification. It cannot be used to
pass the resource gate.

## Device parity

The BM25 rankings were identical on all 80 queries. Qwen embedding and reranker
top-1 rankings were identical on all 80 queries except hybrid plus reranking,
which differed on one comment-only query: MPS ranked the relevant comment
first, while CPU ranked its parent issue first. Top-5 membership matched on
79/80 hybrid queries, 77/80 dense queries, 73/80 BM25-reranked queries, and
67/80 hybrid-reranked queries. All class Recall@5 values, exact top-1, and
filter gates were identical across devices.

The small ordering drift is consistent with different floating-point kernels,
but that is an inference rather than a proved root cause. Production adoption
would require a declared tolerance and deterministic tie-break contract rather
than assuming byte-identical rankings across CPU and MPS.

## Production implications

- Keep BM25 as the default and complete path.
- Keep the existing production embedding preset unchanged. Qwen 0.6B is the
  next standalone signal worth canonical qualification, not an automatically
  promoted preset or a proven replacement for Tiny.
- If Qwen is implemented, prefer MPS on supported Apple hardware and retain an
  explicit CPU fallback warning. CPU embedding is usable for occasional
  queries but too slow for a large initial backfill under the measured setup.
- Do not ship the 0.6B reranker as an always-on shared-union stage. A later
  study may test one fixed pool at depth 5 or 10 behind an
  ambiguity/low-confidence trigger, but only after embedding recall is
  available and a fresh blind split exists.
- qgh's current prepared-model contract exposes only `cls` and `mean` pooling,
  while Qwen requires last-token pooling and task instructions. A dedicated,
  fail-closed runtime/manifest contract is required before production use.
- Before promotion, run a new untouched multilingual qrels split, the complete
  50k backfill/publication/integrity gate, device-parity tolerances, fallback
  behavior, candidate window 80, and query-to-get round-trip using the actual
  qgh runtime adapter.
- Bind a model artifact tree/content manifest, not only a Hugging Face revision
  and logical snapshot byte count.

The benchmark deliberately does not implement a reranker, new MCP tool, hosted
service, ANN index, sparse retriever, or production default change.

## Verification

- `python -m unittest tests/support/test_benchmark_qwen_retrieval.py`: 8 passed
- `ruff check` and `ruff format --check`: passed
- `cargo fmt --check`: passed
- `cargo clippy --all-targets --all-features`: 0 errors, 40 pre-existing
  feature-combination dead-code warnings
- `cargo test --no-default-features`: 375 passed
- `cargo test --all-features`: 535 passed, 6 ignored
- `cargo test --all-features --test search_quality_eval`: 1 passed
- Artifact redaction audit: six files scanned; zero raw query/title/body,
  absolute user path, authorization-header, or GitHub-token matches
- `git diff --check`: passed

## Machine evidence and reproduction

Machine artifacts remain ignored under `target/qgh-eval/qwen-benchmark/`.
The listed evidence is report schema `qgh.qwen_screening_benchmark.v1`. The
post-review harness emits v2 because its RSS field now uses the OS process
high-water counter and its redaction/Git publication checks are stronger.

| Artifact | SHA-256 |
| --- | --- |
| MPS report | `eaf1beef7feab23cc7dd1230a296f4985bf904eb7bf9113090ba0f9773e527ce` |
| CPU report | `4e7b7250c8f9e81108801d0a4ba3ff9a6ef6073478a3a60534b02bb01b5825ac` |
| MPS events | `e0b4a4c49d54b4e2d16da93c3b27c9a1996aa8c5685e41eb7db3c6cad53b68a6` |
| CPU events | `92b5289cf55ba4a3233bb750b405023cc55efe3194f737e5126eb37573558022` |

Prepare the isolated environment from the pinned requirement set and use an
already verified local model cache:

```sh
python3 -m venv target/qgh-eval/qwen-benchmark/.venv
target/qgh-eval/qwen-benchmark/.venv/bin/pip install \
  -r tests/support/qwen_benchmark_requirements.txt

HF_HOME=target/qgh-eval/qwen-benchmark/hf-cache \
HF_HUB_OFFLINE=1 \
PYTHONHASHSEED=0 \
target/qgh-eval/qwen-benchmark/.venv/bin/python \
  tests/support/benchmark_qwen_retrieval.py \
  --fixture-root "$QGH_FRESH_FIXTURE_ROOT" \
  --bm25-root "$QGH_FRESH_BM25_ROOT" \
  --output-root target/qgh-eval/qwen-benchmark/results \
  --cache-dir target/qgh-eval/qwen-benchmark/hf-cache \
  --device mps
```

Use `--device cpu` for the CPU comparison. The harness verifies the hashes
declared by fixture provenance, BM25 query IDs and query hashes, pinned model
revisions, artifact redaction, and that one initially clean Git identity stays
unchanged throughout inference. It does not bind a canonical expected HEAD,
strict event schema, externally anchored fixture hash set, or model-content
manifest; production qualification must add those contracts.

# fastembed 5.17.2 vendoring provenance

- Package: `fastembed` `5.17.2`
- Registry: `crates.io`
- Published archive SHA-256: `545e4fb17fc48768ff36c2a3854aa5b0b809d0ed595ab5530fa8ac94f31bd0ea`
- Upstream repository: <https://github.com/Anush008/fastembed-rs>
- Published VCS revision: `d0e8e9e958215ad4106f58ed4ac1b9220e4a1296`
- License: Apache-2.0; the published `LICENSE` file is retained at
  `vendor/fastembed-5.17.2/LICENSE`.

The directory was extracted from the checksum-verified crates.io archive. qgh
then modifies only `src/models/qwen3.rs`: Qwen3 F16 embedding attention uses
Candle's fused full Metal SDPA for sequences longer than eight tokens. CPU,
Metal F32, and short-sequence execution retain the published generic path.
The local path is selected explicitly through the root `[patch.crates-io]`
entry.

The vendored package retains all public files from the published archive,
including fastembed's bundled sparse-model asset at
`src/sparse_text_embedding/weights/sparse_linear.safetensors`. qgh adds no
Qwen weights, tokens, private repository content, embeddings, queries, or
other runtime data.

## Release boundary

qgh is a `publish = false` binary project released from a tracked Git checkout
through cargo-dist. Cargo
[always excludes nested packages](https://doc.rust-lang.org/cargo/reference/manifest.html#the-exclude-and-include-fields)
from a parent `cargo package` archive, even when `include` is set, so a qgh
source `.crate` is not a supported release artifact for this path dependency.
Release checks instead verify that Cargo resolves this directory, the
dist-profile binary
builds from the checkout, and `git archive` contains the vendored manifest,
license, and patched source.

## Maintenance boundary

Do not remove the `t > 8` guard without an explicit left-padding parity test:
Candle 0.10.2's short vector-SDPA path does not consume the supplied mask. The
full path uses an explicit mask with `do_causal = false`; current Q/K/V and mask
views have zero storage offsets, and qgh serializes the model behind a mutex.
Those constraints keep this use outside the later Candle fixes for
[storage offsets](https://github.com/huggingface/candle/commit/0c58953685e16278c73b48607502c44678110272)
and
[explicit-mask causal handling](https://github.com/huggingface/candle/commit/e020e8a42766c497c795c29df25c2ba2ef0e1480).

A Candle or fastembed upgrade must rerun long, mixed-left-padding, query, and
ranking parity gates before dropping this patch. Candle 0.11 compiled without
source changes, but its default Metal command-buffer schedule erased this
throughput gain in the measured workload; scheduler behavior therefore remains
part of that future upgrade decision. Metal F32 reranking and CPU execution do
not use the patched branch.

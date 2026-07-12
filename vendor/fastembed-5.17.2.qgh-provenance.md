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

# qgh

qgh is a local-first retrieval tool for GitHub Issues and issue comments. It keeps the core workflow explicit: `query -> get -> cite`.

## Install

```sh
brew install juicyjusung/tap/qgh
```

The Homebrew formula installs a self-contained `qgh` binary on your PATH.
Release artifacts are built by `cargo-dist` from `vX.Y.Z` tags and served from
GitHub Releases; the tap formula is published to `juicyjusung/homebrew-tap`.

## First Use

From a git repository with GitHub authentication available:

```sh
qgh init -y
qgh sync
qgh query "search terms"
```

Use `qgh get` with a returned `source_id` before citing a result. Search snippets are source candidates, not citation evidence.

## Optional Local Qwen Search

BM25 remains the complete default path. An experimental local Qwen embedding
adapter and per-query reranker are available as explicit, separately downloaded
options; model weights are not bundled with qgh. See
[Local Qwen models](docs/local-qwen-models.md) for the pinned install,
configuration, device, privacy, and fallback contracts.

## Verify

```sh
qgh --version
qgh help
qgh doctor
```

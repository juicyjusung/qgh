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
qgh model install qwen3-embedding-0.6b
qgh sync
qgh query "search terms"
```

Use `qgh get` with a returned `source_id` before citing a result. Search snippets are source candidates, not citation evidence.

Fastembed-capable release binaries select local Qwen embedding when they create
a new config. The model is not bundled or downloaded by `init`, `sync`, or
`query`; the explicit install command above is required before hybrid search
can be published. Builds without the provider stay model-free, and existing
configs are never silently rewritten. Removing or omitting `[embedding]` keeps
the complete BM25-only workflow.

## Local Qwen Search

Qwen3-Embedding-0.6B is qgh's default semantic preset for newly created
fastembed configs. BM25 remains the complete path whenever `[embedding]` is
absent. The separately configured per-query reranker remains optional and off
by default; neither model's weights are bundled with qgh. See
[Local Qwen models](docs/local-qwen-models.md) for the pinned install,
configuration, device, privacy, and fallback contracts.

## Verify

```sh
qgh --version
qgh help
qgh doctor
```

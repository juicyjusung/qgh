# Privacy and Local Data

qgh treats local retrieval artifacts as Sensitive Derivative Data because they can contain or reflect private GitHub Issue and issue comment content.

Local paths reported by `status` include:

- SQLite database: authoritative source metadata and bodies.
- Tantivy index: derived BM25 search index.
- logs: local diagnostics, which must not include tokens or private source bodies.
- cache: local cache data for the active profile plus explicitly installed,
  pinned local model snapshots.

SQLite files are hardened as single-user files where the platform supports it. Profile data, Tantivy index directories, logs, and cache directories are hardened as single-user directories where the platform supports it.

Ordinary retrieval network egress is limited to the configured GitHub host for
sync and explicit `doctor` probes. The separate CLI-only `qgh model install` command may
contact Hugging Face to fetch one pinned public Qwen snapshot; it sends no
repository content, source metadata, chunks, embeddings, or queries. Qwen
`sync`, `embed`, `query`, `get`, `status`, `doctor`, and MCP paths never acquire
a model. This Qwen boundary does not change the legacy prepared-ONNX preset's
existing explicit embed-time acquisition behavior. `get` is local-only by
default; CLI `qgh get --verify-lifecycle`
explicitly opts in to a configured GitHub host lifecycle check. MCP `get`
remains local-only/read-only and rejects lifecycle verification parameters.
Hosted inference provider paths are disabled unless a future policy adds an
explicit opt-in.

Tracked repo policy config is project policy, not personal credential config. It may define repo scope and safe default filters, but must not contain literal tokens, token source references, profile store paths, arbitrary database paths, or user-local absolute paths.

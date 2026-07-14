# Privacy and Local Data

qgh treats local retrieval artifacts as Sensitive Derivative Data because they can contain or reflect private GitHub Issue and issue comment content.

Local paths reported by `status` include:

- SQLite database: authoritative source metadata and bodies.
- Tantivy index: derived BM25 search index.
- logs: local diagnostics, which must not include tokens or private source bodies.
- cache: local cache data for the active profile plus explicitly installed,
  pinned local model snapshots.

SQLite files are hardened as single-user files where the platform supports it. Profile data, Tantivy index directories, logs, and cache directories are hardened as single-user directories where the platform supports it.

Sync/scheduling stores only the minimum coordination metadata needed for safe
automation: stable empty advisory lock files, sanitized rate-limit response
headers, a host-hash round-robin cursor with one profile id, and one strict
schedule registration. It does not put tokens, source bodies, snippets,
queries, repository names, or local database paths in those records. The
profile id and host/rate metadata can still reveal local operational context,
so schedule state, LaunchAgent/systemd artifacts, and macOS schedule logs use
single-user permissions. Manager artifacts necessarily contain the selected
profile ids, stable qgh executable path, and minimal HOME/XDG/PATH environment.
The scheduled PATH contains only the qgh and resolved `gh` directories plus
fixed platform system directories; qgh does not copy the caller's full PATH.

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
Hosted provider paths are disabled for embedding and reranking unless a future
policy adds an explicit opt-in.

`qgh schedule run` may contact only the GitHub hosts configured by the explicit
profile list. `schedule start`, `schedule status`, and `schedule stop` do not
contact GitHub. They manage one user-scoped LaunchAgent or systemd timer and
never install cron or a system-level service fallback. `schedule start`
accepts only `github_cli` token sources and writes no credential material; the
user manager must be able to read the authenticated GitHub CLI credential
store through the recorded HOME/GH_CONFIG_DIR context.

Tracked repo policy config is project policy, not personal credential config. It may define repo scope and safe default filters, but must not contain literal tokens, token source references, profile store paths, arbitrary database paths, or user-local absolute paths.

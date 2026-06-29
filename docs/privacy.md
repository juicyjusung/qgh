# Privacy and Local Data

qgh treats local retrieval artifacts as Sensitive Derivative Data because they can contain or reflect private GitHub Issue and issue comment content.

Local paths reported by `status` include:

- SQLite database: authoritative source metadata and bodies.
- Tantivy index: derived BM25 search index.
- logs: local diagnostics, which must not include tokens or private source bodies.
- cache: local cache data for the active profile.

SQLite files are hardened as single-user files where the platform supports it. Profile data, Tantivy index directories, logs, and cache directories are hardened as single-user directories where the platform supports it.

Default network egress is limited to the configured GitHub host for sync, get lifecycle checks, and explicit `doctor` probes. Hosted provider paths are disabled unless a future policy adds an explicit opt-in.

Tracked repo policy config is project policy, not personal credential config. It may define repo scope and safe default filters, but must not contain literal tokens, token source references, profile store paths, arbitrary database paths, or user-local absolute paths.

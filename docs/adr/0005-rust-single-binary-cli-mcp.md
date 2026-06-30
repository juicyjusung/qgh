# Rust Single-Binary CLI With MCP Adapter

qgh is implemented as a Rust single-binary CLI-first product with an optional MCP adapter. The primary user is often an agent repeatedly invoking local tools, so the MVP should not depend on Node/Python runtime setup, per-project virtual environments, or native extension drift.

This decision favors packaging reliability, predictable startup, and bundled local dependencies over rapid scripting convenience.

MVP crate baseline is `clap` for CLI, `serde`/`toml`/`serde_json`/`schemars` for strict config and schemas, `reqwest` with rustls for GitHub REST, `tokio` for async sync/MCP adapter work, `rusqlite` with bundled SQLite for local authoritative storage, `tantivy` for BM25 search, and the official MCP Rust SDK as the first MCP adapter implementation path.

CLI args, JSON schema snapshots, and local store/search behavior remain source of truth. If an SDK or crate lags a target contract, qgh isolates that compatibility behind an adapter instead of weakening the product contract.

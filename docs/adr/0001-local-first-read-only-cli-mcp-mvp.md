# CLI-First Local-First Read-Only MVP

qgh MVP is a single-user CLI-first local retrieval tool, not a shared server or GitHub write-back product. The core contract is CLI args, strict CLI `--json` envelopes, and local SQLite/Tantivy retrieval behavior. MCP v1 is a read-only thin adapter over that contract, exposing only `query`, `get`, and `status`.

This keeps private Issues/comments content and derived data local by default, avoids ACL and audit complexity during validation, and lets agents use either CLI `--json` or MCP without changing product semantics.

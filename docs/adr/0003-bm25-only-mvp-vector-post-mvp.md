# BM25 Required Path With Optional Vector Scope Change

qgh must keep `sync -> query -> get -> cite -> status` complete through a Tantivy BM25 path over GitHub Issues and issue comments. SQLite remains the authoritative source/lifecycle store; Tantivy remains a derived, rebuildable search index.

Issue #47 and `qgh-hybrid-search-prd.md` revise the original "vector post-MVP" deferral: local vector/hybrid retrieval is now in scope as an opt-in optional capability. This scope change does not weaken the original guardrail: when embedding is not configured, BM25-only behavior, strict hard filters, `get` round-trip, source-level citation, local-only `status`, and read-only MCP `query`/`get`/`status` must remain unchanged.

Embedding runtime, vector storage, model fingerprinting, chunk retrieval, and RRF fusion are recorded separately in ADR-0012 so future implementation work cannot make vector a hidden dependency of the required BM25 path.

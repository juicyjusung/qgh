# Tantivy BM25 MVP With Vector Post-MVP

qgh MVP must complete `sync -> query -> get -> cite -> status` through a Tantivy BM25-only path over GitHub Issues and issue comments. SQLite remains the authoritative source/lifecycle store; Tantivy is a derived, rebuildable search index.

Vector and hybrid search remain post-MVP because model install, embedding fingerprint drift, partial coverage, and hosted-provider privacy review would obscure the core retrieval correctness and citation-contract validation.

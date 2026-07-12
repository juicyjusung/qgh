# BM25-Protected Semantic Complement

Status: accepted

qgh uses local Qwen retrieval to complement BM25 rather than replace its strongest results. The fixed `lexical_guard_v1` policy preserves the first five BM25 source candidates, then orders the remaining pool with weighted reciprocal-rank fusion (`k=60`, lexical weight `2`, dense weight `1`, dense window `80`). Production and evaluation share the same content-free fusion module, and no CLI, config, or MCP weight/head/window knob is exposed.

Equal-weight RRF improved aggregate quality on the reproducible 80-query multilingual fixture, but harmed five existing BM25 hits at rank 5, four at rank 10, and reduced comment-only Recall@5 from `1.0` to `0.0`. The selected guard preserved every observed BM25 hit at ranks 5 and 10 while rescuing three BM25 misses at rank 10. It deliberately provides no semantic rescue inside the protected top five; an explicitly requested local reranker may reorder the bounded retrieved head, but it cannot retrieve a missing source.

`rrf_rank_score` remains the weighted fusion signal. `final_order_score` is a separate reciprocal post-policy rank signal, so consumers can distinguish fusion evidence from the actual retrieval order without treating either value as confidence. The selection split had already been opened, so a fresh blind multilingual qualification remains required before claiming general model-quality promotion; this ADR records the safer product ordering policy selected for the user-approved Qwen default.

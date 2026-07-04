# Vector Search Consistency

This note covers the internal vector-only search path used before hybrid search
is exposed as a user-facing mode.

## Invariants

- SQLite remains the authoritative store for source identity, canonical URL,
  source versions, chunks, embedding fingerprints, and vector rows.
- Tantivy remains a derived BM25 generation. Publishing a new Tantivy
  generation does not define vector truth.
- `chunk_embedding_vectors.rowid` matches `chunks.id`. Vector rows are usable
  only when the chunk belongs to the active source version and has a
  `chunk_embeddings` row for the active `embedding_fingerprints` row.
- The active embedding fingerprint records provider, model id/revision,
  dimension, pooling, query prefix, chunker version, and source schema version.
  A vector query may use only rows whose active fingerprint matches the query
  vector dimension.
- Vector-only candidate generation applies hard filters before sqlite-vec KNN by
  constraining `vec0` with `rowid IN (filtered active chunk ids)`. Repo, state,
  label, author, issue, and source-type filters must not be repaired later by a
  post-filter.
- Vector chunk hits are deduplicated to source candidates before presentation.
  Every returned source candidate must round-trip through `get` and carry stable
  source identity, canonical URL, and source version metadata.

## Swap Model

Tantivy swaps by building `shadow-<generation>`, renaming it to
`generation-<generation>`, then marking that generation active in SQLite.
Vector storage does not swap by directory. Instead, a vector refresh writes
chunk rows and embedding rows in SQLite, recreating the sqlite-vec table when the
active vector dimension changes or the active fingerprint changes.

Search consistency is therefore checked at read time:

1. Read active chunk ids from SQLite using active source version joins.
2. Join those chunks to the active embedding fingerprint and matching
   `chunk_embeddings` rows.
3. Pass the filtered chunk ids into sqlite-vec with `rowid IN (...)`.
4. Return only source ids that can be fetched from the authoritative store.

This keeps Tantivy generation swaps and vector rows consistent without a
denylist ADR change and without destructive storage migrations.

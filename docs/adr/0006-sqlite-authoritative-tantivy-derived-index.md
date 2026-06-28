# SQLite Authoritative Store With Tantivy Derived Index

qgh stores source identity, source versions, aliases, tombstones, sync runs, and profile metadata in bundled SQLite as the authoritative local store. qgh builds a Tantivy index from committed SQLite rows for BM25 retrieval.

Sync writes SQLite first, records index work, and only exposes a Tantivy generation after shadow build and atomic publish. Query results must still round-trip through SQLite-backed `get`; a Tantivy hit that cannot be resolved by `get` is not a successful result.

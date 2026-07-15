# SQLite Authoritative Store With Tantivy Derived Index

qgh stores source identity, source versions, aliases, tombstones, sync runs, and profile metadata in bundled SQLite as the authoritative local store. qgh builds a Tantivy index from committed SQLite rows for BM25 retrieval.

Sync writes SQLite first, records index work, and only exposes a Tantivy generation after shadow build and atomic publish. Query results must still round-trip through SQLite-backed `get`; a Tantivy hit that cannot be resolved by `get` is not a successful result.

A Tantivy generation is publishable only after its committed files and seal are
complete, its shadow directory has been renamed without replacement, and the
generation directory, `index_root`, and profile directory have crossed the
supported filesystem durability barriers. SQLite publication activation happens
after those barriers. If any barrier fails, qgh returns a structured publication
error and keeps the previous SQLite publication pointer unchanged. qgh uses the
standard macOS/Linux filesystem synchronization contract; stronger guarantees
against device-controller or sudden-power-loss behavior remain platform-dependent.

The Profile Store marker has an independent compatibility lifecycle. Only
backward-safe additive changes may retain `qgh.db.v1`; an incompatible writer
change must bump the marker. An older binary must fail closed before migration
or operational repair when it sees a newer marker or a populated database it
cannot identify. It must never downgrade that store. The same compatibility
check applies to read-only commands, while writable migration rechecks the
marker after acquiring its SQLite write transaction.

An unsupported store is an upgrade-or-restore condition, not permission to
force a sync migration. The existing `qgh.v2` `storage.failure` code reports
`details.reason: "unsupported_schema"`; recognized `qgh.db.vN` markers may be
reported, while arbitrary marker content is redacted as `"unrecognized"`.

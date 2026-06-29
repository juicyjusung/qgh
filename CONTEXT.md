# qgh Domain

qgh is a local-first retrieval context for GitHub Issues and issue comments. The language below keeps product, sync, search, and agent-citation work aligned.

## Language

**Profile**:
A single-user retrieval configuration that fixes GitHub host, token source reference, repo allowlist, XDG profile data path, schema/profile id, SQLite store, and Tantivy index.
_Avoid_: workspace, project, implicit repo

**Repo Allowlist**:
The explicit list of GitHub repositories a profile may sync and query.
_Avoid_: org discovery, fallback repo, current-directory inference

**Repo Policy**:
A repository-owned retrieval policy that defines safe default query scope and filters for that repository without defining credentials, token source, or local storage.
_Avoid_: profile, token config, personal binding

**Repo Scope**:
The subset of a profile corpus limited to one repository's issues and issue comments.
_Avoid_: issue focus, branch task, current ticket

**Effective Scope**:
The final bounded retrieval scope for a command after combining explicit CLI inputs, environment inputs, repo policy, and profile constraints.
_Avoid_: implicit repo, guessed context

**Corpus**:
The set of source entities and source versions currently indexed for one profile.
_Avoid_: knowledge base, dataset

**Profile Store**:
The XDG data directory for one profile, containing the authoritative SQLite store and derived Tantivy index.
_Avoid_: project folder, global DB, cwd index

**Source Entity**:
A GitHub issue or issue comment tracked as a retrievable source.
_Avoid_: document, file

**Source Identity**:
The stable qgh URI for a source entity, based on GitHub `node_id` and independent from mutable URLs, titles, and issue numbers.
_Avoid_: URL id, title key

**Locator**:
A mutable way to find or display a source, such as canonical GitHub URL, issue number, title, or comment URL.
_Avoid_: identity, primary key

**Source Version**:
A specific observed version of source content with body hash, GitHub updated timestamp, and indexed timestamp.
_Avoid_: latest source, revision

**Source Candidate**:
A `query` result that may answer the user's need but is not itself citation evidence.
_Avoid_: answer, citation

**Authoritative Source**:
The content returned by `get`, including canonical URL, parent context, and source version metadata.
_Avoid_: snippet, preview

**Citation Contract**:
The required `query -> get -> cite` workflow where citations use `get` results, not snippets.
_Avoid_: search answer, snippet citation

**Tombstone**:
A lifecycle marker that a source entity is deleted, transferred, unavailable, or otherwise excluded from active search.
_Avoid_: inactive flag

**Reconciliation**:
A bounded sync pass that compares known source identities against GitHub state to detect deletes, transfers, and stale ghosts.
_Avoid_: refresh, cleanup

**Status Snapshot**:
A local-only report of profile, store, index, sync, and reconciliation state. It does not perform network or model probes.
_Avoid_: health check, live probe

**Doctor Probe**:
An explicit diagnostic run that may check config, auth, GitHub reachability, rate-limit headers, local permissions, and schema/index consistency.
_Avoid_: status, background check

**Structured Error**:
A schema-visible failure state distinct from no-result success.
_Avoid_: empty result, silent fallback

**Output Envelope**:
A versioned JSON wrapper used for CLI `--json` output and MCP structured content, separating `data`, `error`, `warnings`, and `meta`.
_Avoid_: ad hoc JSON, plain text error

**Sensitive Derivative Data**:
Local DB rows, snippets, logs, cache files, and embeddings derived from private GitHub content.
_Avoid_: cache, metadata only

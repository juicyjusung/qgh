# qgh Domain

qgh is a local-first retrieval context for GitHub Issues, issue comments, and Wiki sources. The language below keeps product, sync, search, and agent-citation work aligned.

## Language

**Profile**:
A single-user retrieval configuration that fixes GitHub host, token source reference, repo allowlist, local DB path, and schema/profile id.
_Avoid_: workspace, project, implicit repo

**Repo Allowlist**:
The explicit list of GitHub repositories a profile may sync and query.
_Avoid_: org discovery, fallback repo, current-directory inference

**Corpus**:
The set of source entities and source versions currently indexed for one profile.
_Avoid_: knowledge base, dataset

**Source Entity**:
A GitHub issue, issue comment, Wiki page, or Wiki section tracked as a retrievable source.
_Avoid_: document, file

**Source Identity**:
The stable identifier for a source entity, independent from mutable URLs, titles, issue numbers, and Wiki paths.
_Avoid_: URL id, path id, title key

**Locator**:
A mutable way to find or display a source, such as canonical GitHub URL, issue number, title, comment URL, or Wiki path.
_Avoid_: identity, primary key

**Source Version**:
A specific observed version of source content with body hash, GitHub updated timestamp or Wiki commit, and indexed timestamp.
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
A bounded sync pass that compares known source identities against GitHub/Wiki state to detect deletes, renames, transfers, and stale ghosts.
_Avoid_: refresh, cleanup

**Structured Error**:
A schema-visible failure state distinct from no-result success.
_Avoid_: empty result, silent fallback

**Sensitive Derivative Data**:
Local DB rows, snippets, logs, cache files, and embeddings derived from private GitHub content.
_Avoid_: cache, metadata only

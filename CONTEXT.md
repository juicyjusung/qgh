# qgh Domain

qgh is a local-first retrieval context for GitHub Issues and issue comments. The language below keeps product, sync, search, and agent-citation work aligned.

## Language

**Profile**:
A single-user retrieval configuration that fixes GitHub host, token source reference, repo allowlist, XDG profile data path, schema/profile id, SQLite store, and Tantivy index.
_Avoid_: workspace, project, implicit repo

**Repo Allowlist**:
The explicit list of GitHub repositories a profile may sync and query.
_Avoid_: org discovery, fallback repo, current-directory inference

**Profile Resolution**:
The process that selects a profile from an explicit CLI input, explicit environment input, or exactly one profile whose repo allowlist contains the requested repo scope.
_Avoid_: default profile, account fallback

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

**Freshness**:
How current a corpus is with respect to recently changed GitHub issue and issue comment sources.
_Avoid_: live check, remote truth, coverage

**Coverage**:
How much of the intended GitHub issue and issue comment history is represented in a corpus.
_Avoid_: freshness, snapshot age, search quality

**Partial Corpus**:
A corpus whose intended source history is not fully represented yet, even if its recently changed sources may be fresh.
_Avoid_: stale corpus, failed sync, recent-only corpus

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

**Lexical Head**:
The leading BM25 source candidates whose order is preserved when semantic retrieval is used as a complement.
_Avoid_: hybrid winners, semantic head

**Semantic Complement**:
Source candidates recovered through local semantic retrieval without displacing the lexical head.
_Avoid_: BM25 replacement, vector answer

**Reranking**:
An explicitly requested, bounded reordering of already retrieved source candidates that cannot introduce a new source.
_Avoid_: retrieval, semantic search, answer scoring

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

**Live Incremental Sync**:
A sync pass focused on recently changed source entities so freshness can advance without rescanning all history.
_Avoid_: full sync, historical backfill

**Open Issue Sweep**:
A coverage pass that prioritizes currently open issues regardless of age.
_Avoid_: recent bootstrap, live incremental sync

**Historical Backfill**:
A coverage pass that fills older issue and issue comment history after higher-priority current work is represented.
_Avoid_: freshness check, hidden sync

**Recent All-State Bootstrap**:
A bootstrap pass that seeds open and closed sources updated within the lookback window to accelerate initial coverage. It is acceleration, not a corpus boundary.
_Avoid_: corpus boundary, recent-only corpus, lookback cutoff

**Bootstrap Floor**:
The fixed `bootstrap_start - lookback` timestamp stored at bootstrap time. Historical backfill is complete once the history cursor reaches this floor; it must not drift with current time.
_Avoid_: moving cutoff, now-relative window

**Historical Comment Backfill**:
A per-issue comment fetch performed while backfilling an older issue, because repo-level `since` comment listing returns only recently changed comments and cannot recover historical comment coverage.
_Avoid_: repo-level since listing, fresh-only comments

**Lifecycle Verification**:
An explicit opt-in network check on `get` that confirms a source is active, transferred, or unavailable. It is off by default and behaves identically on CLI and MCP; default `get` is local-only.
_Avoid_: implicit get probe, hidden lifecycle check, CLI-only behavior

**Snapshot Age**:
The local time since the last successful sync, the only basis for a freshness decision. It is not a claim about remote truth, which is never probed at query time.
_Avoid_: remote freshness, live check, true currency

**Targeted Refresh**:
An explicit sync pass for a named source entity, independent of age or scheduled coverage priority.
_Avoid_: hidden auto-sync, live probe

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

**Primary Install Channel**:
The user-facing installation path that must work before a qgh release is considered shippable.
_Avoid_: deployment target, optional package

**Release Artifact Origin**:
The authoritative hosted location for immutable qgh release binaries, checksums, and installer inputs.
_Avoid_: package manager, tap, build output

**One-Command Install**:
A single package-manager or installer command that places a self-contained `qgh` binary on the user's PATH.
_Avoid_: bootstrap, sync, first query

**First-Use Smoke Gate**:
The post-install verification path where a user can initialize an explicit repo scope, sync, and run a query after authentication is available.
_Avoid_: install success, hidden sync, automatic repo access

**Release Automation**:
The repeatable system that turns a version tag into release artifacts and updates install channels.
_Avoid_: manual release notes, local build

**Release Target Matrix**:
The operating system and CPU target set that must have release artifacts and install-channel smoke tests for a shippable qgh release.
_Avoid_: supported platforms, best-effort builds

**Release Integrity Gate**:
The checks and provenance evidence that must exist before release artifacts and install channels are trusted.
_Avoid_: optional checksum, local signing experiment

**Release Trigger**:
The explicit version-control action that starts release automation for a qgh version.
_Avoid_: ad hoc workflow run, implicit version bump

**Release Preflight Gate**:
The blocking verification set that must pass before qgh publishes release artifacts or install-channel updates.
_Avoid_: manual dogfood, post-release smoke, advisory check

**Tap Publish Credential**:
The narrowly scoped GitHub credential used by release automation to update the qgh Homebrew formula in the tap repository.
_Avoid_: general GitHub token, user auth, runtime token

**Install Documentation Surface**:
The user-facing and release-contract documents that describe how qgh is installed and verified.
_Avoid_: hidden release notes, package-manager-only docs

**Sensitive Derivative Data**:
Local DB rows, snippets, logs, cache files, and embeddings derived from private GitHub content.
_Avoid_: cache, metadata only

@/Users/user/.codex/RTK.md

# AGENTS.md - agent guidance for qgh

qgh is a local-first, read-only CLI/MCP retrieval tool for GitHub
Issues, issue comments, and Wiki content. The core workflow is
`query -> get -> cite`: search results are source candidates, not
answers.

## Source of truth

- PRD: `qgh-prd.md`
- Product brief: `qgh-product-brief.md`
- MVP evidence decision: `qgh-mvp-evidence-decision-summary.md`
- Research/go-no-go report: `github-issues-wiki-hybrid-search-go-no-go.md`
- Agent setup: `docs/agents/`
- Architecture decisions: `docs/adr/` if present

Read the relevant source-of-truth docs before changing scope, product
contracts, storage/search design, MCP/CLI behavior, privacy defaults, or
validation gates.

## Web Search

Prefer tavily skills over the WebSearch tool. Fall back to WebSearch on failure (e.g., Tavily API token exhaustion).

- Web search: `tavily-search` skill
- URL content extraction: `tavily-extract` skill
- Site crawling: `tavily-crawl` skill

For current GitHub API, MCP, SQLite FTS/vector, or other external
contracts, verify against primary sources before changing product or tool
contracts. Do not rely on model memory for moving APIs or specs.

## Agent skills

### Issue tracker

Issues and PRDs are tracked in GitHub Issues for `juicyjusung/qgh`; external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

The canonical triage roles use the default label strings: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, and `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

This repo uses a single-context domain docs layout. See `docs/agents/domain.md`.

## GitHub issue tracker SSOT

Issues, plans, and PRDs are tracked in GitHub Issues for
`juicyjusung/qgh`. Existing `.scratch/` files are local drafts or legacy
planning material unless the user explicitly says otherwise. When
project truth changes, update the owning GitHub issue: scope, acceptance
criteria, plan, blocker/risk, decision, and final verification.

If the owning GitHub issue is unclear and the update matters, ask for the
issue number. Skip noisy local attempts, secrets, tokens, and sensitive
repo content.

Final responses after issue-backed work should include either:

- `Updated #<number>: <reason>`
- `Issue update not needed: <reason>`

## Product guardrails

- Keep the MVP focused on GitHub Issues, issue comments, and Wiki
  retrieval. Do not broaden into generic RAG, code search, Web UI, shared
  server, write-back, PR/Discussions/Projects indexing, or org-wide
  discovery unless the PRD/ADR/user explicitly changes scope.
- Repo selection must be explicit. Do not add implicit org discovery or
  broad fallback search.
- Default mode must not send private repo content or derived data to
  hosted embedding, hosted rerank, telemetry, or third-party services.
  GitHub host access for sync is expected; local query should not consume
  GitHub search quota.
- BM25-only must remain a complete working path for `sync`, `query`,
  `get`, and `status`. Vector/hybrid work is post-MVP unless scope
  changes, and must not break the BM25 path.
- Query results are not answers. A successful result must carry stable
  source identity, canonical URL, source version/staleness metadata, and
  `get` arguments. Results that cannot round-trip through `get` are not
  successful results.
- MCP v1 is read-only: `query`, `get`, and `status` only. Do not add MCP
  write, sync, embed, delete, or update tools without an explicit scope
  change.
- Config, CLI, and MCP schemas must be strict. Unknown keys, typoed
  parameters, malformed JSON, and invalid enums should fail with
  structured errors, not silent fallback.
- Do not store literal GitHub tokens in config, fixtures, logs, or docs.
  Use token source references.
- Treat local DB, snippets, embeddings, logs, and cache as sensitive
  derivative data. Avoid logging private content; when storage exists,
  prefer single-user file permissions.
- Respect GitHub rate limits during sync. Use bounded backoff and expose
  rate-limit/backoff state instead of hiding it.

## Agent coding discipline

Bias toward correctness and source fidelity over speed.

### Think before coding

- State assumptions when they affect implementation.
- Multiple plausible interpretations exist -> surface them instead of
  silently choosing one.
- Simpler approach exists -> say so.
- A wrong guess could affect privacy, storage format, source identity,
  schema contracts, or user data -> stop and ask.

### Simplicity first

- Build the minimum change that satisfies the request and product
  guardrails.
- Do not add speculative features, abstractions, configurability, or
  dependencies.
- Prefer clear, testable code over clever patterns.
- If work grows large, split it into smaller verified steps.

### Surgical changes

- Touch only files required for the task.
- Do not refactor adjacent code, comments, formatting, or names unless
  required.
- Match existing style and boundaries.
- Clean up imports, variables, functions, tests, and docs made obsolete
  by your own change.
- If unrelated dead code or risk is noticed, mention it instead of
  deleting it.
- Every changed line should trace back to the user request, PRD/ADR,
  source-of-truth docs, or failing verification.

### Goal-driven execution

- Convert tasks into verifiable goals before editing.
- Bug fixes: write or identify a failing test/reproduction first when
  feasible.
- Refactors: verify behavior before and after.
- Multi-step tasks: keep a short plan with verification checkpoints.
- Loop until relevant build, tests, lint, snapshots, or documented manual
  verification passes. If a check cannot run, report the exact blocker.

## Writing language

- GitHub-facing and planning writing is Korean by default: issue bodies,
  PR descriptions, review comments, issue comments, release notes, and
  project-planning docs.
- Keep code identifiers, API names, protocol fields, compiler errors, log
  output, file paths, and Conventional Commit types in English.
- Use English when quoting upstream docs/errors or when consistency with
  existing long-lived technical docs matters.

## Verification

- Docs-only changes: inspect the rendered/diffed markdown and confirm
  links or referenced paths exist when practical.
- CLI/MCP changes: verify JSON schemas, structured errors, stdout/stderr
  separation, and `query -> get -> cite` round-trip behavior.
- Sync/search/storage changes: test pagination, edit/delete/rename,
  tombstone/reconciliation, rate-limit handling, and stale result
  behavior with fixtures or a documented manual reproduction.
- Privacy-sensitive changes: verify no unexpected network egress, no
  token persistence, and no private content in logs/fixtures.

---
name: setting-up-qgh
description: Set up or troubleshoot qgh installation, profiles, authentication references, snapshot coverage, local embeddings, and readiness. Use when the user asks to install, initialize, configure, sync, backfill, diagnose, or repair qgh, or retrieval is blocked because qgh is missing or unready. Never install, sync, run doctor, download a model, rebuild embeddings, or verify lifecycle without explicit authorization for that side effect.
---

# Setting Up qgh

Guide qgh from missing binary to a usable local snapshot while making every network, download, configuration, and data-changing boundary visible.

Keep five layers distinct: this Agent Skill provides instructions; the Homebrew package provides the qgh binary; `qgh init` creates profile/repository configuration; optional model installation enables hybrid retrieval; `qgh sync` creates or refreshes the local snapshot. Installing one layer never implies the others are ready.

## Operating Rule

Start with read-only checks. Before a command with side effects, state what it contacts or changes and confirm the user's request authorizes that specific operation. A request for an explanation is not authorization to install, initialize, sync, run `doctor`, download a model, rebuild embeddings, or verify lifecycle.

Do not ask for or persist a literal GitHub token. qgh configuration stores a `github_cli` or environment-variable source reference, never the token value.

Read [references/setup-and-recovery.md](references/setup-and-recovery.md) before proposing or running setup and repair commands.

## Workflow

1. **Classify the goal.** Distinguish first-time setup, profile/scope configuration, snapshot refresh, historical backfill, optional hybrid setup, and repair. Do not bundle unrelated operations.
2. **Inspect without side effects.** Use `command -v qgh`, `qgh --version`, and, when qgh is configured, `qgh status --json`. `status` is local-only and does not load the model. It reports content-free readiness and aggregate state; it does not identify an exact offending source or chunk.
3. **Preview the next operation.** Show the exact command, affected profile/repo, network destination, local data written, and whether purge is possible. Obtain authorization if the user did not already request that operation explicitly.
4. **Install only when requested.** qgh's published Homebrew binary is separate from agent skill installation and separate from optional model weights.
5. **Initialize explicit scope.** Prefer interactive `qgh init` so a person can review host, repository, profile, token source reference, and paths. Use `qgh init -y` only for explicitly requested automation with all inferred values available and reviewed.
6. **Keep BM25 as the safe default.** The complete `query -> get -> cite` workflow works without embeddings. Install a pinned local model only when the user explicitly wants hybrid retrieval.
7. **Refresh deliberately.** Normal `qgh sync` contacts the configured GitHub host, refreshes BM25, and incrementally updates missing or changed embeddings when a configured model is installed. Use `embed --force` only for an explicit full vector rebuild or repair.
8. **Handle coverage separately from freshness.** Complete open coverage before budgeted historical backfill. Follow qgh's emitted action, but review its scope and side effects before execution.
9. **Verify the outcome locally.** Re-run `qgh status --json`; if lexical retrieval is ready, BM25 remains usable while optional hybrid repair is pending. Hand off to `using-qgh-context` and preserve the local-only `query -> get -> cite` flow. The stable opening command is `qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json`; do not shorten it to `qgh get --json <emitted get_args>`. Do not claim live GitHub correctness from local status.

Use only commands and options confirmed by the installed `qgh --help` or the released reference. Never invent convenience surfaces such as `qgh auth`, `qgh log`, `qgh download`, `qgh embed --only`, `.qghignore`, or code-file indexing; they are not part of qgh's Issue/comment retrieval product.

## Side-Effect Classes

| Command | Boundary |
| --- | --- |
| `qgh status --json` | Local-only metadata; safe readiness check. |
| `qgh query --json` and default `qgh get --json` | Local-only retrieval; no GitHub request. |
| `gh auth status --hostname <host>` | GitHub CLI authentication check that may contact the selected host; honor an explicit no-network boundary. |
| `qgh init` | Writes profile and repository configuration. |
| `qgh sync` | Contacts GitHub, writes the local snapshot, and may purge confirmed unavailable qgh-managed data or repos explicitly removed from the profile allowlist. |
| `qgh sync --backfill` | Same boundary as sync plus a budgeted historical pass. |
| `qgh doctor` | Contacts GitHub and probes the configured local model runtime. |
| `qgh model install ...` | Contacts Hugging Face for pinned public weights; sends no repository content. |
| `qgh embed --force` | Rebuilds all local vector embeddings and may be resource intensive. |
| `qgh get --verify-lifecycle` | Contacts GitHub and may purge content confirmed unavailable. |

## Output Contract

For guidance-only requests, return:

```text
State: <missing | uninitialized | ready-bm25 | ready-hybrid | partial | stale | repair-needed>
Evidence: <content-free status/error fields>
Next command: <one exact command>
Side effects: <network and local writes>
Why this command: <short rationale>
Not doing yet: <operations that still need authorization>
```

After executing an authorized step, report the command outcome without reproducing tokens, raw source bodies, raw queries, full JSON payloads, or user-local storage paths.

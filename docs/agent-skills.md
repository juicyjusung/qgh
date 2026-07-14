# qgh Agent Skill

qgh ships one optional Agent Skill that teaches compatible agents how to
retrieve and cite local GitHub Issue and comment evidence safely. It packages
repeatable workflows and guardrails; it is not a second qgh runtime.

## One Skill, Three Routes

Install one public skill. It routes the request internally, so a person or
agent does not need to choose between overlapping setup, lookup, and research
skills:

| Route | Trigger | Does not do implicitly |
| --- | --- | --- |
| Retrieval | A prior decision, Issue history, comment rationale, or repository context could improve the task. | Install, initialize, sync, doctor, download models, verify lifecycle, or write to GitHub. |
| Research | Planning, debugging, architecture, or review needs multiple historical sources and a traceable brief. | Live-only GitHub checks, writes, code search, generic web research, or implicit maintenance. |
| Setup and recovery | The user asks to install, configure, sync, backfill, diagnose, or repair qgh. | Any operation outside the explicitly requested scope. |

The single entry point keeps `query -> get -> cite` intact across route changes.
Moving from retrieval to setup never grants permission to run a side effect.
A repo-scoped `#N`, Issue/comment URL, or `gh issue` request also invokes the
skill as a routing check, including when `#N` anchors an implementation task.
It may decide that qgh adds no value to a live-only
read or write and route directly to `gh`; PR-only numbers, `gh pr`, headings,
source line numbers, and explicit no-lookup requests do not trigger qgh retrieval.

## Install

The [`skills` CLI](https://github.com/vercel-labs/skills) requires Node.js 18 or newer. Project-local installation is recommended so the selected instruction files remain scoped and reviewable with the project.

Codex:

```sh
npx skills add juicyjusung/qgh --skill qgh --agent codex
```

Claude Code:

```sh
npx skills add juicyjusung/qgh --skill qgh --agent claude-code
```

Add `--global` only for an intentional user-wide install. Do not use a bare `npx skills add juicyjusung/qgh`: this repository also contains maintainer workflow skills, so every user-facing installation must select public names with `--skill`.

Review an external skill's `SKILL.md` and referenced resources before installing it. Installing these skills does not install the `qgh` executable, create qgh configuration, authenticate GitHub, acquire model weights, or synchronize repository content.

## Runtime Contract

The qgh skill enforces these boundaries in every route:

1. Issue-aware invocation first decides whether local qgh evidence helps; a trigger alone does not run qgh.
2. `command -v qgh` and `qgh status --json` establish local availability and readiness when qgh retrieval is selected. Agents check `ok` before reading command-specific `data`.
3. `qgh query --json` finds candidates. Known repo-scoped Issue numbers use exact `#N` plus `--repo`/`--issue N`; supplied URLs pass through unchanged instead of being reconstructed from a guessed host.
4. Successful candidates use the documented `entity_type` and preserve each result's exact `get_args.source_id`/`get_args.profile_id`. Snippets and ranking scores are not evidence or confidence.
5. `qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json` opens the authoritative body. Two to 20 results from one profile can use one batch `get`, with every item checked before synthesis.
6. Citations include the canonical URL and source version, plus freshness and coverage caveats.
7. qgh represents a local snapshot. `gh` is separate live GitHub truth and the authorized write path.
8. Retrieval or skill invocation alone never authorizes `init`, `sync`, `doctor`, `embed`, `model install`, or `get --verify-lifecycle`. An explicitly requested operator task may run only its stated operation after disclosing the boundary.

BM25 remains a complete model-free path. A missing embedding model affects hybrid retrieval, not the ability to perform `query -> get -> cite` with BM25.

## Privacy

Local qgh sources and derived artifacts may reflect private repositories. Skills must not persist tokens, source bodies, raw queries, complete JSON envelopes, snippets, embeddings, cache paths, or other user-local paths in fixtures, issue comments, benchmarks, or diagnostic logs. Durable evidence briefs should retain only the minimum source identity, canonical URL, version, paraphrase, and content-free status needed for the decision.

## Design and Validation

The skill follows the cross-vendor [Agent Skills specification](https://agentskills.io/specification): concise trigger metadata, a focused `SKILL.md`, and one-level on-demand references. Its procedures and evaluation cases follow the official [skill creation best practices](https://agentskills.io/skill-creation/best-practices) and [evaluation guidance](https://agentskills.io/skill-creation/evaluating-skills).

The design also aligns with [OpenAI's Skills guidance](https://help.openai.com/en/articles/20001066) and [Anthropic's skill authoring guide](https://resources.anthropic.com/hubfs/The-Complete-Guide-to-Building-Skill-for-Claude.pdf): package repeatable workflows, keep instructions portable, and require users to review external skills. Distribution syntax and agent targets are verified against the official [Vercel skills CLI](https://github.com/vercel-labs/skills/blob/main/README.md).

Maintainers validate releases with:

- frontmatter and directory-name checks for the single public `qgh` skill;
- focused with-skill and no-skill evaluation cases;
- an isolated local `npx skills` discovery/install smoke test that must find only `qgh`;
- the repository release-contract tests, which reject bare installs and stale availability claims;
- privacy scans that reject token-like fixture content.

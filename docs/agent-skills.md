# Agent Skills for qgh

qgh ships optional Agent Skills that teach compatible agents how to retrieve and cite local GitHub Issue and comment evidence safely. They package repeatable workflows and guardrails; they are not a second qgh runtime.

## Choose a Skill

Install only the workflows you need:

| Skill | Trigger | Does not do automatically |
| --- | --- | --- |
| [`using-qgh-context`](../skills/using-qgh-context/SKILL.md) | Prior decisions, issue history, comment rationale, or repository context could improve an answer or task. | Install, initialize, sync, doctor, download models, verify lifecycle, or write to GitHub. |
| [`setting-up-qgh`](../skills/setting-up-qgh/SKILL.md) | The user explicitly asks to install, configure, sync, backfill, diagnose, or repair qgh. | Any side effect that the user did not authorize after its boundary is made clear. |
| [`researching-with-qgh`](../skills/researching-with-qgh/SKILL.md) | Planning, debugging, architecture, or review needs multiple historical sources and a traceable brief. | Live-only GitHub checks, writes, code search, generic web research, or implicit qgh maintenance. |

The suite is intentionally role-based. A separate skill for every qgh command would fragment the `query -> get -> cite` contract and make unsafe partial workflows easier to trigger.

## Install

The [`skills` CLI](https://github.com/vercel-labs/skills) requires Node.js 18 or newer. Project-local installation is recommended so the selected instruction files remain scoped and reviewable with the project.

Codex:

```sh
npx skills add juicyjusung/qgh --skill using-qgh-context --agent codex
```

Claude Code:

```sh
npx skills add juicyjusung/qgh --skill using-qgh-context --agent claude-code
```

Optional workflows can be selected explicitly:

```sh
npx skills add juicyjusung/qgh --skill setting-up-qgh --skill researching-with-qgh --agent codex
```

Add `--global` only for an intentional user-wide install. Do not use a bare `npx skills add juicyjusung/qgh`: this repository also contains maintainer workflow skills, so every user-facing installation must select public names with `--skill`.

Review an external skill's `SKILL.md` and referenced resources before installing it. Installing these skills does not install the `qgh` executable, create qgh configuration, authenticate GitHub, acquire model weights, or synchronize repository content.

## Runtime Contract

The core skill enforces these boundaries:

1. `command -v qgh` and `qgh status --json` establish local availability and readiness.
2. `qgh query --json` finds candidates; snippets and ranking scores are not evidence or confidence.
3. `qgh get '<get_args.source_id>' --profile-id '<get_args.profile_id>' --json` opens the authoritative body through the exact emitted round-trip values.
4. Citations include the canonical URL and source version, plus freshness and coverage caveats.
5. qgh represents a local snapshot. `gh` is separate live GitHub truth and the authorized write path.
6. The core and research skills never automatically run `init`, `sync`, `doctor`, `embed`, `model install`, or `get --verify-lifecycle`.

BM25 remains a complete model-free path. A missing embedding model affects hybrid retrieval, not the ability to perform `query -> get -> cite` with BM25.

## Privacy

Local qgh sources and derived artifacts may reflect private repositories. Skills must not persist tokens, source bodies, raw queries, complete JSON envelopes, snippets, embeddings, cache paths, or other user-local paths in fixtures, issue comments, benchmarks, or diagnostic logs. Durable evidence briefs should retain only the minimum source identity, canonical URL, version, paraphrase, and content-free status needed for the decision.

## Design and Validation

The suite follows the cross-vendor [Agent Skills specification](https://agentskills.io/specification): concise trigger metadata, a focused `SKILL.md`, and one-level on-demand references. Its procedures and evaluation cases follow the official [skill creation best practices](https://agentskills.io/skill-creation/best-practices) and [evaluation guidance](https://agentskills.io/skill-creation/evaluating-skills).

The design also aligns with [OpenAI's Skills guidance](https://help.openai.com/en/articles/20001066) and [Anthropic's skill authoring guide](https://resources.anthropic.com/hubfs/The-Complete-Guide-to-Building-Skill-for-Claude.pdf): package repeatable workflows, keep instructions portable, and require users to review external skills. Distribution syntax and agent targets are verified against the official [Vercel skills CLI](https://github.com/vercel-labs/skills/blob/main/README.md).

Maintainers validate releases with:

- frontmatter and directory-name checks for every public skill;
- focused with-skill and no-skill evaluation cases;
- an isolated local `npx skills` discovery/install smoke test;
- the repository release-contract tests, which reject bare installs and stale availability claims;
- privacy scans that reject token-like fixture content.

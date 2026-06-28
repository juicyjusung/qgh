# qgh Issue Grill With Docs Design

Date: 2026-06-28 KST
Status: Proposed for review

## Purpose

Add a qgh-specific `issue-grill-with-docs` skill that turns a GitHub issue number into a focused design-sharpening session.

The skill should help a future agent take `#NNN`, gather the owning issue plus qgh product/domain docs, classify open choices, and record only the durable decisions back on the GitHub issue in concise Korean.

## Source Skill

Source: `/Users/user/projects/juicyjusung/juicy-cloak/.agents/skills/issue-grill-with-docs`

The Cloak skill is useful because it wraps `grill-with-docs` around a GitHub issue and uses issue comments as an append-only decision trail. It cannot be copied as-is because it contains Cloak-specific security rules, issue-number assumptions, and product concepts.

## qgh Adaptation

Create:

- `.agents/skills/issue-grill-with-docs/SKILL.md`
- `.agents/skills/issue-grill-with-docs/evals/evals.json`

Keep the skill in `.agents/skills` because the repo's Matt Pocock engineering skills live there, and this skill wraps `grill-with-docs` / `domain-modeling`.

## Triggering

The skill should trigger when the user gives a GitHub issue number and asks to sharpen, grill, clarify, or prepare the issue:

- `$issue-grill-with-docs #3`
- `issue #3 grill 해줘`
- `이슈 3만 보고 구현 전에 설계 날카롭게 만들어줘`
- `#9 Citation Contract 방향 알아서 grill-with-docs로 정리해줘`

If no issue number is present, ask for the issue number and stop.

## GitHub Issues SSOT

GitHub Issues for `juicyjusung/qgh` remain the source of truth for issue-backed work, PRD mirrors, implementation slices, blockers, decisions, and final verification.

The skill may write GitHub issue comments only when explicitly invoked for an issue. This is different from L1 loop triage, which remains local-only.

The skill must not:

- use `.scratch/` as a tracker
- create or edit local tracker files
- apply labels
- close issues
- edit issue bodies
- change milestones or assignees
- create implementation branches, PRs, or worktrees

## Context Gathering

For `#NNN`, gather:

1. `gh issue view <number> --comments --json number,title,body,labels,comments,state,author,assignees,milestone,createdAt,updatedAt,url`
2. `AGENTS.md`
3. `qgh-prd.md`
4. `qgh-product-brief.md`
5. `CONTEXT.md`
6. `docs/agents/issue-tracker.md`
7. `docs/agents/triage-labels.md`
8. `docs/agents/domain.md`
9. Relevant ADRs under `docs/adr/`
10. Relevant code/docs search hits for issue terms

If a document is missing, proceed with available context and mention the gap only if it changes confidence.

## Decision Classes

Use the source skill's D0-D3 model, adapted for qgh.

**D0 Auto Detail**

Small, reversible implementation detail. Decide silently.

Examples:

- local helper naming
- wording inside the issue comment
- test file placement when it follows existing conventions

**D1 Tracked Assumption**

Reversible, but relevant to implementation or agent handoff. Choose the best qgh-consistent path and record it as checked in the issue.

Examples:

- interpreting an ambiguous issue phrase using `CONTEXT.md`
- choosing between equivalent CLI wording that does not change schema
- clarifying an implementation-order assumption

**D2 Provisional Decision**

Material choice that affects qgh contracts or long-lived docs. Recommend and proceed provisionally only if consistent with PRD/ADR guardrails; mark maintainer confirmation as needed before merge or scope lock.

Examples:

- source identity or locator interpretation
- Source Version metadata shape
- storage schema boundaries
- query/get output contract wording
- tombstone/reconciliation behavior
- MCP schema semantics

**D3 Stop Condition**

Continuing may violate qgh guardrails or needs an explicit product decision. Stop after posting a concise blocker comment.

Examples:

- hosted embedding/rerank default
- MCP write/sync tool addition
- broadening MVP beyond Issues/comments/Wiki
- weakening Citation Contract semantics
- storing tokens or private content in config/logs/fixtures
- contradicting an ADR without opening a new ADR decision

## Issue Comment Policy

Write GitHub-facing comments in Korean by default. Keep code identifiers, commands, file paths, API names, labels, and error text in English.

Use the owning issue as an append-only decision log. Do not edit old comments unless explicitly asked.

Post comments only for:

- D1/D2/D3 decisions
- docs changed
- ADR created/proposed
- final grill summary

Do not post full transcripts. Batch related decisions into one compact comment when practical.

### D1/D2 Comment Template

```markdown
## 의사결정 추적

- [x] D1 Assumption: <가정 한 줄>
  - 추천: `<recommended path>`
  - 근거: <짧은 이유>
  - 상태: agent 판단으로 진행

- [ ] D2 Provisional: <결정 한 줄>
  - 추천: `<recommended path>`
  - 근거: <짧은 이유>
  - 대안: <대안 1>, <대안 2>
  - 확인 필요: <merge 전 확인 / scope lock 전 확인>
```

### D3 Comment Template

```markdown
## 자동 Grill 중단

- 중단 사유: <qgh guardrail / ADR 충돌 / privacy risk / 기타>
- 현재 판단: <agent recommendation>
- 필요한 확인: <가장 작은 질문 하나>
```

### Final Summary Template

```markdown
## 자동 Grill 요약

- 결과: <ready for agent / needs maintainer confirmation / blocked>
- 확정한 내용: <짧은 bullet list>
- 추적 중인 결정: <D2 unchecked count and titles>
- 문서 변경: <paths or 없음>
- 다음 단계: <agent brief / issue split / maintainer confirmation / implementation plan>
```

## Domain Docs

The skill may use `domain-modeling` when terms are fuzzy or newly resolved.

Rules:

- `CONTEXT.md` remains glossary-only, not a spec.
- ADRs are created or proposed only for hard-to-reverse, surprising, real trade-off decisions.
- PRD/product brief/ADR edits require human confirmation unless the user explicitly asked for doc updates in the same task.

## Additional Improvements Over The Source Skill

1. Shorter frontmatter description that describes when to use the skill, not the whole workflow.
2. qgh-specific D2/D3 guardrails for source identity, Citation Contract, storage schema, MCP read-only surface, hosted egress, token handling, and BM25-only MVP scope.
3. Explicit split between L1 local-only loops and this user-invoked issue-comment workflow.
4. No hardcoded issue numbers for PRD/Product Brief; qgh reads canonical local docs plus the requested owning issue.
5. Eval cases focused on qgh concepts, including Citation Contract and MCP read-only constraints.
6. A leakage check to prevent Cloak terms such as vault, CryptoKit, OCR, plaintext leak, and iCloud from appearing in the qgh skill.

## Verification

After implementation:

1. Confirm `.agents/skills/issue-grill-with-docs/SKILL.md` exists.
2. Confirm evals exist at `.agents/skills/issue-grill-with-docs/evals/evals.json`.
3. Search the new skill for Cloak-specific terms and confirm no false carryover:
   - `Cloak`
   - `vault`
   - `CryptoKit`
   - `OCR`
   - `iCloud`
   - `plaintext`
4. Confirm the skill mentions GitHub Issues SSOT and `.scratch/` prohibition.
5. Confirm the skill tells agents to use Korean for GitHub-facing comments.
6. Confirm the skill prohibits labels, issue closure, body edits, branch creation, PR creation, and implementation work.
7. Confirm the skill's final response includes either `Updated #<number>: <reason>` or `Issue update not needed: <reason>`.

## Out Of Scope

- Adding GitHub Actions automation
- Automatically applying labels
- Automatically creating agent briefs
- Running implementation from the grill skill
- Replacing the existing `grill-with-docs` skill

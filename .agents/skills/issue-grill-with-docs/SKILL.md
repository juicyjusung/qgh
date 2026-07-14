---
name: issue-grill-with-docs
description: Use when user gives a GitHub issue number and wants qgh issue design sharpened, grilled, clarified, prepared before implementation, or tracked as concise Korean GitHub issue decision comments. Prefer this over raw grill-with-docs for issue-number-driven qgh work.
metadata:
  internal: true
disable-model-invocation: true
---

# qgh Issue Grill With Docs

Turn a GitHub issue number into a focused `grill-with-docs` session for qgh. GitHub Issues remain tracker source of truth. Issue comments become append-only decision trail only when this skill is explicitly invoked.

Use `grill-with-docs` and `domain-modeling`. This skill is wrapper: issue is input surface, qgh docs are context, Korean issue comments hold durable D1/D2/D3 decisions.

## Input

Accept:

- `#3`
- `issue #3`
- `$issue-grill-with-docs #3`
- `이슈 3 grill 해줘`
- `#9 Citation Contract 방향 구현 전에 정리해줘`

If no issue number exists, ask for issue number and stop.

## Preconditions

- Use `gh` for GitHub issue reads/comments.
- Use GitHub Issues for `juicyjusung/qgh` as tracker source of truth.
- Write GitHub-facing text in Korean.
- Keep code identifiers, API names, file paths, commands, compiler errors, labels, and protocol fields in English.
- Never use `.scratch/` as tracker source of truth.
- Never include tokens, secrets, noisy local attempts, or private content in issue comments.

## Context Gathering

1. Read owning issue:
   ```bash
   gh issue view <number> --comments --json number,title,body,labels,comments,state,author,assignees,milestone,createdAt,updatedAt,url
   ```
2. Read qgh source-of-truth docs:
   - `AGENTS.md`
   - `qgh-prd.md`
   - `qgh-product-brief.md`
   - `CONTEXT.md`
   - `docs/agents/issue-tracker.md`
   - `docs/agents/triage-labels.md`
   - `docs/agents/domain.md`
   - relevant ADRs under `docs/adr/`
3. Search repo for terms from issue title/body/comments. Use qgh domain terms, not just exact words.
4. Detect previous grill/decision comments on issue. Do not reopen resolved decisions unless new evidence contradicts them.

If a referenced doc is missing, continue with available context. Mention gap only when it changes confidence.

## Decision Classes

Classify each open choice before acting.

### D0 Auto Detail

Small, reversible, local implementation detail. Decide silently.

Examples:

- helper naming
- local test placement following existing convention
- wording cleanup in decision comment

### D1 Tracked Assumption

Reversible, but affects implementation direction or handoff. Choose qgh-consistent best path and record as checked issue comment.

Examples:

- interpreting fuzzy issue wording using `CONTEXT.md`
- choosing an implementation order assumption
- clarifying CLI wording that does not change schema

### D2 Provisional Decision

Material choice affecting qgh contracts or long-lived docs. Recommend path, proceed provisionally only when consistent with PRD/ADR, and mark maintainer confirmation needed before merge or scope lock.

Examples:

- Source Identity or Locator interpretation
- Source Version metadata shape
- storage schema boundary
- query/get output contract wording
- tombstone/reconciliation behavior
- MCP schema semantics
- Citation Contract details

### D3 Stop Condition

Continuing may violate qgh guardrails or needs explicit product decision. Stop after concise blocker comment.

Examples:

- broadening MVP beyond Issues, issue comments, and Wiki retrieval
- adding write-capable or sync-capable MCP tool
- weakening Citation Contract semantics
- enabling hosted embedding/rerank or telemetry by default
- storing tokens or private content in config, fixtures, logs, or cache
- contradicting ADR without new ADR decision

## Grill Loop

Repeat until issue is implementation-ready or blocked:

1. State current design hypothesis internally.
2. Split open choices into D0/D1/D2/D3.
3. Apply D0 silently.
4. For D1/D2, choose recommended qgh-consistent path and append compact Korean issue comment.
5. For D3, stop and ask smallest necessary question.
6. Use `domain-modeling` when qgh terms are fuzzy or newly resolved.
7. Propose ADR only when decision is hard to reverse, surprising without context, and a real trade-off.
8. Keep scope on owning issue. Do not begin implementation.

## Issue Comment Policy

Use owning issue as append-only log. Do not edit old comments unless user explicitly asks.

Post comments only for:

- D1/D2/D3 decisions
- docs changed
- ADR created/proposed
- final grill summary

Do not post full transcript. Batch related decisions into one comment when practical.

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

### Docs Changed Template

```markdown
## 문서 업데이트

- 변경: `<path>`
- 이유: <용어 확정 / ADR 기록 / scope 정리>
- 연결된 결정: <D1/D2 id or short title>
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

## qgh Guardrails

These require D2 or D3 handling:

- PRD/product scope
- Source Identity, Locator, Source Version
- tombstone/reconciliation
- Citation Contract
- strict CLI/config/MCP schemas
- MCP read-only surface
- BM25-only MVP path
- hosted egress or telemetry
- token source handling
- private-content persistence in logs, fixtures, DB, or cache

This skill must not:

- apply labels
- close issues
- edit issue bodies
- change milestones or assignees
- create implementation branches, PRs, or worktrees
- start implementation
- replace `grill-with-docs`

## Final Response

End with:

- issue number processed
- comments posted count
- docs changed
- D2 confirmations still open
- exact verification performed

Also include:

`Updated #<number>: <reason>`

If no comment was posted:

`Issue update not needed: <reason>`

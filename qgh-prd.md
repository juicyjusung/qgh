# qgh PRD

문서 역할: canonical full PRD.
관련 문서: `qgh-product-brief.md`는 제품 포지셔닝/brief다. 별도 tracker working PRD가 생기면 이 파일의 파생본이어야 한다.
업데이트 규칙: MVP 요구사항, acceptance criteria, release gate, source model, CLI contract, MCP adapter contract가 바뀌면 이 파일을 먼저 수정하고 필요한 mirror 문서를 동기화한다.

작성일: 2026-06-27 KST
개정: 2026-06-27 KST (리뷰 반영 — positioning 정정, search-quality gate 수치화, 삭제 감지 전략, vector post-MVP 연기, GHES 결정, privacy hardening)
개정: 2026-06-28 KST (문서 역할 정리 — canonical PRD와 tracker working PRD 관계 명시)
개정: 2026-06-29 KST (MVP scope reset — GitHub Issues와 issue comments만 포함, Wiki는 post-MVP source connector로 연기)
개정: 2026-06-29 KST (architecture lock — Rust single-binary CLI, bundled SQLite authoritative store, Tantivy derived BM25 index, explicit XDG profile store)
개정: 2026-06-29 KST (grill closure — CLI-only doctor, REST sync scheduler contract, user-facing eval deferral)
개정: 2026-06-29 KST (config policy grill — worktree-root repo policy and repo scope defaults)
개정: 2026-06-29 KST (profile resolution grill — CLI/env/single-match profile precedence)
개정: 2026-06-29 KST (effective scope diagnostics — CLI meta, status, doctor)
개정: 2026-06-29 KST (MCP repo policy/profile resolution — read-only tools share CLI scope contract)
개정: 2026-06-29 KST (repo policy bootstrap — CLI-only `init` creates tracked `.qgh.toml`)
개정: 2026-06-30 KST (first-run init wizard and git-origin Command Resolution)
개정: 2026-06-30 KST (MVP token source contract — `github_cli`/`env` only, `credential_store` post-MVP)
개정: 2026-06-30 KST (CLI-first contract — MCP is a read-only thin adapter over CLI JSON/local retrieval)
개정: 2026-06-30 KST (init preset preview, customize fallback, and `-y` alias)
제품명: qgh
문서 목적: GitHub Issues와 issue comments용 CLI-first local retrieval 도구와 read-only MCP adapter의 MVP 요구사항을 정의한다.

## 1. Executive Summary

qgh는 개발자와 AI coding agent가 명시적으로 허용한 GitHub repository의 Issues와 issue comments를 로컬 인덱스로 동기화한 뒤 CLI `--json` 기반 `query -> get -> cite` 흐름으로 검색하고 원문을 확인하게 하는 CLI-first read-only retrieval 도구다. MCP v1은 같은 JSON/local retrieval contract를 감싸는 선택적 read-only thin adapter다.

MVP의 핵심 제품 판단은 “GitHub 검색을 대체하는 범용 플랫폼”이 아니라 “agent가 CLI `--json` 또는 그 thin adapter인 MCP로 private repo의 Issues와 issue comments를 rate-limit 없이 반복 검색하고, 검색 결과를 답변으로 착각하지 않고 stable source id로 `get`을 호출해 canonical URL과 원문까지 결정적으로 도달하게 하는 로컬 검색 계층”이다.

qgh의 차별점은 두 종류의 경쟁군에 대해 서로 다르게 성립하며, 이 둘을 섞어 말하지 않는다.

- **GitHub native semantic/hybrid issue search 대비** (데이터가 이미 GitHub에 있으므로 privacy는 축이 아니다):
  - GitHub native issue search가 놓치는 issue comments를 MVP 검색 corpus에 포함한다.
  - agent의 반복 호출이 GitHub search rate limit/quota를 태우지 않는 rate-limit-free local **query**를 제공한다. (단, `sync`는 여전히 GitHub rate limit 대상이다.)
  - 검색 결과를 답변처럼 취급하지 않고, stable source id로 `get`을 호출해 canonical URL과 원문을 확인하는 결정적 citation contract를 강제한다.
  - query-time GitHub 의존 없이 offline/local에서 동작한다.
- **third-party hosted RAG/embedding 도구 대비** (mcp-local-rag류):
  - private repo content와 derived data(snippet/embedding/log)를 외부 embedding/rerank provider로 보내지 않는 local-first privacy default를 제공한다.

설계 기본값은 Rust single-binary CLI, strict `--json` envelope/schema, bundled SQLite authoritative store, Tantivy derived BM25 search index가 단독으로 전체 workflow를 완성하는 것이다. MCP는 이 CLI JSON/local retrieval contract를 재사용하는 adapter이며, Vector/hybrid는 MVP release gate에서 제외된 post-MVP capability다(§6.1).

가정: MVP 사용자는 이미 GitHub token 또는 GitHub CLI 인증을 가진 개발자이며, repo allowlist를 직접 지정할 수 있다. 조직형 shared server, org-wide discovery, Web UI, hosted embedding/rerank default는 MVP 밖이다.

## 2. Product Definition

qgh는 GitHub Issues와 issue comments를 위한 local-first source retrieval product다. 사용자는 repo allowlist를 설정하고 `sync`로 로컬 인덱스를 만든 뒤 CLI 또는 MCP에서 `query`를 실행한다. 결과는 answer가 아니라 source candidate이며, 각 결과는 `get`에 필요한 source identity와 canonical GitHub URL을 포함한다. 사용자나 agent는 반드시 `get`으로 원문을 확인하고 citation에 URL과 source identity를 사용한다.

제품 판단과 기술 판단은 다음처럼 분리한다.

| 구분 | 결정 | 이유 |
|---|---|---|
| 제품 판단 | CLI-first local-first single-user retrieval | private repo 문맥 검색의 초기 adoption barrier는 서버 운영보다 privacy, 설치 신뢰성, shell에서 검증 가능한 JSON contract다. |
| 제품 판단 | read-only MVP | agent workflow에서 write 권한은 auth, audit, accidental mutation 리스크를 급격히 키운다. |
| 제품 판단 | explicit repo allowlist | 암묵적 org discovery는 wrong repo indexing, private corpus leakage, rate-limit 폭발을 만든다. |
| 제품 판단 | `query -> get -> cite` | snippet 기반 답변과 hallucinated citation을 막고 GitHub 원문을 최종 근거로 둔다. |
| 기술 판단 | Rust single-binary CLI with MCP adapter | agent가 반복 호출하는 로컬 제품 CLI이므로 런타임 설치, Node/Python 버전, native extension drift를 줄인다. MCP는 CLI contract를 감싸는 integration adapter다. |
| 기술 판단 | REST Issues/comments sync | GitHub Issues/comments는 REST incremental 수집이 단순하고, MVP source lifecycle을 한 API family 안에서 검증할 수 있다. |
| 기술 판단 | Bundled SQLite authoritative store + Tantivy derived BM25 index | SQLite는 source truth와 lifecycle을 안정적으로 보관하고, Tantivy는 Rust-native search 성능과 BM25 검색 품질을 제공한다. |
| 기술 판단 | vector/hybrid는 post-MVP (release gate 아님) | semantic recall은 유용하지만 private data egress, model install, fingerprint drift를 MVP 필수 조건으로 만들면 adoption이 늦어진다. §6.1로 명시 연기한다. |
| 제품 판단 | GitHub.com first-class, GHES best-effort | GHES API/rate-limit 차이는 profile capability로 표현하되 MVP release gate에 묶지 않는다(§17 Q1 결정). |

Adjacent/competing tools 대비 qgh의 위치는 다음과 같다.

| Alternative | 현재 강점 | qgh가 다르게 푸는 문제 |
|---|---|---|
| GitHub native semantic/hybrid issue search | GitHub UI/API에서 issue semantic search를 바로 제공한다. | qgh는 issue comments coverage, agent 반복 호출에 대한 rate-limit-free local query, `query -> get -> cite` citation contract, offline 동작을 제품 핵심으로 둔다. (privacy는 native search 대비 차별점이 아니다 — 데이터가 이미 GitHub에 있다.) |
| GitHub official MCP server | GitHub API를 MCP tool로 노출해 agent integration이 쉽다. | qgh는 live API proxy가 아니라 CLI-first local read-only index이며 MCP v1을 CLI JSON/local retrieval contract의 `query`, `get`, `status` adapter로 제한한다. |
| dogsheep/github-to-sqlite | GitHub 데이터를 SQLite로 가져오는 prior art다. | qgh는 Issues/comments retrieval, strict source identity, MCP citation workflow, search quality gate까지 제품 contract로 묶는다. |
| qmd | local-first 문서 검색과 MCP `query/get/status` workflow reference다. | qgh는 filesystem path가 아니라 GitHub issue/comment source lifecycle과 canonical URL citation을 중심 모델로 삼는다. |
| mcp-local-rag | local-first MCP RAG 패턴을 제공한다. | qgh는 generic local RAG가 아니라 GitHub Issues/comments connector, third-party egress 없는 privacy default, source-correct citation에 집중한다. |
| Sourcegraph-style code search | code corpus와 enterprise search UX가 강하다. | qgh MVP는 code search가 아니라 Issues/comments decision context retrieval이다. |

## 3. Problem Statement

GitHub Issues와 issue comments는 많은 프로젝트에서 의사결정, 장애 대응, 작업 맥락, 운영 지식의 실제 기록이다. 하지만 시간이 지나면 다음 문제가 반복된다.

- Issues body와 comments가 분산되어 한 질문의 근거를 찾는 데 시간이 오래 걸린다.
- GitHub native semantic issue search는 유용하지만 comments coverage가 없고, agent가 반복 호출하면 search rate limit/quota를 소모하며, query-time GitHub 연결과 cloud 처리에 의존하고, comment-level citation을 보장하지 않는다. (“GitHub 검색이 약하다”가 아니라 coverage·반복 호출 비용·결정적 citation·offline 요구가 다르다.)
- private repo 내용을 third-party hosted embedding/rerank provider로 보내기 어려운 조직이 많다. (이 privacy 우려는 third-party 도구 대비 축이며, GitHub native search 대비 축이 아니다.)
- agent가 search snippet만 보고 답하면 원문 확인 없이 잘못된 citation을 만들 수 있다.
- shared server를 먼저 만들면 ACL, token, stale index, 운영 문제가 product validation보다 먼저 커진다.

따라서 MVP 문제는 “GitHub 지식 전체를 검색하는 플랫폼”이 아니라 “허용된 repo의 Issues와 issue comments를 로컬에서 rate-limit 없이 반복 검색하고, 원문으로 검증 가능한 citation까지 이어지는 workflow를 제공하는 것”이다.

## 4. Target Users and Use Cases

| 사용자 | 주요 use case | 성공 기준 |
|---|---|---|
| 개발자 | 과거 버그, 결정 배경, 장애 대응 thread 검색 | GitHub UI에서 오래 걸리던 source를 top results에서 찾고 원문 URL로 이동한다. |
| AI coding agent 사용자 | private repo context를 agent가 검색하되 원문 확인과 citation을 강제 | agent가 `query -> get -> cite`를 반복하고 snippet만으로 결론을 내리지 않는다. |
| maintainer/tech lead | repo에 누적된 논의와 운영 지식을 개인 로컬 환경에서 재사용 | shared service 없이 repo별 knowledge retrieval 가치를 검증한다. |
| 보안/플랫폼 담당자 | private content가 기본적으로 third-party provider로 나가지 않는 도구 검토 | default mode에서 GitHub host 외 egress가 없고 MCP가 read-only이며 local DB 권한이 단일 사용자로 제한됨을 확인한다. |

## 5. User Workflows

### 5.1 Setup

사용자는 `qgh init` first-run wizard 또는 `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`의 strict TOML profile config로 GitHub host, token source, repo allowlist를 명시한다. Top-level `qgh init`은 current git worktree `origin`에서 repo/host defaults를 감지하고, 기본 profile id `work`, token source `github_cli`, config path, repo policy path, profile DB path를 preview한 뒤 Enter/`Y`면 preset을 적용하고 `n`이면 customize prompt로 들어간다. EOF 취소는 파일을 쓰지 않고 `validation.init_cancelled`로 종료한다. `qgh init --yes`와 `qgh init -y`는 preview/prompt 없이 같은 inferred preset을 적용한다. Repository는 tracked repo policy config로 repo scope와 안전한 기본 filter를 정의할 수 있으며, CLI-only `qgh init repo`는 current git worktree root의 `.qgh.toml` repo policy bootstrap만 제공한다. SQLite DB와 Tantivy index path는 profile id와 XDG data dir에서 deterministic하게 파생하며, MVP에서는 임의 DB path override를 제공하지 않는다. qgh는 explicit `--profile` 또는 `QGH_PROFILE`을 우선 사용하고, 없으면 current worktree repo policy 또는 Git `origin` remote에서 얻은 effective repo scope를 allowlist에 포함하는 profile이 정확히 하나일 때만 그 profile을 자동 선택한다. matching profile이 없거나 여러 개면 structured error로 실패한다.

왜 필요한가: 일반적인 repo/worktree 위치에서는 현재 repository의 Issues/comments만 쉽게 조회하게 하되, CLI와 MCP가 다른 private corpus를 보거나 typo 때문에 private repo 범위가 넓어지는 실패를 막아야 한다.

### 5.2 Sync

사용자는 CLI에서 명시적으로 `sync`를 실행한다. current repo Effective Scope가 있으면 qgh는 profile 전체가 아니라 해당 repo만 기본 동기화하며, broad profile-wide sync는 explicit `--all`로만 수행한다. qgh는 Issues title/body/metadata와 issue comments를 수집하고 local index 상태를 갱신한다. incremental sync는 `since` 기반 create/edit를 수집하고, 삭제/transfer는 §FR-05의 reconciliation 전략으로 감지한다. MCP는 MVP에서 sync를 실행하지 않는다.

왜 필요한가: agent가 expensive sync나 token-consuming job을 반복하지 않게 하고, 사용자가 repo allowlist와 stale state를 통제하게 해야 한다.

### 5.3 Query

사용자 또는 agent는 자연어, keyword, issue number, URL, repo/label/state/author 같은 filter로 `query`를 실행한다. 현재 git worktree root에 repo policy가 있으면 CLI와 MCP adapter 모두 기본 query repo scope는 해당 repository의 Issues/comments다. repo policy가 없어도 Git `origin` remote가 configured profile host와 allowlist repo에 매핑되면 같은 Effective Scope를 사용한다. 다른 repo를 조회하려면 CLI는 `--repo owner/repo`, MCP adapter는 `repo` tool argument를 명시해야 하며, 이 repo도 resolved profile allowlist 안에 있어야 한다. 결과는 source candidate이며 `source_id`, `entity_type`, snippet, canonical URL, `get_args`, ranking signal, source 신선도(`indexed_at`/source version)를 포함한다. `source_id`는 `qgh://<host>/issue/<percent-encoded-node_id>` 또는 `qgh://<host>/issue-comment/<percent-encoded-node_id>` 형식이다. snippet은 미리보기일 뿐이며 citation 근거가 아니다 — 결과는 `get`으로 round-trip해야 한다.

왜 필요한가: 검색은 답변이 아니라 원문 후보를 찾는 단계다. 결과가 `get`으로 이어지지 않으면 agent citation workflow가 깨진다.

### 5.4 Get

사용자 또는 agent는 `get(source_id)` 또는 결과의 `get_args`로 원문을 조회한다. `get_args`는 `source_id`와 해당 source를 반환한 profile store의 `profile_id`를 포함해 repo policy 또는 explicit repo override 결과도 같은 store로 round-trip해야 한다. `get --profile-id`는 current cwd보다 우선한다. `--profile-id`가 없고 current Effective Scope가 있으면, qgh는 scope 밖 source를 반환하지 않고 structured error로 실패한다. qgh는 issue body 또는 comment body와 parent context, canonical GitHub URL, source version metadata(body hash, GitHub updated timestamp, `indexed_at`)를 반환한다.

왜 필요한가: snippet은 근거로 충분하지 않다. final answer와 human verification은 authoritative source를 기준으로 해야 하며, agent는 source가 GitHub 대비 얼마나 stale할 수 있는지 알아야 한다.

### 5.5 Cite

agent는 `get` 결과의 canonical URL과 source identity를 citation에 사용한다. comment 결과는 parent issue context와 comment URL을 함께 제공한다.

왜 필요한가: 사용자는 답변의 근거를 GitHub에서 재확인할 수 있어야 하며, comment-only answer도 parent issue를 잃지 않아야 한다.

### 5.6 Status

사용자는 `status`로 resolved profile, effective repo scope, repo policy path, profile store paths, last sync, indexed source count, stale/tombstone count, DB/schema version, Tantivy index generation, dirty index task count를 확인한다. vector mode가 활성일 때는 missing embedding count와 fingerprint 상태를 추가로 표시한다. `status`는 network probe나 model load를 수행하지 않는다.

### 5.7 Doctor

사용자는 CLI에서 명시적으로 `doctor`를 실행해 repo policy parse/validation, profile resolution, allowlist match count, ambiguous/no-match details, file permissions, SQLite/Tantivy consistency, GitHub auth/reachability, rate-limit response headers를 점검한다. `doctor`는 MCP tool이 아니며 agent가 자동으로 호출할 mutation/sync/probe surface를 늘리지 않는다.

왜 필요한가: `status`는 빠르고 부작용 없는 local snapshot이어야 한다. 네트워크/auth/probe가 필요한 진단은 사용자가 명시적으로 실행한 `doctor`로 분리해야 한다.

## 6. MVP Scope

MVP에 포함한다.

- GitHub.com first-class host. GHES는 profile capability로 표현하되 best-effort이며 release gate에서 제외(§17 Q1).
- Rust single-binary CLI with read-only MCP adapter
- XDG Base Directory 기반 config/data/cache 분리
- explicit profile selection or single-match profile auto-resolution from repo scope
- worktree-root repo policy config for repo scope and safe default filters
- explicit repo allowlist
- GitHub Issues title/body/state/labels/milestone/assignees/author/timestamps/canonical URL sync
- issue comments body/author/timestamps/canonical URL sync
- source identity와 mutable locator 분리
- edit/delete/transfer 가능성을 반영하는 tombstone/reconciliation requirement와 명시적 삭제 감지 전략(FR-05)
- bundled SQLite authoritative store
- Tantivy derived BM25 search index with shadow build and atomic publish
- citation granularity를 위한 source chunking (issue body/comment)
- 한국어/영어 mixed corpus를 고려한 Tantivy tokenizer baseline과 CJK n-gram fallback field (eval로 검증)
- CLI: `init`, `sync`, `query`(canonical; `search`는 alias), `get`, `status`, `doctor`
- MCP tools: read-only `query`, `get`, `status` as thin adapters over the CLI JSON/local retrieval contract
- strict CLI/config/MCP schema validation, with CLI args + JSON schemas + local store/search behavior as the contract source of truth
- versioned JSON output schema와 human-readable CLI output
- local DB/log 파일 single-user 권한 hardening
- 수치 target이 정의된 curated 20~30 query validation set (directional gate)

### 6.1 Post-MVP로 명시적으로 연기 (release gate 아님)

다음은 설계상 자리를 잡되 MVP acceptance(AC-01~AC-27, 단 AC-13/AC-20 제외)의 전제가 아니다. M6에서 prototype 또는 연기를 결정한다. M6의 first candidate는 local ONNX embedding runtime + local vector index이며, hosted embedding/rerank는 별도 privacy policy와 explicit opt-in 전에는 후보가 아니다.

- GitHub Wiki source connector (`{repo}.wiki.git` sync, page/section chunking, wiki lifecycle reconciliation)
- optional local vector/hybrid search over the same authoritative SQLite source ids
- embedding fingerprint(provider/model/dimension/chunker/schema)와 partial coverage gating
- missing embedding count를 제외한 vector-specific status

연기 이유: BM25-only 경로만으로 핵심 가설(local read-only retrieval correctness, agent citation workflow)을 검증할 수 있고, vector를 MVP 필수로 두면 model install·egress·fingerprint drift 변수가 가설 검증보다 먼저 커진다.

MVP 제약은 다음을 변경 불가 조건으로 둔다.

- vector 기능이 없어도 `sync -> query -> get -> cite -> status`가 완전히 작동해야 한다 (vector는 post-MVP).
- hosted embedding/rerank는 default가 아니며 MVP acceptance의 전제 조건이 아니다.
- rate-limit-free 가치는 `query`에만 적용된다. `sync`는 GitHub rate limit 대상이다.
- search result는 confidence가 아니라 ranking evidence를 제공한다.
- `get`으로 원문을 확인할 수 없는 result는 성공 result가 아니다.
- MCP v1은 write, sync, embed, delete, update tool을 제공하지 않는다.

## 7. Non-Goals

MVP에서 제외한다.

- organization-wide repo auto-discovery
- shared server, central index, team index, remote sync
- full ACL enforcement 또는 query-time GitHub permission recheck
- GitHub write-back, issue 생성/수정, label 변경, Wiki edit
- PR, Discussions, Projects, Actions logs, full timeline events
- GitHub Wiki sync/search/get/status
- Web UI
- webhook listener 또는 watch daemon 기반 near-real-time sync
- hosted embedding/rerank/query expansion default
- OpenSearch, Meilisearch, Typesense 등 별도 검색 서버
- fine-tuned ranking/query expansion
- answer generation 또는 autonomous summarization

참고: optional vector/hybrid는 Non-Goal이 아니라 §6.1의 post-MVP 연기 항목이다 (거부가 아니라 release gate에서 분리).

왜 제외하는가: 위 항목은 모두 유용할 수 있지만 MVP의 핵심 가설인 local read-only retrieval correctness, privacy default, agent citation workflow 검증보다 ACL/운영/품질 변수를 먼저 키운다.

## 8. Functional Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| FR-01 | qgh는 profile마다 GitHub host, token source reference, repo allowlist, derived XDG data profile path를 명시적으로 고정해야 한다. Profile resolution은 CLI `--profile`, `QGH_PROFILE`, repo scope의 single matching profile 순서로 동작한다. single matching profile은 현재 worktree repo policy, Git `origin` remote, 또는 explicit `--repo`가 정한 repo scope가 정확히 하나의 profile allowlist에 포함될 때만 허용한다. Top-level `qgh init`은 first-run preset preview와 customize fallback으로 XDG profile config와 repo allowlist를 bootstrap하고, `qgh init --yes`/`-y`는 같은 preset을 non-interactive로 적용하며, `qgh init repo`는 repo policy bootstrap만 제공한다. Runtime이 GitHub CLI -> environment -> OS credential store로 자동 fallback하면 안 된다. | common repo/worktree workflow를 짧게 유지하면서 wrong repo/private corpus 검색, ambiguous profile drift, wrong-account fallback을 방지한다. | AC-01, AC-12 |
| FR-02 | qgh는 allowlisted repo의 Issues title/body와 metadata(state, labels, milestone, assignees, author, created_at/updated_at/closed_at, canonical URL)를 `state=all`로 backfill 및 `since` incremental sync해야 한다. REST issue payload에 `pull_request` field가 있으면 PR로 취급해 MVP corpus에서 제외한다. | issue body와 명시된 metadata는 source lookup과 filter의 기본 corpus다. PR은 source lifecycle과 permissions가 달라 MVP 범위를 흐린다. | AC-02, AC-03 |
| FR-03 | qgh는 allowlisted repo의 issue comments(body, author, timestamps, canonical URL)를 backfill 및 `since` incremental sync해야 한다. | comments에는 결정 배경과 장애 대응 맥락이 많고 GitHub native semantic issue search와의 핵심 차별점이다. | AC-02, AC-03 |
| FR-04 | qgh MVP는 GitHub Wiki를 sync/search/get/status 대상으로 포함하지 않아야 하며, Wiki connector는 post-MVP capability로 문서화해야 한다. | MVP는 issue/comment lifecycle과 citation contract를 먼저 검증한다. Wiki git clone, auth, rename/delete diff는 같은 제품 가설에 필요하지 않은 별도 connector 리스크다. | AC-04 |
| FR-05 | qgh는 supported lifecycle fixture에서 issue/comment edit, comment delete, issue transfer/unavailable 후 stale ghost result가 active search에 남지 않게 해야 한다. `since` listing이 삭제를 반환하지 않으므로, (a) issue `comments` count/`updated_at` 변화 시 해당 issue comment 재나열·diff, (b) `get` 시 404/410/301 → tombstone, (c) explicit full reconciliation command(기본 manual, optional `reconcile_after_days`는 status warning 우선)로 삭제·transfer를 감지한다. | 삭제되거나 이동된 private content가 검색/citation되면 제품 신뢰와 privacy가 깨진다. incremental sync만으로는 삭제를 감지할 수 없다. | AC-03, AC-05 |
| FR-06 | qgh는 Tantivy BM25-only path로 query, get, status를 완전히 지원해야 한다. | vector extension, model download, GPU/runtime 실패가 core adoption을 막지 않아야 한다. | AC-06 |
| FR-07 | qgh는 worktree-root repo policy에서 기본 repo scope와 safe default filters를 resolve하고, exact lookup과 structured filter를 지원하며, filter는 query expansion 또는 semantic rewrite의 영향을 받지 않아야 한다. Explicit `--repo`는 repo policy 기본값을 override할 수 있지만 resolved profile allowlist 밖 repo로 넓힐 수 없다. | repo/label/state/author/issue scope가 넓어지는 agent misuse를 막고 현재 repo의 Issues/comments를 기본 retrieval surface로 만든다. | AC-07 |
| FR-08 | 모든 query result는 stable `source_id`, `entity_type`, canonical URL, snippet, `get_args`, parent context, `indexed_at`/source version을 포함해야 한다. `source_id`는 GitHub `node_id` 기반 `qgh://<host>/issue/...` 또는 `qgh://<host>/issue-comment/...` URI여야 한다. | 검색 결과가 원문 조회와 citation으로 round-trip되어야 하고 agent가 staleness를 인지해야 한다. | AC-08 |
| FR-09 | `get`은 issue body 또는 comment body 원문과 canonical GitHub URL, source version(body hash, GitHub updated timestamp, `indexed_at`)을 반환해야 한다. | final answer의 근거는 search snippet이 아니라 authoritative source여야 하며 신선도를 판단할 수 있어야 한다. | AC-08, AC-09 |
| FR-10 | `status`는 resolved profile, profile source, effective repo scope, repo source, repo policy path, last sync, source count, stale/tombstone count, DB/schema version, profile paths, Tantivy active generation, dirty index task count, reconciliation age를 표시하고, vector mode 활성 시 missing embedding count를 추가 표시해야 한다. | 검색 실패의 원인이 stale index인지, wrong profile/scope인지, stale reconciliation인지, partial vector coverage인지 진단해야 한다. | AC-10 |
| FR-11 | CLI/config/MCP adapter는 strict schema validation을 적용하고 unknown key/parameter를 structured error로 실패시켜야 한다. | typo가 broad search, wrong profile, silent no-op으로 바뀌면 private repo 검색에서 위험하다. | AC-11 |
| FR-12 | MCP v1은 read-only `query`, `get`, `status`만 제공하고, CLI와 같은 profile/repo policy resolution, Effective Scope metadata, structured tool errors를 사용해야 한다. | agent가 sync, write-back, embedding job, external egress를 임의로 유발하지 않게 하면서 current worktree repo scope를 벗어난 retrieval을 막는다. | AC-12 |
| FR-13 | (post-MVP) optional vector/hybrid path는 fingerprint와 partial coverage 상태를 추적해야 하며 BM25 path를 깨뜨리면 안 된다. MVP release gate 아님. | semantic quality 개선은 필요하지만 mixed model/vector state는 잘못된 ranking을 만든다. | AC-13 |
| FR-14 | qgh는 no result, validation error, auth error, rate-limit/backoff state, stale index warning을 JSON schema상 구분해야 한다. | agent와 shell automation이 실패를 성공 결과로 오인하지 않아야 한다. | AC-11, AC-14 |
| FR-15 | qgh는 GitHub config에 token 평문을 저장하지 않고 MVP supported source reference(`github_cli`, `env`)로만 다뤄야 한다. `credential_store`와 GitHub App token은 post-MVP capability다. Runtime source fallback은 금지한다. | local config leak가 private repo access leak로 이어지지 않고 wrong-account fallback이 생기지 않아야 한다. | AC-26 |
| FR-16 | qgh는 CLI-only `doctor` command를 제공해 repo policy, profile resolution, allowlist match count, config/profile, local permissions, SQLite/Tantivy consistency, GitHub auth/reachability, and rate-limit headers를 명시적 probe로 점검해야 한다. MCP에는 `doctor`를 노출하지 않는다. | `status`를 local snapshot으로 유지하면서 설치/인증/권한 문제를 진단할 수 있어야 한다. | AC-28 |

### 8.1 REST Sync Scheduler Contract

MVP sync implementation baseline은 다음을 따른다.

- Issues listing은 allowlisted repo마다 `GET /repos/{owner}/{repo}/issues` with `state=all`, `sort=updated`, `direction=asc`, `since=<watermark>`, `per_page=100`를 사용한다.
- GitHub REST Issues endpoints may return pull requests; any response item with `pull_request` is excluded from the MVP corpus.
- Comments backfill uses `GET /repos/{owner}/{repo}/issues/{issue_number}/comments` with `per_page=100`. Incremental comment sync may use issue-specific comments when issue `comments` count or issue `updated_at` changed, and repository comment listing with `since` only when parent issue identity can be resolved.
- All pagination follows `Link` header `rel="next"` until exhausted. qgh does not infer completeness from page counts alone.
- Sync persists ETag/conditional request metadata per endpoint/cursor where available and treats `304 Not Modified` as a successful no-change result.
- Watermarks use GitHub `updated_at` plus an overlap window. MVP default overlap is 60 seconds with idempotent upsert by source identity and version hash.
- Default max in-flight GitHub REST requests is 4 per host, with a hard config cap of 16. qgh must never approach GitHub's documented 100 concurrent request secondary limit.
- Sync uses response rate-limit headers as the primary rate-limit signal. `GET /rate_limit` is reserved for explicit `doctor` or debug output because it can still count against secondary limits.
- On primary rate limit, qgh waits until `x-ratelimit-reset`. On secondary limit, qgh honors `retry-after` when present; otherwise it waits at least one minute and applies bounded exponential backoff.
- Sync output records backoff state, cursor state, fetched/updated/tombstoned counts, skipped PR count, and dirty index task count.
- Full reconciliation is never hidden background work. It is invoked as `qgh sync --reconcile full` and uses bounded rate-limit budget with status-visible last run age and estimated cost class.

## 9. Non-Functional Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| NFR-01 | Default mode는 GitHub host 외 네트워크 egress를 발생시키지 않아야 한다. | private Issues/comments content와 metadata의 third-party 전송을 막는다. | AC-15 |
| NFR-02 | Local DB는 concurrent read와 explicit sync write를 안전하게 처리해야 한다. | CLI/MCP adapter 동시 사용 중 empty/corrupt index가 발생하면 retrieval 신뢰가 깨진다. | AC-16 |
| NFR-03 | Initial sync와 incremental sync는 GitHub primary/secondary rate limits와 retry headers를 존중해야 한다. | backfill 중 차단되거나 agent workflow가 API abuse로 실패하는 일을 줄인다. | AC-17 |
| NFR-04 | Tantivy BM25-only query latency는 10k sources 또는 50k chunks fixture의 warm local profile에서 p95 500ms 이하를 만족해야 한다. cold-start(첫 query) latency는 별도로 측정·기록한다. | agent가 반복 호출할 수 있으므로 local search가 GitHub search proxy보다 느리면 제품 가치가 약하다. | AC-18 |
| NFR-05 | Output schema는 versioned이고 release artifact와 문서가 같은 tool contract를 설명해야 한다. | agent가 stale schema나 unreleased option을 호출하는 문제를 막는다. | AC-11, AC-19 |
| NFR-06 | qgh는 GHES endpoint를 profile 단위로 표현할 수 있어야 한다. (best-effort, release gate 아님) | regulated/private 환경에서는 github.com이 아닌 GHES가 target일 수 있다. | AC-01, AC-20 |
| NFR-07 | Config는 `${XDG_CONFIG_HOME:-~/.config}/qgh`, profile data는 `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`, cache는 `${XDG_CACHE_HOME:-~/.cache}/qgh` 아래에 둔다. Local DB, Tantivy index, log, cache 파일은 single-user 권한(예: 0600/0700)으로 생성해야 한다. | issue/comment에 흔히 포함되는 secret/internal host가 인덱싱되므로 derived data의 로컬 노출을 줄인다. | AC-23 |

## 10. Data/Source Model Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| DSR-01 | Source identity는 GitHub `node_id` 기반 qgh URI이며 URL, title, issue number와 분리해야 한다. REST numeric `id`는 secondary identity로 저장한다. | locator는 transfer, title edit로 바뀔 수 있으므로 identity로 쓰면 wrong source를 반환한다. | AC-05, AC-08 |
| DSR-02 | Source model은 issue와 comment의 parent-child 관계를 표현해야 한다. | comment-only result도 parent issue title/number/repo context가 있어야 citation이 이해된다. | AC-08, AC-09 |
| DSR-03 | Source version은 body hash, GitHub updated timestamp, indexed_at을 포함하고 query/get 결과에 노출해야 한다. | edit 반영 여부와 stale result를 검증하고 agent가 신선도를 판단해야 한다. | AC-03, AC-08, AC-09, AC-10 |
| DSR-04 | Source alias는 current and historical locators를 기록하고 tombstone/reconciliation 상태를 표현해야 한다. | issue transfer, URL 변화 후 기존 search handle을 안전하게 처리해야 한다. | AC-05 |
| DSR-05 | Chunk는 source version에 귀속되고 `get` 가능한 범위만 검색 결과로 노출해야 한다. | orphan chunk나 stale result가 citation 불가능한 result를 만들지 않게 한다. (BM25 chunking에도 적용) | AC-08 |
| DSR-06 | (post-MVP) Vector embedding은 provider/model/dimension/chunker/source schema fingerprint를 가져야 한다. | mixed embeddings와 old chunk schema가 같은 ranking에 섞이는 일을 막는다. | AC-13 |

### 10.1 Storage and Index Baseline

MVP implementation baseline은 다음을 따른다.

- SQLite authoritative tables: `profile_meta`, `repositories`, `source_entities`, `source_versions`, `source_aliases`, `issue_metadata`, `comment_metadata`, `chunks`, `sync_runs`, `sync_cursors`, `tombstones`, `index_generations`, `index_tasks`, `schema_migrations`.
- SQLite stores latest issue/comment title/body once per active source entity. `source_versions` stores lineage, body hash, GitHub updated timestamp, indexed timestamp, and lifecycle metadata; old full bodies are not retained by default to reduce sensitive duplication.
- Tantivy fields: `source_id`, `entity_type`, `repo`, `issue_number`, `state`, `labels`, `author`, `title`, `body`, `parent_issue_title`, `updated_at`, `indexed_at`.
- Sync writes committed SQLite rows first, then records `index_tasks`. Tantivy indexes only committed SQLite source versions.
- Query reads Tantivy candidates, then resolves and filters them through SQLite. Tombstoned, unavailable, or `get`-unresolvable hits are not successful results.
- Full index rebuild uses a shadow Tantivy directory and publishes a new `index_generations` record atomically. `status` exposes active generation and dirty task count.

## 11. CLI and MCP Adapter Interface Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| IR-01 | CLI는 `sync`, `query`(canonical), `get`, `status`, `doctor`를 제공하고 `search`는 `query`의 alias로 둔다. | developer가 setup부터 retrieval까지 terminal에서 검증할 수 있어야 하고 canonical 동사가 모호하지 않아야 한다. | AC-01~AC-10, AC-28 |
| IR-02 | CLI는 machine-readable JSON output과 human-readable output을 구분해야 한다. | shell/agent automation과 사람이 읽는 UX가 서로를 깨지 않게 한다. | AC-11, AC-14 |
| IR-03 | MCP adapter는 `query`, `get`, `status` tools만 expose하고 `readOnlyHint: true` annotation을 가져야 한다. | MCP client가 도구를 안전하게 선택하고 mutation을 기대하지 않게 한다. | AC-12 |
| IR-04 | MCP adapter tool input/output은 `inputSchema`와 `outputSchema`를 갖고 validation 실패는 successful result처럼 보이지 않는 `isError` structured error로 반환해야 한다. | stale tool call, typoed parameter, broad fallback search를 막는다. | AC-11 |
| IR-05 | MCP server stdout은 protocol messages로만 사용하고 diagnostics는 stderr/log channel로 보내야 한다. | JSON-RPC framing 오염은 agent integration을 즉시 깨뜨린다. | AC-21 |
| IR-06 | `query` response는 result ranking을 설명하되 score를 confidence처럼 표시하지 않아야 하고, snippet이 citation 근거가 아니라 `get` 필요 미리보기임을 schema/문서로 명시해야 한다. | ranking score와 answer correctness를 혼동하거나 snippet만으로 답하면 agent가 false positive를 확신할 수 있다. snippet-only 억제는 protocol이 아니라 contract/eval로 enforce되는 advisory임을 분명히 한다. | AC-07, AC-22 |
| IR-07 | `doctor`는 CLI-only command이며 MCP tool list에 포함하지 않는다. `doctor`는 명시적 실행에서만 network/auth probes를 수행하고 같은 JSON envelope를 사용해야 한다. | agent에게 probe surface를 노출하지 않으면서 local install/auth 문제를 진단한다. | AC-12, AC-21, AC-28 |

### 11.1 JSON and Error Contract

Machine-readable output uses one versioned envelope:

```json
{
  "schema_version": "qgh.v1",
  "ok": true,
  "data": {},
  "warnings": [],
  "meta": {
    "profile_id": "work",
    "command": "query"
  }
}
```

Failures use the same envelope with `ok: false` and no partial success data:

```json
{
  "schema_version": "qgh.v1",
  "ok": false,
  "error": {
    "code": "config.no_matching_profile",
    "message": "No configured profile matches the effective repo scope.",
    "details": {"repo": "owner/repo"},
    "hint": "Run qgh with --profile <profile-id> or configure a matching repo allowlist.",
    "retryable": false,
    "exit_code": 2
  },
  "warnings": [],
  "meta": {
    "command": "query"
  }
}
```

Contract rules:

- `query` no-result is success: `ok: true`, `data.results: []`, exit code `0`.
- Validation, config, auth, source-not-found, tombstoned source, storage/index corruption, and rate-limit/backoff are errors with stable namespaced `error.code`.
- Initial code families are `config.*`, `validation.*`, `auth.*`, `github.*`, `sync.*`, `index.*`, `source.*`, `storage.*`, `internal.*`.
- CLI `--json` prints the envelope to stdout for both success and failure; human diagnostics and logs go to stderr and must not include private content.
- CLI human mode prints human-readable output, but exit code and internal error code mapping must match JSON mode.
- Exit code classes: `0` success, `2` validation/config usage, `3` auth/permission, `4` source not found/tombstoned, `5` GitHub rate-limit/backoff, `6` storage/index state, `70` internal error.
- MCP adapter returns the same envelope in structured content. Tool-level errors set `isError: true`; JSON-RPC protocol errors are reserved for malformed protocol messages or server faults.

### 11.2 Config Contract

MVP profile config lives at `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml` and is strict TOML. Unknown keys fail validation.

```toml
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "github_cli"
```

Rules:

- Profile id must match `[a-z0-9][a-z0-9._-]{0,63}`.
- `repos` is a non-empty explicit allowlist of `owner/repo` strings. Wildcards, org discovery, current Git remote inference, and empty repo lists are invalid.
- Token source is a discriminated table. MVP supported types are `github_cli` and `env`.
- `github_cli` does not fallback to `env`; `env` must name exactly one environment variable. `credential_store` is post-MVP and must fail MVP config validation instead of passing as a supported path.
- GitHub.com uses `host = "github.com"`, `api_base_url = "https://api.github.com"`, and `web_base_url = "https://github.com"`. GHES may override API/web base URLs but remains best-effort and outside release gate.
- Profile data path is not configured in MVP. It is derived as `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`.
- Cache path is derived as `${XDG_CACHE_HOME:-~/.cache}/qgh`.

Repo policy config may live at the current git worktree root as `.qgh.toml` and is strict TOML. It is tracked project policy, not personal credential config.

```toml
schema_version = "qgh.repo.v1"

[repo]
github = "owner/repo"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue", "issue_comment"]
labels = []

[query]
limit = 10
```

Repo policy rules:

- qgh finds repo policy from the current git worktree root, not another checkout or the original worktree.
- Repo policy may define repo identity, default query scope, and safe filters only.
- Repo policy must not define profile id, token source, literal token, profile store path, arbitrary DB path, or user-local absolute paths.
- CLI-only top-level `qgh init` may create this file as part of profile/repo bootstrap; `qgh init repo` may create only this file at the current git worktree root from `--repo owner/repo` or a supported GitHub/GHES `origin` remote. Neither command is exposed to MCP.
- Default `scope = "repo"` means the repository's Issues and issue comments, not one current issue.
- Issue number remains an explicit hard filter such as `--issue <number>`.
- Explicit CLI args override repo policy defaults, but cannot select a repo outside the resolved profile allowlist.
- If no `--profile`/`QGH_PROFILE` is provided, qgh may auto-select a profile only when effective repo scope exists and exactly one profile allowlists that repo scope.
- Zero matching profiles fail with `config.no_matching_profile`; multiple matching profiles fail with `config.ambiguous_profile`.
## 12. Privacy and Security Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| PSR-01 | Default configuration은 hosted embedding, hosted rerank, telemetry upload, shared server를 비활성화해야 한다. | private repo content와 derived data의 third-party egress를 기본적으로 막는다. | AC-15 |
| PSR-02 | Hosted provider는 future option으로만 남기고 repo/profile policy와 explicit opt-in 없이는 사용할 수 없어야 한다. opt-in 없이는 코드 경로가 활성화되지 않아야 한다. | 보안 검토 없이 “품질 개선”이 private data transfer로 바뀌는 것을 막는다. | AC-15 |
| PSR-03 | Local DB, snippets, embeddings, logs는 sensitive derivative data로 취급해야 한다. | vector/snippet도 원문 재구성이 어렵더라도 private content를 반영한다. | AC-15, AC-23 |
| PSR-04 | Token은 qgh config에 평문 저장하지 않고 MVP에서는 `github_cli` 또는 `env` explicit source reference로 다뤄야 한다. `credential_store`와 GitHub App token은 post-MVP/server capability다. | local config leak와 wrong-account fallback이 private repo access leak로 이어지지 않아야 한다. | AC-24, AC-26 |
| PSR-05 | MCP v1은 read-only이고 GitHub write permission 없이 사용할 수 있어야 한다. | 최소 권한으로 agent integration을 가능하게 한다. | AC-12, AC-24 |

## 13. Search Quality Requirements

| ID | Requirement | 왜 필요한가 | Acceptance |
|---|---|---|---|
| SQR-01 | BM25 baseline은 issue number/URL/title exact lookup, keyword query, repo/label/state filters를 평가해야 한다. | exact lookup은 semantic ranking보다 우선해야 하는 core workflow다. | AC-06, AC-07, AC-25 |
| SQR-02 | 한국어/영어 mixed corpus tokenization은 Tantivy tokenizer baseline과 CJK n-gram fallback field를 시작점으로 두고 AC-25 eval로 적합성을 검증해야 한다. eval 미달 시 형태소 tokenizer를 fallback tier로 둔다. | 기본 tokenization만으로 CJK recall이 부족할 수 있고, n-gram도 적합성이 검증 대상이다. | AC-25 |
| SQR-03 | Curated eval은 issue body, comment-only answer, exact lookup, CJK/mixed, negative query class를 포함해야 한다. | qgh의 차별점인 comments coverage와 false positive abstention을 직접 검증해야 한다. | AC-25 |
| SQR-04 | Search result는 lexical/(post-MVP) vector/rerank/final signal을 typed field로 분리해야 한다. | score를 confidence로 오해하지 않고 ranking provenance를 debugging할 수 있어야 한다. | AC-22 |
| SQR-05 | 모든 top-k eval result는 `get` round-trip을 통과해야 한다 (100% hard gate). | 검색 품질은 원문 조회 가능성까지 포함해야 한다. | AC-08, AC-25 |
| SQR-06 | Eval은 query마다 gold source set(`source_id` list)을 라벨링한 ground truth를 가지며, 라벨러·라벨 기준·제외(ambiguous) 규칙을 문서화해야 한다. 20~30 query는 statistical guarantee가 아니라 directional gate다. | 수치 target이 의미를 가지려면 “정답”의 정의와 한계가 명시돼야 한다. | AC-25 |

### 13.1 Search-quality numeric targets (initial MVP gate, first eval 후 PRD/ADR로만 recalibrate)

| Query class | Metric | MVP target |
|---|---|---|
| exact lookup (issue number/URL/title) | top-1 hit rate | ≥ 0.95 |
| keyword/body/comment semantic | top-5 hit rate | ≥ 0.80 |
| CJK/mixed (n-gram fallback baseline) | top-5 hit rate | ≥ 0.70 |
| negative query | abstention rate (no confident false-positive in top result) | ≥ 0.80 |
| 전체 top-k | `get` round-trip success | 1.00 (hard) |

## 14. Acceptance Criteria

| ID | Criteria | Covered requirements |
|---|---|---|
| AC-01 | 새 profile 생성 시 GitHub host, repo allowlist, token source, derived XDG profile data path가 모두 검증되어야 하며 malformed repo, missing token source, matching profile 없음, ambiguous profile은 non-zero exit와 structured error를 낸다. Explicit `--profile` 없이도 effective repo scope를 allowlist에 포함하는 profile이 정확히 하나면 그 profile로 resolve된다. | FR-01, NFR-06, IR-01 |
| AC-02 | allowlisted repo 1개 이상에서 Issues title/body/metadata와 issue comments를 최초 backfill하고 source count가 `status`에 표시된다. | FR-02, FR-03 |
| AC-03 | issue body edit와 comment edit가 다음 sync 후 query/get 결과에 반영되고 old version 상태가 확인 가능하다. | FR-02, FR-03, FR-05, DSR-03 |
| AC-04 | MVP CLI/MCP adapter/config/schema에는 Wiki sync/search/get/status surface가 없고, Wiki 관련 unsupported parameter는 structured validation error를 낸다. | FR-04, FR-11 |
| AC-05 | deleted comment, transferred or unavailable issue를 fixture로 재현했을 때 reconciliation 전략(FR-05)으로 stale ghost result가 active search에 남지 않고 tombstone/reconciliation 상태가 표시된다. | FR-05, DSR-01, DSR-04 |
| AC-06 | local model cache, GPU/runtime, vector extension이 없는 환경에서도 `sync`, Tantivy BM25 `query`, `get`, `status`가 통과한다. | FR-06, SQR-01 |
| AC-07 | worktree root `.qgh.toml`의 repo policy가 기본 repo scope와 safe filters를 설정하고, issue number, full URL, repo/label/state/author filters는 exact/hard filter로 동작하며 semantic rewrite 또는 query expansion이 filter를 넓히지 않는다. Explicit `--repo`는 repo policy 기본값을 override하지만 resolved profile allowlist 밖 repo는 structured error로 실패한다. | FR-07, IR-06, SQR-01 |
| AC-08 | top-k query results는 모두 qgh URI `source_id`, `entity_type`, canonical URL, snippet, `get_args`, parent context, `indexed_at`/source version을 포함하고 `get` round-trip에 성공한다. | FR-08, FR-09, DSR-01, DSR-02, DSR-03, DSR-05, SQR-05 |
| AC-09 | `get`은 issue body와 comment 각각에서 authoritative body, canonical GitHub URL, source version(body hash, GitHub updated timestamp, indexed_at)을 반환한다. | FR-09, DSR-02, DSR-03 |
| AC-10 | `status`는 resolved profile, profile source, effective repo scope, repo policy path, source count, stale/tombstone count, DB/schema version, profile paths, Tantivy active generation, dirty index task count, reconciliation age를 network/model load 없이 표시하고, vector mode 활성 시 missing embedding count를 추가 표시한다. | FR-10, DSR-03 |
| AC-11 | profile config, repo policy config, CLI/MCP adapter unknown keys, typoed params, malformed JSON, invalid enum은 silent fallback 없이 structured validation error를 낸다. Repo policy가 credential, token source, profile store path, arbitrary DB path, or user-local absolute path를 정의하면 structured validation error를 낸다. | FR-11, FR-14, NFR-05, IR-02, IR-04 |
| AC-12 | MCP tool 목록은 read-only `query`, `get`, `status`로 제한되고 각 tool은 CLI JSON/local retrieval contract의 thin adapter로 `readOnlyHint: true`, `inputSchema`, `outputSchema`를 노출하며 mutation/sync/embed/write tool이 없다. MCP launch는 `--profile`, `QGH_PROFILE`, repo-scope single-match profile precedence를 사용하고, repo policy 기본 scope와 explicit `repo` argument allowlist 검사를 CLI와 같은 structured envelope로 반환한다. | FR-01, FR-12, IR-03, PSR-05 |
| AC-13 | (post-MVP) optional vector mode는 fingerprint mismatch와 partial embedding coverage를 detect하고 BM25-only search를 중단시키지 않는다. MVP release gate 아님. | FR-13, DSR-05, DSR-06 |
| AC-14 | no result, validation error, auth error, rate-limit/backoff, stale index warning은 JSON schema에서 서로 다른 상태로 표현된다. | FR-14, IR-02 |
| AC-15 | default private repo sync/search 중 mocked network에서 GitHub host 외 outbound call이 발생하지 않고, hosted provider 경로는 explicit opt-in 없이는 비활성이다. | NFR-01, PSR-01, PSR-02, PSR-03 |
| AC-16 | CLI sync와 MCP query/status가 동시에 실행되는 fixture에서 DB corruption, empty Tantivy publish, partial schema migration이 발생하지 않는다. | NFR-02 |
| AC-17 | sync scheduler는 GitHub rate-limit headers, retry-after, secondary-limit response를 존중하고 bounded backoff 상태를 `status` 또는 sync output에 표시한다. | NFR-03 |
| AC-18 | Tantivy BM25-only query는 10k sources 또는 50k chunks fixture의 warm local profile에서 p95 500ms 이하를 통과하고, cold-start latency가 측정·기록된다. | NFR-04 |
| AC-19 | release artifact의 schema version, generated MCP schema, CLI help, PRD-derived acceptance snapshot이 서로 같은 tool contract를 설명한다. | NFR-05 |
| AC-20 | (best-effort, release gate 아님) GitHub.com과 GHES-style API base URL profile이 모두 validation되고, GHES rate-limit 차이는 profile capability/status로 노출된다. | NFR-06 |
| AC-21 | MCP stdio mode에서 stdout에 protocol message 외 diagnostics가 섞이지 않는다. | IR-05 |
| AC-22 | query response는 lexical/vector/rerank/final ordering signal을 분리하고 final score를 confidence 또는 probability로 명명하지 않는다. | IR-06, SQR-04 |
| AC-23 | local DB, log, cache 파일이 single-user 권한으로 생성되고 경로가 `status`에 표시되며 sensitive data 포함 가능성이 문서화된다. | PSR-03, NFR-07 |
| AC-24 | 최소 권한 read token으로 sync/query/get/status가 작동하고 write permission 없이 MCP workflow가 통과한다. | PSR-04, PSR-05 |
| AC-25 | 20~30개 curated query set에서 issue body, comment-only answer, exact lookup, CJK/mixed, negative query class별로 §13.1 numeric target을 충족하고, gold source set과 라벨링 규칙이 문서화되며 모든 top-k가 `get` round-trip을 통과한다. | SQR-01, SQR-02, SQR-03, SQR-05, SQR-06 |
| AC-26 | qgh config 파일/저장소 어디에도 literal token 문자열이 기록되지 않고 source reference만 저장됨이 검증된다. | FR-15, PSR-04 |
| AC-27 | reconciliation job(FR-05)이 bounded rate-limit 예산 안에서 deleted comment와 transferred/unavailable issue를 tombstone하고, manual/configured cadence, last run age, estimated cost class가 `status`/문서에 표시된다. | FR-05, NFR-03 |
| AC-28 | CLI `doctor --profile <id>`는 config/profile, file permissions, SQLite/Tantivy consistency, GitHub auth/reachability, and rate-limit headers를 versioned envelope로 보고하고, MCP tool 목록에는 `doctor`가 없다. | FR-16, IR-07 |

## 15. Validation and Research Plan

1. Primary-source validation: GitHub REST Issues/comments/search/rate-limit docs, MCP 2025-11-25 tools/schema docs, Tantivy docs, SQLite docs를 release 전 재확인한다. (vector 관련 sqlite-vec 등은 M6에서 확인.)
2. Adjacent-tool validation: GitHub official MCP server, github-to-sqlite, qmd, mcp-local-rag, Sourcegraph-style code search와 qgh의 scope 차이를 README/positioning에 반영한다. positioning은 native-search 대비(comment coverage·rate-limit-free query·citation·offline)와 third-party 대비(privacy)를 분리 기술한다.
3. Sync correctness fixtures: fake GitHub REST server로 pagination, since window, issue/comment edit, comment delete, issue transfer/unavailable, ETag/304, rate-limit retry, 삭제 감지 reconciliation을 테스트한다.
4. Retrieval contract tests: 모든 query result가 `get` round-trip을 통과하고 parent context/canonical URL/source version을 포함하는지 검증한다.
5. Privacy test: default mode에서 GitHub host 외 outbound call이 없는지 mocked network로 검증하고, DB/log 파일 권한을 확인한다.
6. Search eval: gold source set을 라벨링한 curated 20~30 queries를 query class별로 나누고 §13.1 numeric target(top-1/top-5 hit, abstention, get round-trip)을 측정한다. directional gate임을 명시한다.
7. Agent workflow validation: CLI `--json`과 MCP adapter fixture에서 `query -> get -> cite` workflow를 반복 실행하고, snippet-only answer를 유도하지 않는지(advisory contract) 확인한다.
8. User validation: 개발자 3~5명에게 자신의 repo 또는 fixture repo에서 “과거 결정 찾기”, “comment에만 있는 정보 찾기”, “삭제된 comment가 검색되지 않는지 확인하기” task를 수행하게 하고 실패 원인을 기록한다.

## 16. Risks and Mitigations

| Risk | 제품 영향 | Mitigation |
|---|---|---|
| GitHub native search와 차별점이 약해 보임 | 사용자가 “GitHub가 이미 검색을 제공한다”고 판단할 수 있다. | issue comments coverage, rate-limit-free 반복 query, 결정적 `get` citation, offline 동작을 positioning 중심에 둔다. privacy는 third-party 도구 대비로만 주장하고 native search 대비로는 주장하지 않는다. |
| Stale or ghost result | 삭제된 private content가 검색되거나 잘못된 source를 citation한다. | tombstone, last-seen, source version, FR-05 삭제 감지 전략, periodic reconciliation을 MVP requirement로 둔다. |
| 삭제 감지 비용 vs rate limit | full reconciliation이 rate-limit 예산을 소모해 NFR-03/반복-sync 회피 목표와 충돌한다. | count/updated_at 기반 incremental diff 우선, full reconciliation은 bounded cadence·manual default, 비용을 status에 노출한다(AC-27). |
| Source identity drift | URL/title/number 변경 후 다른 원문을 반환한다. | source identity와 locator/alias를 분리한다. |
| GitHub API secondary rate limit | backfill이 실패하거나 token이 일시 차단된다. | low-concurrency scheduler, retry-after 준수, conditional requests, bounded backoff status를 둔다. |
| Wiki 연기 때문에 source coverage가 좁아 보임 | runbook이나 architecture note를 Wiki에 두는 repo에서는 MVP가 모든 decision context를 찾지 못할 수 있다. | MVP positioning을 Issues/comments에 고정하고, Wiki는 post-MVP source connector로 별도 검증한다. |
| Hosted provider privacy pressure | 품질 개선 요구가 private data egress로 이어진다. | BM25-only default와 explicit opt-in policy 없이는 hosted provider 코드 경로를 비활성화한다. |
| Agent misuse / snippet-only 답변 | agent가 typoed param, broad query, mutation tool로 위험 행동을 하거나 snippet만으로 답한다. | strict schema, read-only MCP, no sync/write tools, structured errors. snippet-only 억제는 protocol이 아니라 contract/eval로 enforce하는 advisory임을 한계로 명시한다. |
| BM25 CJK quality 부족 | 한국어/영어 mixed repo에서 recall이 낮다. | Tantivy tokenizer baseline + CJK n-gram fallback field + §13.1 CJK numeric target, 미달 시 형태소 tokenizer fallback tier. |
| Vector optional complexity | fingerprint mismatch나 partial embeddings가 ranking을 오염한다. | vector를 §6.1 post-MVP로 연기하고 fingerprint/partial coverage gate를 M6 결정 사항으로 둔다. |
| SQLite local DB corruption/migration race | local trust가 깨지고 agent workflow가 불안정해진다. | WAL, busy timeout, single-writer queue, migration lock, shadow publish를 implementation plan gate로 둔다. |
| Sensitive content 로컬 노출 | issue/comment의 secret이 평문 DB/log로 노출된다. | DB/log/cache single-user 권한(NFR-07), sensitive 취급 문서화, token 평문 미저장(FR-15). |
| Scope creep | shared server, Web UI, PR/Discussions, vector를 MVP로 끌어와 지연한다. | Non-Goals와 §6.1 deferral을 release gate로 사용하고 MVP acceptance 전 확장 설계를 금지한다. |
| GHES variance | endpoint와 rate limit policy가 github.com과 다를 수 있다. | GHES를 best-effort profile capability로 표현하고 release gate에서 제외한다(§17 Q1). |

## 17. Resolved Planning Decisions

이 PRD는 MVP planning에 필요한 제품 결정을 닫은 상태다. 남은 것은 구현 중 검증할 empirical uncertainty다.

1. MVP first-class target은 GitHub.com이다. GHES는 best-effort profile capability이며 release gate(AC-20)에서 제외한다.
2. Token source default는 `github_cli`다. 단, profile에는 explicit source reference가 저장되어야 하며 runtime fallback은 금지한다. MVP supported source는 `github_cli`와 `env`뿐이다. `credential_store`와 GitHub App token은 post-MVP/server capability다.
3. 초기 curated eval corpus는 synthetic fixture repo로 시작한다. 실제 private repo 익명화 corpus는 user validation 보조 자료이며 release gate가 아니다. Gold source set은 fixture source ids로 라벨링하고 ambiguous query는 제외 규칙을 문서화한다.
4. CJK baseline은 Tantivy tokenizer baseline + CJK n-gram fallback field다. 형태소 tokenizer는 CJK/mixed target 미달 또는 false-positive 분석에서 n-gram 한계가 확인될 때 optional tier로 올린다.
5. Full reconciliation은 hidden background work가 아니다. 일반 `sync`는 cheap lifecycle checks를 수행하고, full reconciliation은 explicit `qgh sync --reconcile full`로 시작한다. Optional `reconcile_after_days`는 자동 실행보다 status warning을 우선한다.
6. Wiki source connector는 post-MVP다. MVP CLI/MCP adapter/config/schema에는 Wiki sync/search/get/status surface를 넣지 않는다.
7. Optional vector path는 M6에서 local ONNX embedding runtime + SQLite vector index를 first candidate로 검토한다. Hosted embedding/rerank는 explicit policy/opt-in 전에는 검토하지 않는다.
8. 서버형 제품화 gate는 local MVP quality gate 통과, 3~5명 user validation에서 repeated workflow 가치 확인, shared index/ACL 요구가 local duplicate-index 비용보다 크다는 evidence, GitHub App/token 운영 모델 정의다.
9. `doctor`는 MVP CLI-only diagnostic이다. `status`는 local-only snapshot, `doctor`는 opt-in probe다. MCP v1에는 `doctor`와 `sync`를 노출하지 않는다.
10. `eval`은 MVP release/test harness로 유지하고 user-facing CLI/MCP adapter command로 노출하지 않는다. 추후 public eval UX는 search-quality workflow가 반복 사용될 때 별도 판단한다.
11. Repo policy는 current git worktree root의 tracked `.qgh.toml`에서 읽고 기본 repo scope와 safe filters만 정의한다. Profile은 explicit `--profile`/`QGH_PROFILE`이 우선이며, 없을 때는 effective repo scope를 allowlist에 포함하는 configured profile이 정확히 하나일 때만 자동 선택한다. Matching profile 0개 또는 2개 이상은 structured error다.

## 18. Milestones / Next Steps

MVP release gate = AC-01~AC-27, 단 AC-13(vector)·AC-20(GHES)은 제외.

| Milestone | Deliverable | Exit criteria |
|---|---|---|
| M0 PRD lock | 이 PRD, §13.1 numeric target, acceptance criteria, §17 decisions 확정 | Non-Goals, §6.1 deferral, numeric target, GHES/token/eval/reconciliation decisions가 implementation-ready |
| M1 Contract design | profile, repo policy, effective scope, source model, CLI args, JSON schemas, local store/search behavior, MCP adapter schema, token source policy, doctor contract | FR/DSR/IR requirements가 schema tests로 전환 가능 |
| M2 Sync vertical slice | REST Issues/comments fixture sync + 삭제 감지 reconciliation | AC-02~AC-05, AC-27 fixture 통과 |
| M3 BM25 retrieval slice | BM25-only `query -> get -> cite -> status` | AC-06~AC-11 통과 |
| M4 MCP read-only adapter slice | MCP `query`, `get`, `status` mirror the CLI JSON/local retrieval contract | AC-12, AC-21, agent workflow fixture 통과 |
| M5 Search quality gate | gold-labeled curated 20~30 query eval | §13.1 numeric target, AC-22, AC-25 통과 및 top failure 분석 |
| M6 Optional vector decision | local ONNX + SQLite vector prototype or deferral | AC-13 충족 또는 vector MVP 제외 확정 (release gate 아님) |
| M7 MVP release candidate | docs, schema, privacy(NFR-07), status, packaging check | release-gate AC 검증 결과와 residual risks 문서화 |

## 19. Technology Stack Baseline

MVP Rust stack baseline:

| Layer | Baseline | Decision rule |
|---|---|---|
| Language/runtime | Rust stable, single binary | No Node/Python runtime dependency for MVP execution. |
| CLI | `clap` | Generated help must match JSON/schema docs. |
| Config/JSON schema | `serde`, `toml`, `serde_json`, `schemars` | `deny_unknown_fields` style strict decoding; generated schema snapshots are release artifacts. |
| GitHub REST client | `reqwest` with rustls TLS + typed qgh structs | Prefer direct REST calls over broad GitHub SDK abstraction so rate-limit headers, ETags, pagination, `pull_request` filtering, and source identity fields stay explicit. |
| Async runtime | `tokio` | Used for GitHub sync and MCP stdio; SQLite writes remain controlled by a single-writer path. |
| SQLite | `rusqlite` with bundled SQLite | Bundled local store avoids system SQLite drift and extension install friction. |
| Search | `tantivy` | Derived BM25 index with shadow generation and atomic publish. |
| MCP | official MCP Rust SDK first | CLI args, JSON schema snapshots, and local store/search behavior remain source of truth; if SDK behavior lags the target spec, isolate the compatibility adapter instead of changing qgh product contracts. |

## Sources and References

Local source documents:

- `qgh-product-brief.md`
- `qgh-mvp-evidence-decision-summary.md`
- `github-issues-wiki-hybrid-search-go-no-go.md`

Primary/current references checked for this PRD:

- GitHub REST Issues API: https://docs.github.com/en/rest/issues/issues
- GitHub REST Issue comments API: https://docs.github.com/en/rest/issues/comments
- GitHub REST Search API: https://docs.github.com/en/rest/search/search
- GitHub REST rate limits: https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
- GitHub REST best practices: https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
- GitHub pagination: https://docs.github.com/en/rest/using-the-rest-api/using-pagination-in-the-rest-api
- GitHub issue semantic/hybrid search GA changelog: https://github.blog/changelog/2026-04-02-improved-search-for-github-issues-is-now-generally-available/
- GitHub MCP server: https://github.com/github/github-mcp-server
- MCP 2025-11-25 tools specification: https://modelcontextprotocol.io/specification/2025-11-25/server/tools
- MCP 2025-11-25 schema: https://modelcontextprotocol.io/specification/2025-11-25/schema
- MCP SDKs: https://modelcontextprotocol.io/docs/sdk
- MCP Rust SDK: https://github.com/modelcontextprotocol/rust-sdk
- Tantivy: https://docs.rs/tantivy/
- SQLite: https://www.sqlite.org/docs.html
- rusqlite: https://github.com/rusqlite/rusqlite
- reqwest: https://github.com/seanmonstar/reqwest
- sqlite-vec (post-MVP vector 검토용): https://github.com/asg017/sqlite-vec

Adjacent tools:

- dogsheep/github-to-sqlite: https://github.com/dogsheep/github-to-sqlite
- qmd: https://github.com/tobi/qmd
- mcp-local-rag: https://github.com/shinpr/mcp-local-rag
- Sourcegraph Code Search: https://sourcegraph.com/code-search

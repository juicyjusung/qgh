# qgh Product Brief

문서 역할: canonical product brief.
게시본: GitHub issue #1 (`https://github.com/juicyjusung/qgh/issues/1`)은 이 파일의 mirror다.
업데이트 규칙: 제품 포지셔닝, 사용자, scope, success criteria가 바뀌면 이 파일을 먼저 수정하고 GitHub issue #1 body를 동기화한다.

작성일: 2026-06-27 KST
개정: 2026-06-27 — 딥리서치 반영 (GitHub native semantic search GA, MCP 2025-11-25, FTS5 한국어 토크나이저, sqlite-vec 상태, 인접 도구, rate-limit 리스크)
개정: 2026-06-27 — PRD grill lock 반영 (vector post-MVP, token fallback 금지, reconciliation manual default, native search 대비 privacy positioning 정정)
개정: 2026-06-28 — 문서 역할 정리 (canonical product brief와 GitHub issue mirror 관계 명시)
개정: 2026-06-29 — MVP scope reset (GitHub Issues와 issue comments만 포함, Wiki는 post-MVP)
개정: 2026-06-29 — architecture lock (Rust single-binary CLI, XDG profile store, bundled SQLite authoritative store, Tantivy derived BM25 index)
개정: 2026-06-29 — grill closure (CLI-only doctor, REST sync scheduler contract, user-facing eval deferral)
개정: 2026-06-29 — config policy (worktree-root repo policy for query repo scope defaults)
개정: 2026-06-29 — profile resolution (CLI/env/single-match profile precedence)
개정: 2026-06-29 — effective scope metadata (CLI meta, status, doctor diagnostics)
개정: 2026-06-29 — MCP scope resolution (read-only tools share CLI repo policy/profile contract)

## 1. 제품 정의

qgh는 **GitHub Issues와 issue comments용 local-first read-only CLI/MCP 검색 도구**다.

제품의 핵심은 GitHub Issues와 issue comments를 로컬 인덱스로 동기화한 뒤, 개발자와 agent가 CLI 또는 MCP에서 빠르게 검색하고 원문으로 되돌아갈 수 있게 하는 것이다. 첫 버전은 GitHub를 대체하는 검색 플랫폼이 아니라, private repo의 issue thread 문맥을 외부로 보내지 않고 로컬에서 검색 가능한 source index를 만드는 도구다.

qgh가 제공해야 하는 기본 경험은 단순하다.

1. 사용자가 명시한 repo만 로컬에 동기화한다.
2. Issues와 comments 최신 내용을 읽기 전용으로 인덱싱한다.
3. `search/query`는 관련 source 후보를 찾는다.
4. `get`은 canonical GitHub URL과 원문 식별자를 포함한 authoritative source를 돌려준다.
5. MCP는 agent가 같은 workflow를 안전하게 반복할 수 있게 한다.

## 2. 제품 문제와 시장 맥락

많은 프로젝트에서 GitHub Issues와 issue comments는 실제 의사결정, 장애 대응, 작업 맥락, 운영 지식의 SSOT에 가깝다. 그러나 시간이 지나면 다음 문제가 반복된다.

- GitHub은 2026-04 issue semantic/hybrid search를 GA했고 공식 MCP server로도 (`semantic_issues_search`) 노출하지만, (a) issue **title/body만** index해 comments를 빼고, (b) semantic/hybrid 쿼리가 **분당 10회**로 throttle되며, (c) cloud에서 private content를 서버 처리하고, (d) comment 단위의 deterministic `query -> get -> cite` citation을 보장하지 않는다.
- issue title, body, comments, label, 과거 결정이 분산되어 있어 필요한 맥락을 찾는 데 오래 걸린다.
- private repo 내용은 외부 검색/embedding provider로 보내기 어렵다.
- agent가 GitHub 검색 결과 snippet만 보고 답을 만들면 원문 확인과 citation이 약해진다.
- 조직 전체 검색 플랫폼을 먼저 만들면 ACL, 운영, 토큰 관리, stale index 문제가 제품 검증보다 먼저 커진다.

따라서 초기 제품 문제는 “GitHub 지식을 모두 검색하는 플랫폼”이 아니라, **개발자와 agent가 접근 권한이 있는 repo의 Issues와 issue comments를 로컬에서 안정적으로 찾고 원문으로 검증하는 것**이다.

### 2.1 GitHub native search 대비 (왜 여전히 local이 필요한가)

GitHub native semantic search의 GA는 "GitHub 검색이 약하다"는 단순 전제를 약화시킨다. qgh의 정당성은 그 대신 **GitHub index가 빼먹는 corpus와 운영 제약**에 있다.

| 축 | GitHub native semantic search | qgh |
|---|---|---|
| 대상 | issue title/body | issue body + **comments** |
| Wiki | MVP 범위 밖 | post-MVP source connector 후보 |
| rate limit | semantic/hybrid 분당 10회 | per-query 제한 없음, offline |
| privacy | 데이터가 이미 GitHub에 있으므로 native search 대비 핵심 차별점은 아님 | third-party hosted RAG/embedding 대비 local-first, 외부 egress 없음 |
| scope | top-100 repo | 명시 repo allowlist 임의 |
| citation | ranked issue 반환 | issue/comment 단위 canonical URL |
| GHES/air-gap | cloud-first (GHES 지원 미확인) | local 동작 |

brief는 "GitHub 검색이 약하다"가 아니라 **comment coverage + offline + rate-limit-free + deterministic citation + GHES/규제 환경**을 정당성으로 명시한다. Privacy는 GitHub native search 대비 주장이 아니라 third-party hosted RAG/embedding 대비 주장이다. "GitHub가 이미 semantic issue search를 주는데 왜 local index인가"는 PRD에서 가장 먼저 받을 질문이므로 FAQ로 선제 대응한다.

### 2.2 인접 도구와 차별화

- **GitHub 공식 MCP server**: live API proxy + cloud semantic issue search. local index/offline/comment corpus 없음.
- **dogsheep/github-to-sqlite**: issues→local SQLite sync (최근접 prior art). qgh식 MCP citation contract와 comment-first retrieval UX 없음.
- **tobi/qmd**: 아키텍처 sibling. local markdown 대상, `get`/`multi_get` round-trip 있음. GitHub source connector 없음.
- **shinpr/mcp-local-rag**: local-first + MCP + hybrid + chunk retrieval 보유. GitHub connector만 없음.

개별 재료(local-first, MCP, hybrid search, chunk-get)는 2026 기준 모두 commoditized다. qgh의 moat는 **Issues+comments source connector + deterministic canonical-URL citation의 교집합**이며, 카테고리 신규성이 아니라 이 niche 소유로 포지셔닝한다. github-to-sqlite + qmd-style MCP 조합으로 모방 가능하므로 통합 UX와 citation 계약의 완성도가 방어선이다.

## 3. 사용자

### Primary users

- **개발자**: 과거 이슈, 장애 대응, 결정 배경을 빠르게 찾아 현재 작업에 연결하고 싶다.
- **AI coding agent 사용자**: agent가 private repo의 Issues/comments 맥락을 검색하되, 원문 조회와 citation이 가능한 형태로 쓰게 하고 싶다.
- **기술 리드/maintainer**: repo에 누적된 논의와 운영 지식을 별도 서버 구축 없이 개인 로컬 환경에서 재사용하고 싶다.

### Secondary users

- **소규모 팀**: shared server를 만들기 전, repo별 검색 품질과 workflow 가치를 검증하고 싶다.
- **보안/플랫폼 담당자**: private content가 기본적으로 로컬 밖으로 나가지 않는 검색 도구를 선호한다.

## 4. MVP Scope

MVP는 작은 범위를 강하게 검증한다.

### 포함

- 명시적 repo allowlist
- Rust single-binary CLI/MCP
- XDG config/data/cache 기반 profile store (`--profile`/`QGH_PROFILE` 우선, repo scope single-match profile resolution 허용, data path override 없음)
- current git worktree root의 tracked `.qgh.toml` repo policy로 CLI/MCP query 기본 repo scope와 safe filters 제공
- GitHub Issues title/body metadata sync
- issue comments sync
- bundled SQLite authoritative store
- Tantivy derived BM25 search index (shadow build + atomic publish)
- Rust crate baseline: `clap`, `serde`/`toml`/`schemars`, `reqwest`+rustls, `tokio`, `rusqlite` bundled, `tantivy`, official MCP Rust SDK 우선
- BM25/keyword 검색 기본 동작 (한국어/영어 mixed corpus는 Tantivy tokenizer baseline + CJK n-gram fallback field를 eval로 검증)
- optional vector/hybrid search는 post-MVP capability (sqlite-vec/ONNX 후보, MVP release gate 아님)
- CLI 명령: `sync`, `search` 또는 `query`, `get`, `status`, `doctor`
- MCP tools: `query`, `get`, `status` (MCP 2025-11-25: structured output `outputSchema`, `readOnlyHint: true`, validation/resolution 실패는 `isError`)
- 검색 결과의 stable source id, entity type, canonical URL, `get` 호출 정보
- versioned JSON output envelope (`data`, `error`, `warnings`, `meta`)와 stable namespaced error code
- CLI JSON envelope와 MCP structuredContent `meta`, `status`/`doctor` diagnostics에 resolved profile/effective repo scope 표시
- read-only 동작
- local-first privacy default
- strict config/CLI/MCP schema validation
- stale/missing index 상태를 보여주는 status

### MVP에서 반드시 지킬 제약

- repo allowlist는 profile config에 명시해야 한다. CLI query/search는 explicit `--repo`가 없을 때 current git worktree root의 repo policy로 기본 repo scope를 정할 수 있지만 profile allowlist 밖으로 넓힐 수 없다.
- profile resolution은 CLI `--profile` > `QGH_PROFILE` > repo scope single-match 순서다. Repo scope 없이 configured profile 개수만 보고 profile을 고르지 않는다.
- hosted embedding/rerank는 기본값이 아니다.
- vector 기능이 없어도 sync/query/get/status는 작동해야 한다.
- `status`는 local-only snapshot이고, network/auth/schema probe는 CLI-only `doctor`에서만 수행한다.
- 검색 결과 score를 confidence처럼 노출하지 않는다.
- source identity는 GitHub `node_id` 기반 qgh URI이며 URL, title, issue number와 분리한다.
- `get`으로 원문을 다시 조회할 수 없는 검색 결과는 성공으로 보지 않는다.

## 5. Non-Goals

MVP에서 제외할 항목은 제품 집중도를 지키기 위한 의도적 결정이다.

- organization-wide repo auto-discovery
- shared server 또는 central index
- full ACL enforcement
- GitHub write-back, issue 생성/수정, label 변경
- PR, Discussions, Projects, Actions log 통합
- Web UI
- webhook 기반 실시간 동기화
- hosted embedding/rerank default
- OpenSearch/Meilisearch/Typesense 등 별도 검색 서버
- fine-tuned ranking/query expansion
- GitHub Wiki sync/search/get/status
- 모든 issue timeline event indexing
- user-facing `eval` command 또는 MCP eval tool

## 6. 핵심 Workflow

### 6.1 초기 설정

사용자는 검색하고 싶은 repo를 `~/.config/qgh/config.toml`의 strict TOML profile에 명시한다. qgh는 token source, GitHub host, repo allowlist를 하나의 profile로 고정하고, SQLite/Tantivy data path는 XDG data dir과 profile id에서 파생한다. Repository는 tracked `.qgh.toml`로 query/search의 기본 repo scope와 safe filters를 정의할 수 있지만 profile id, token source, literal token, profile store path, arbitrary DB path, user-local absolute path는 정의할 수 없다. CLI `--profile`과 `QGH_PROFILE`이 없으면 effective repo scope를 allowlist에 포함하는 profile이 정확히 하나일 때만 profile을 자동 선택한다.

성공 경험:

- 어떤 repo가 인덱싱되는지 명확하고, repo worktree에서 query/search 기본 scope가 profile allowlist 안의 current repo로 제한된다.
- CLI와 MCP가 같은 profile, 같은 SQLite store, 같은 Tantivy index를 본다.
- 잘못된 repo, 누락된 token, unknown config key, no matching profile, ambiguous profile은 조용히 무시되지 않고 실패한다.

### 6.2 동기화

사용자는 `sync`를 실행해 Issues와 comments를 로컬로 가져온다. qgh는 변경된 source를 반영하고 삭제/이동 가능성이 있는 source를 추적한다.

성공 경험:

- 최초 backfill이 끝나면 search 가능한 상태가 된다.
- 이후 sync는 edit/update를 반영한다.
- status에서 마지막 sync, stale source, reconciliation age를 볼 수 있다. vector mode가 later 활성화되면 missing embedding 상태도 표시한다.

### 6.2.1 진단

사용자는 `doctor --profile <id>`를 명시적으로 실행해 config/profile, 파일 권한, SQLite/Tantivy consistency, GitHub auth/reachability, rate-limit headers를 점검한다. `doctor`는 MCP tool이 아니며, `status`는 네트워크를 건드리지 않는 local snapshot으로 유지한다.

### 6.3 검색

사용자 또는 agent는 자연어, keyword, issue number, repo/label/state 같은 filter로 검색한다. current git worktree root에 repo policy가 있으면 CLI query/search 기본 repo scope는 해당 repository의 Issues/comments다. Explicit `--repo`는 repo policy 기본값을 override할 수 있지만 profile allowlist 밖 repo는 structured error로 실패한다. 결과는 답변이 아니라 source 후보로 표시된다.

성공 경험:

- exact lookup과 semantic query가 둘 다 가능하다.
- issue body와 comment가 구분된다.
- 결과에는 source id, canonical URL, snippet, `get` 정보가 포함된다.
- source id는 `qgh://<host>/issue/<percent-encoded-node_id>` 또는 `qgh://<host>/issue-comment/<percent-encoded-node_id>` 형식이다.
- no result와 error가 JSON schema상 명확히 구분된다.

### 6.4 원문 조회

사용자 또는 agent는 검색 결과의 source id로 `get`을 호출한다. qgh는 authoritative issue body 또는 comment body와 canonical GitHub URL을 반환한다.

성공 경험:

- agent가 snippet만으로 답하지 않고 원문을 확인할 수 있다.
- citation에 필요한 GitHub URL과 entity id가 안정적으로 제공된다.
- comment-only answer도 parent issue context를 잃지 않는다.

### 6.5 Agent 사용

MCP client는 `query -> get -> cite` 순서로 qgh를 사용한다. MCP v1은 read-only search/retrieval/status만 제공한다.

성공 경험:

- agent가 sync, embedding, write-back 같은 비싼 작업을 임의로 실행하지 않는다.
- agent에게 `doctor`나 `eval` 같은 probe/test command가 MCP tool로 노출되지 않는다.
- MCP stdout/logging이 protocol을 깨지 않는다.
- unknown tool parameter는 실패한다.

## 7. 성공 기준

### Product success

- 사용자가 repo 하나를 설정하고 첫 검색까지 도달하는 시간이 짧다.
- 개발자가 GitHub UI에서 직접 찾기 어려운 issue/comment 맥락을 qgh로 찾는다.
- agent가 qgh 결과를 이용해 원문 기반 답변과 citation을 만들 수 있다.
- private content가 기본 설정에서 GitHub 외부 provider로 전송되지 않는다.

### MVP acceptance criteria

- 명시 repo의 Issues + comments를 backfill할 수 있다.
- issue/comment edit가 다음 sync 후 검색 결과에 반영된다.
- 삭제되었거나 사라진 source가 계속 검색되는 ghost result를 막는다.
- 20~30개 curated query에서 올바른 source가 top 5 안에 나온다.
- 모든 top-k 검색 결과가 `get`으로 round-trip 된다.
- vector 기능이 없어도 BM25-only 경로로 core workflow가 작동한다.
- CLI/MCP/config의 unknown parameter와 malformed input은 structured error를 낸다.
- MCP launch도 `--profile`/`QGH_PROFILE`/repo-scope single-match profile resolution을 사용하고, repo policy 기본 scope와 explicit `repo` argument allowlist 검사를 structured tool result로 반환한다.
- no-result는 성공(`results: []`)이고 validation/auth/rate-limit/source-not-found는 실패 envelope로 구분된다.
- `status`가 resolved profile, effective repo scope, repo policy path, last sync, source count, stale count, reconciliation age, DB/schema 상태, profile paths, Tantivy generation, dirty index task count를 보여준다. vector mode가 later 활성화되면 missing embeddings도 표시한다.
- `doctor`가 명시적 실행에서만 repo policy/profile resolution diagnostics와 GitHub auth/reachability/rate-limit probe를 수행하고 같은 JSON envelope로 결과를 낸다.

### Not success

- 검색 결과가 좋아 보여도 `get`으로 원문을 확인할 수 없다.
- hosted embedding을 켜야만 쓸 만하다.
- repo scope 없는 implicit profile fallback 때문에 CLI와 MCP가 다른 corpus를 검색한다.
- shared server/ACL 문제를 풀기 전에 MVP가 조직형 플랫폼으로 커진다.

## 8. 주요 리스크

### Source correctness

URL, issue number, title은 바뀔 수 있다. qgh가 이를 identity로 쓰면 transfer, title edit, delete 이후 잘못된 원문을 반환할 수 있다.

대응: source identity와 locator를 분리하고, tombstone/reconciliation을 MVP 설계에 포함한다.

### Stale index

GitHub content는 수정, 삭제, 이동된다. 특히 comment delete와 issue transfer/unavailable state는 단순 incremental sync만으로 놓칠 수 있다.

대응: watermark, last seen tracking, periodic reconciliation, status visibility를 둔다. issue/comment delete와 transfer는 REST metadata diff와 explicit reconciliation으로 보정한다.

### API rate limit과 sync 안정성

GitHub primary 한도(App installation 5,000/hr 등)보다 secondary limit(동시 요청 100, REST 900 points/min)이 먼저 binding된다. 이를 무시하면 backfill이 중단되거나 차단된다.

대응: fetch scheduler를 secondary limit 중심으로 설계하고 `retry-after`/`x-ratelimit-reset`을 준수한다. conditional request(ETag/304, primary 한도 비소모)로 폴링 비용을 낮추고, incremental sync는 `since` + `state=all` + `pull_request` key 필터를 쓴다. 기본 동시 요청은 host당 4, hard cap은 16으로 둔다. token type은 headroom을 위해 GitHub App installation token을 서버형/post-MVP에서 우선 검토한다.

### Privacy

Issues/comments에는 private technical context와 조직 정보가 들어 있다. hosted embedding/rerank를 기본값으로 두면 도입 장벽이 커진다.

대응: local-first와 BM25-only path를 기본으로 하고, hosted provider는 명시적 opt-in으로 제한한다.

### Search quality

exact lookup, label/state filter, semantic question, comment-only answer, Korean/English mixed query는 서로 다른 품질 기준을 가진다.

대응: 하나의 blended score를 confidence로 포장하지 않고, query class별 curated eval을 만든다. MVP lexical baseline은 Tantivy tokenizer + CJK n-gram fallback field로 시작하고, CJK/mixed target 미달 시 Rust에 번들 가능한 형태소 tokenizer를 optional tier로 검토한다. local embedding은 post-MVP에서 한국어 fine-tune 모델의 ONNX 경로를 우선 검토하고, score field는 typed(lexical/vector/rrf/rerank/final)로 분리한다.

### Scope creep

PR, Discussions, Projects, Web UI, shared server는 모두 유용하지만 MVP 검증을 흐린다.

대응: MVP는 Issues/comments read-only local CLI/MCP로 고정하고, 서버형 제품화와 Wiki connector는 별도 판단으로 분리한다.

### Local runtime friction

SQLite extension, vector model, local inference, platform packaging이 설치 실패를 만들 수 있다.

대응: core path는 Rust single-binary + bundled SQLite + Tantivy BM25-only로 유지하고, vector/rerank는 optional capability로 둔다. SQLite는 WAL, busy timeout, single-writer queue, migration lock을 적용하고, Tantivy는 shadow index rebuild + atomic publish로 empty/corrupt index exposure를 막는다. partial embedding coverage(issue body만 embed되고 comment 누락)는 post-MVP vector status에 추적한다.

## 9. 다음 단계

1. Product brief를 기준으로 MVP spec을 확정한다.
2. source model을 정의한다: `source_entity`, `source_version`, `source_alias`, `chunk`, `sync_run`, `tombstone`.
3. CLI/MCP/config schema와 JSON/error envelope를 먼저 고정한다 (MCP 2025-11-25: structured output, `readOnlyHint`, validation error는 `isError`, stdout은 MCP message만).
4. fake GitHub server로 issue/comment sync correctness test를 만든다 (`state=all`, `pull_request` 제외, Link pagination, ETag/304, 60초 watermark overlap, rate-limit/backoff).
5. BM25-only vertical slice를 구현한다 (SQLite authoritative store + Tantivy search index): `sync -> query -> get -> status`.
6. `doctor` CLI-only diagnostic과 `query/get` round-trip contract test를 만든다.
7. M6에서 optional vector path(sqlite-vec/SQLite vector index, 한국어 embedding은 local ONNX 경로 우선) prototype 여부를 판단한다. MVP release gate는 아니다.
8. 20~30개 curated query로 MVP 검색 품질을 평가한다.
9. MVP 결과로 서버형, GitHub App, hosted embedding, Web UI 여부를 재판단한다.

## 10. One-Line Positioning

qgh is a local-first, read-only CLI/MCP tool that syncs a repo's GitHub Issues and comments into a private local index — covering comment context that GitHub's own semantic issue search omits — so developers and agents can search and reliably return to the canonical source before acting on it.

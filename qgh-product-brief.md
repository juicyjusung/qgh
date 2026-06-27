# qgh Product Brief

작성일: 2026-06-27 KST
개정: 2026-06-27 — 딥리서치 반영 (GitHub native semantic search GA, MCP 2025-11-25, FTS5 한국어 토크나이저, sqlite-vec 상태, 인접 도구, rate-limit 리스크)

## 1. 제품 정의

qgh는 **GitHub Issues/Wiki용 local-first read-only CLI/MCP 검색 도구**다.

제품의 핵심은 GitHub Issues, issue comments, Wiki를 로컬 인덱스로 동기화한 뒤, 개발자와 agent가 CLI 또는 MCP에서 빠르게 검색하고 원문으로 되돌아갈 수 있게 하는 것이다. 첫 버전은 GitHub를 대체하는 검색 플랫폼이 아니라, private repo의 문맥을 외부로 보내지 않고 로컬에서 검색 가능한 source index를 만드는 도구다.

qgh가 제공해야 하는 기본 경험은 단순하다.

1. 사용자가 명시한 repo만 로컬에 동기화한다.
2. Issues, comments, Wiki 최신 내용을 읽기 전용으로 인덱싱한다.
3. `search/query`는 관련 source 후보를 찾는다.
4. `get`은 canonical GitHub URL과 원문 식별자를 포함한 authoritative source를 돌려준다.
5. MCP는 agent가 같은 workflow를 안전하게 반복할 수 있게 한다.

## 2. 제품 문제와 시장 맥락

많은 프로젝트에서 GitHub Issues와 Wiki는 실제 의사결정, 장애 대응, 작업 맥락, 운영 지식의 SSOT에 가깝다. 그러나 시간이 지나면 다음 문제가 반복된다.

- GitHub은 2026-04 issue semantic/hybrid search를 GA했고 공식 MCP server로도 (`semantic_issues_search`) 노출하지만, (a) issue **title/body만** index해 comments와 Wiki를 빼고, (b) semantic/hybrid 쿼리가 **분당 10회**로 throttle되며, (c) cloud에서 private content를 서버 처리하고, (d) chunk/comment 단위의 deterministic `query -> get -> cite` citation을 보장하지 않는다.
- issue title, comment, label, wiki page, 과거 결정이 분산되어 있어 필요한 맥락을 찾는 데 오래 걸린다.
- private repo 내용은 외부 검색/embedding provider로 보내기 어렵다.
- agent가 GitHub 검색 결과 snippet만 보고 답을 만들면 원문 확인과 citation이 약해진다.
- 조직 전체 검색 플랫폼을 먼저 만들면 ACL, 운영, 토큰 관리, stale index 문제가 제품 검증보다 먼저 커진다.

따라서 초기 제품 문제는 “GitHub 지식을 모두 검색하는 플랫폼”이 아니라, **개발자와 agent가 접근 권한이 있는 repo의 Issues/Wiki를 로컬에서 안정적으로 찾고 원문으로 검증하는 것**이다.

### 2.1 GitHub native search 대비 (왜 여전히 local이 필요한가)

GitHub native semantic search의 GA는 "GitHub 검색이 약하다"는 단순 전제를 약화시킨다. qgh의 정당성은 그 대신 **GitHub index가 빼먹는 corpus와 운영 제약**에 있다.

| 축 | GitHub native semantic search | qgh |
|---|---|---|
| 대상 | issue title/body | issue body + **comments + Wiki** |
| Wiki | 검색 안 됨 (Copilot도 private wiki 못 읽음, 2026-06 확인) | git clone 기반 index |
| rate limit | semantic/hybrid 분당 10회 | per-query 제한 없음, offline |
| privacy | cloud, private content 서버 처리 | local-first, 외부 egress 없음 |
| scope | top-100 repo | 명시 repo allowlist 임의 |
| citation | ranked issue 반환 | chunk/comment 단위 canonical URL |
| GHES/air-gap | cloud-first (GHES 지원 미확인) | local 동작 |

brief는 "GitHub 검색이 약하다"가 아니라 **comments/Wiki coverage + offline + rate-limit-free + deterministic citation + GHES/규제 환경**을 정당성으로 명시한다. "GitHub가 이미 semantic issue search를 주는데 왜 local index인가"는 PRD에서 가장 먼저 받을 질문이므로 FAQ로 선제 대응한다.

### 2.2 인접 도구와 차별화

- **GitHub 공식 MCP server**: live API proxy + cloud semantic issue search. local index/offline/Wiki 없음.
- **dogsheep/github-to-sqlite**: issues→local SQLite sync (최근접 prior art). MCP/semantic/Wiki 없음.
- **tobi/qmd**: 아키텍처 sibling. local markdown 대상, `get`/`multi_get` round-trip 있음. GitHub source connector 없음.
- **shinpr/mcp-local-rag**: local-first + MCP + hybrid + chunk retrieval 보유. GitHub connector만 없음.

개별 재료(local-first, MCP, hybrid search, chunk-get)는 2026 기준 모두 commoditized다. qgh의 moat는 **Issues+comments+Wiki source connector + deterministic canonical-URL citation의 교집합**이며, 카테고리 신규성이 아니라 이 niche 소유로 포지셔닝한다. github-to-sqlite + qmd-style MCP 조합으로 모방 가능하므로 통합 UX와 citation 계약의 완성도가 방어선이다.

## 3. 사용자

### Primary users

- **개발자**: 과거 이슈, 장애 대응, 결정 배경, Wiki 문서를 빠르게 찾아 현재 작업에 연결하고 싶다.
- **AI coding agent 사용자**: agent가 private repo의 Issues/Wiki 맥락을 검색하되, 원문 조회와 citation이 가능한 형태로 쓰게 하고 싶다.
- **기술 리드/maintainer**: repo에 누적된 논의와 운영 지식을 별도 서버 구축 없이 개인 로컬 환경에서 재사용하고 싶다.

### Secondary users

- **소규모 팀**: shared server를 만들기 전, repo별 검색 품질과 workflow 가치를 검증하고 싶다.
- **보안/플랫폼 담당자**: private content가 기본적으로 로컬 밖으로 나가지 않는 검색 도구를 선호한다.

## 4. MVP Scope

MVP는 작은 범위를 강하게 검증한다.

### 포함

- 명시적 repo allowlist
- GitHub Issues title/body metadata sync
- issue comments sync
- GitHub Wiki latest branch sync
- local SQLite 기반 index
- BM25/keyword 검색 기본 동작 (한국어 corpus 위해 FTS5 trigram tokenizer + 1~2자 쿼리 LIKE fallback; unicode61 기본값은 한국어 recall 부족)
- optional vector/hybrid search (sqlite-vec)
- CLI 명령: `sync`, `search` 또는 `query`, `get`, `status`
- MCP tools: `query`, `get`, `status` (MCP 2025-11-25: structured output `outputSchema`, `readOnlyHint: true`, validation 실패는 `isError`)
- 검색 결과의 stable source id, entity type, canonical URL, `get` 호출 정보
- read-only 동작
- local-first privacy default
- strict config/CLI/MCP schema validation
- stale/missing index 상태를 보여주는 status

### MVP에서 반드시 지킬 제약

- repo는 사용자가 명시해야 한다.
- hosted embedding/rerank는 기본값이 아니다.
- vector 기능이 없어도 sync/search/get/status는 작동해야 한다.
- 검색 결과 score를 confidence처럼 노출하지 않는다.
- source identity는 URL, title, path, issue number와 분리한다.
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
- Wiki 전체 history 검색
- 모든 issue timeline event indexing

## 6. 핵심 Workflow

### 6.1 초기 설정

사용자는 검색하고 싶은 repo를 명시한다. qgh는 token source, GitHub host, repo allowlist, local DB path를 하나의 profile로 고정한다.

성공 경험:

- 어떤 repo가 인덱싱되는지 명확하다.
- CLI와 MCP가 같은 profile과 같은 local DB를 본다.
- 잘못된 repo, 누락된 token, unknown config key는 조용히 무시되지 않고 실패한다.

### 6.2 동기화

사용자는 `sync`를 실행해 Issues, comments, Wiki를 로컬로 가져온다. qgh는 변경된 source를 반영하고 삭제/이동 가능성이 있는 source를 추적한다.

성공 경험:

- 최초 backfill이 끝나면 search 가능한 상태가 된다.
- 이후 sync는 edit/update를 반영한다.
- status에서 마지막 sync, stale source, missing embedding, Wiki commit 상태를 볼 수 있다.

### 6.3 검색

사용자 또는 agent는 자연어, keyword, issue number, repo/label/state 같은 filter로 검색한다. 결과는 답변이 아니라 source 후보로 표시된다.

성공 경험:

- exact lookup과 semantic query가 둘 다 가능하다.
- issue body, comment, wiki section이 구분된다.
- 결과에는 source id, canonical URL, snippet, `get` 정보가 포함된다.
- no result와 error가 JSON schema상 명확히 구분된다.

### 6.4 원문 조회

사용자 또는 agent는 검색 결과의 source id로 `get`을 호출한다. qgh는 authoritative body/comment/wiki section과 canonical GitHub URL을 반환한다.

성공 경험:

- agent가 snippet만으로 답하지 않고 원문을 확인할 수 있다.
- citation에 필요한 GitHub URL과 entity id가 안정적으로 제공된다.
- comment-only answer도 parent issue context를 잃지 않는다.

### 6.5 Agent 사용

MCP client는 `query -> get -> cite` 순서로 qgh를 사용한다. MCP v1은 read-only search/retrieval/status만 제공한다.

성공 경험:

- agent가 sync, embedding, write-back 같은 비싼 작업을 임의로 실행하지 않는다.
- MCP stdout/logging이 protocol을 깨지 않는다.
- unknown tool parameter는 실패한다.

## 7. 성공 기준

### Product success

- 사용자가 repo 하나를 설정하고 첫 검색까지 도달하는 시간이 짧다.
- 개발자가 GitHub UI에서 직접 찾기 어려운 issue/comment/wiki 맥락을 qgh로 찾는다.
- agent가 qgh 결과를 이용해 원문 기반 답변과 citation을 만들 수 있다.
- private content가 기본 설정에서 GitHub 외부 provider로 전송되지 않는다.

### MVP acceptance criteria

- 명시 repo의 Issues + comments + Wiki를 backfill할 수 있다.
- issue/comment/wiki edit가 다음 sync 후 검색 결과에 반영된다.
- 삭제되었거나 사라진 source가 계속 검색되는 ghost result를 막는다.
- 20~30개 curated query에서 올바른 source가 top 5 안에 나온다.
- 모든 top-k 검색 결과가 `get`으로 round-trip 된다.
- vector 기능이 없어도 BM25-only 경로로 core workflow가 작동한다.
- CLI/MCP/config의 unknown parameter와 malformed input은 structured error를 낸다.
- `status`가 last sync, source count, stale count, missing embeddings, DB/schema 상태를 보여준다.

### Not success

- 검색 결과가 좋아 보여도 `get`으로 원문을 확인할 수 없다.
- hosted embedding을 켜야만 쓸 만하다.
- implicit repo/profile fallback 때문에 CLI와 MCP가 다른 corpus를 검색한다.
- shared server/ACL 문제를 풀기 전에 MVP가 조직형 플랫폼으로 커진다.

## 8. 주요 리스크

### Source correctness

URL, issue number, wiki path, title은 바뀔 수 있다. qgh가 이를 identity로 쓰면 rename, transfer, delete 이후 잘못된 원문을 반환할 수 있다.

대응: source identity와 locator를 분리하고, tombstone/reconciliation을 MVP 설계에 포함한다.

### Stale index

GitHub content는 수정, 삭제, 이동된다. 특히 comment delete와 wiki rename/delete는 단순 incremental sync만으로 놓칠 수 있다.

대응: watermark, last seen tracking, periodic reconciliation, status visibility를 둔다. 특히 wiki는 `gollum` webhook이 created/edited만 보내 delete/rename을 감지하지 못하므로 wiki tombstone은 git clone tree diff로만 처리한다. issue/comment delete는 webhook(`deleted`/`transferred`)으로 감지하되 listener downtime 보정을 위해 periodic reconciliation을 병행한다.

### API rate limit과 sync 안정성

GitHub primary 한도(App installation 5,000/hr 등)보다 secondary limit(동시 요청 100, REST 900 points/min)이 먼저 binding된다. 이를 무시하면 backfill이 중단되거나 차단된다.

대응: fetch scheduler를 secondary limit 중심으로 설계하고 `retry-after`/`x-ratelimit-reset`을 준수한다. conditional request(ETag/304, primary 한도 비소모)로 폴링 비용을 낮추고, incremental sync는 `since` + `state=all` + `pull_request` key 필터를 쓴다. token type은 headroom을 위해 GitHub App installation token을 우선한다.

### Privacy

Issues/comments/Wiki에는 private technical context와 조직 정보가 들어 있다. hosted embedding/rerank를 기본값으로 두면 도입 장벽이 커진다.

대응: local-first와 BM25-only path를 기본으로 하고, hosted provider는 명시적 opt-in으로 제한한다.

### Search quality

exact lookup, label/state filter, semantic question, comment-only answer, Korean/English mixed query는 서로 다른 품질 기준을 가진다.

대응: 하나의 blended score를 confidence로 포장하지 않고, query class별 curated eval을 만든다. 한국어는 FTS5 unicode61로 recall이 거의 없으므로 zero-dep baseline은 trigram tokenizer(+1~2자 LIKE fallback), 품질 tier는 형태소 분석(kiwipiepy/lindera) 후 unicode61로 둔다. local embedding은 한국어 fine-tune(arctic-embed-ko/KURE) 기준 ONNX 경로를 우선 검토하고, score field는 typed(lexical/vector/rrf/rerank/final)로 분리한다.

### Scope creep

PR, Discussions, Projects, Web UI, shared server는 모두 유용하지만 MVP 검증을 흐린다.

대응: MVP는 Issues/Wiki read-only local CLI/MCP로 고정하고, 서버형 제품화는 별도 판단으로 분리한다.

### Local runtime friction

SQLite extension, vector model, local inference, platform packaging이 설치 실패를 만들 수 있다.

대응: core path는 BM25-only로 유지하고, vector/rerank는 optional capability로 둔다. sqlite-vec는 pre-1.0(v0.1.9, stable은 brute-force KNN)이지만 read-only static index에는 적합하며 scale 시 binary quantization으로 대응한다. local DB는 WAL, busy timeout, single-writer queue, migration lock, shadow FTS rebuild + atomic publish를 둔다. partial embedding coverage(issue body만 embed되고 comment 누락)는 status에 추적한다.

## 9. 다음 단계

1. Product brief를 기준으로 MVP spec을 확정한다.
2. source model을 정의한다: `source_entity`, `source_version`, `source_alias`, `chunk`, `sync_run`, `tombstone`.
3. CLI/MCP/config schema를 먼저 고정한다 (MCP 2025-11-25: structured output, `readOnlyHint`, validation error는 `isError`, stdout은 MCP message만).
4. fake GitHub server와 wiki git fixture로 sync correctness test를 만든다.
5. BM25-only vertical slice를 구현한다 (FTS5 trigram tokenizer + 한국어 LIKE fallback): `sync -> search -> get -> status`.
6. query/get round-trip contract test를 만든다.
7. optional vector path(sqlite-vec, 한국어 embedding은 ONNX 경로 우선)와 embedding fingerprint를 추가한다.
8. 20~30개 curated query로 MVP 검색 품질을 평가한다.
9. MVP 결과로 서버형, GitHub App, hosted embedding, Web UI 여부를 재판단한다.

## 10. One-Line Positioning

qgh is a local-first, read-only CLI/MCP tool that syncs a repo's GitHub Issues, comments, and Wiki into a private local index — covering what GitHub's own semantic search omits — so developers and agents can search and reliably return to the canonical source before acting on it.

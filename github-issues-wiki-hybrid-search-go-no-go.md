# GitHub Issues/Wiki 하이브리드 검색 Go/No-Go 판단 보고서

작성일: 2026-06-27 KST  
대상: GitHub Issues + GitHub Wiki를 SSOT로 쓰는 조직/프로젝트의 별도 인덱싱 및 임베딩 기반 검색 시스템  
검토 기준 저장소: `tobi/qmd` `v2.6.3`, commit `e428df76bc0274d9e93eb7ca3e95673315c42e90`

## 0. 현재 MVP scope override

2026-06-29 KST에 qgh MVP scope를 GitHub Issues와 issue comments only로 재설정했다. 또한 architecture lock은 Rust single-binary CLI/MCP, bundled SQLite authoritative store, Tantivy derived BM25 index, explicit XDG profile store다. 이 보고서의 Wiki, SQLite FTS5, sqlite-vec, server/search-engine 비교 분석은 post-MVP 또는 historical baseline 검토 자료로 남기며, 현재 MVP 계약이나 release gate가 아니다. 현재 canonical MVP scope는 `qgh-prd.md`, `qgh-product-brief.md`, `qgh-mvp-evidence-decision-summary.md`, ADR-0003~0007을 따른다.

## 0.1 결론 요약

최종 판단은 **조건부 가능, MVP는 Go**다.

단, 이것은 “GitHub 전체를 대체하는 범용 검색”이 아니라 **repo 단위 Issues + Wiki를 읽기 전용으로 동기화하고, agent/개발자용 검색 및 원문 조회를 제공하는 로컬 CLI/MCP 우선 시스템**으로 시작할 때의 판단이다. 조직 전체 서버형 검색, 완전한 ACL, 모든 repo의 실시간 동기화, 프로젝트 필드/PR/Discussion까지 포함한 통합 검색은 1차 범위를 넘긴다.

추천 MVP 아키텍처는 다음과 같다.

- **동기화**: GitHub REST API로 Issues/Comments를 수집하고, Wiki는 `{repo}.wiki.git` clone/fetch로 수집한다.
- **저장소**: 로컬 우선은 SQLite FTS5 + `sqlite-vec`. 서버형 전환 시 Postgres + pgvector + FTS를 우선 검토한다.
- **검색**: qmd의 설계를 변형해 BM25/FTS + vector + RRF + 선택적 rerank를 사용한다.
- **인터페이스**: CLI + MCP server를 먼저 만든다. GitHub App은 서버형/조직형 단계에서 붙인다.
- **임베딩**: private repo 기본값은 local embedding. hosted embedding은 명시적 opt-in과 데이터 처리 정책 검토 후 허용한다.

피해야 할 방향은 다음과 같다.

- GitHub Search API만으로 검색 경험을 해결하려는 방향
- Wiki를 HTML scraping이나 비공식 API로 다루는 방향
- 첫 버전부터 조직 전체 shared server와 완전 ACL을 목표로 잡는 방향
- qmd를 거의 그대로 감싸서 Issues를 “파일처럼” 흉내 내는 방향

## 1. 확인된 사실과 판단의 구분

**확인된 사실**

- qmd는 README만이 아니라 `src/store.ts`, `src/db.ts`, `src/llm.ts`, `src/mcp/server.ts`, `src/cli/qmd.ts`, `src/collections.ts`, `src/ast.ts`, `docs/SYNTAX.md`, `example-index.yml`, tests, `package.json`, `CHANGELOG.md`를 직접 확인했다.
- qmd는 로컬 Markdown/문서 파일을 대상으로 SQLite FTS5, `sqlite-vec`, local GGUF embedding/rerank/generation 모델, MCP server, CLI/SDK를 제공한다.
- GitHub REST Issues API는 issue body, labels, assignees, milestone, state, timestamps 등을 제공하고, issue comments는 별도 endpoint에서 pagination 및 `since` 기반 수집을 지원한다.
- GitHub Wiki는 별도 Git repo로 clone 가능하며, Wiki file 수에는 GitHub 문서상 5,000개 soft limit이 있다.
- GitHub rate limit, pagination, conditional request, webhook, GHES endpoint/limit 관련 내용은 GitHub 공식 문서 기준으로 확인했다.

**판단/추론**

- REST API를 Issues bulk sync의 1차 수단으로 권장하는 것은 구현 판단이다. GraphQL도 가능하지만, 대량 incremental sync에서는 REST의 endpoint/ETag/pagination 모델이 더 단순하다.
- SQLite FTS5 + `sqlite-vec`를 MVP 기본 저장소로 권장하는 것은 qmd 구조와 로컬 배포 난이도를 기준으로 한 판단이다.
- qmd의 ranking 구조를 그대로 복사하지 말고 GitHub metadata boost와 source type별 chunking을 추가해야 한다는 부분은 설계 판단이다.

## 2. qmd 전수 검사 요약

### 2.1 qmd의 전체 구조

qmd는 local-first 검색 엔진이다. 핵심 흐름은 다음과 같다.

1. collection config에서 로컬 경로와 glob/ignore/update hook을 읽는다.
2. 파일을 scan해 content hash, title, body, path, collection을 SQLite에 저장한다.
3. FTS5 virtual table에 path/title/body를 넣어 BM25 lexical search를 제공한다.
4. 문서를 chunking하고 local GGUF embedding 모델로 chunk vector를 만든다.
5. `sqlite-vec` virtual table에 vector를 저장한다.
6. query 시 BM25, vector search, query expansion, RRF fusion, optional rerank를 조합한다.
7. CLI, SDK, MCP server가 동일한 store/search/retrieval 계층을 사용한다.

확인한 주요 구성:

- CLI: `qmd`, `collection`, `context`, `update`, `embed`, `search`, `vsearch`, `query`, `get`, `multi-get`, `status`, `doctor`, `bench`, `mcp`, `cleanup`, `pull`
- Config: YAML 기반 `index.yml`, project-local `.qmd/index.yml`, XDG/QMD config path, collection별 path/pattern/ignore/context/update/includeByDefault
- Storage: SQLite, FTS5, `sqlite-vec`, content hash, document table, vector table, LLM cache, config/collection metadata
- Embedding: local GGUF model download/cache, model fingerprint, batch embedding, stale/missing embedding detection
- Search: BM25, vector, structured query, query expansion, RRF, top-rank bonus, rerank, explain output
- Retrieval: `get`, `multi-get`, docid, line ranges, `qmd://` virtual path
- Agent workflow: search first, retrieve exact full source next, cite docid/line numbers
- MCP: stdio and HTTP server, tools `query`, `get`, `multi_get`, `status`

### 2.2 qmd의 indexing 방식

qmd indexing은 filesystem 기반이다. `fast-glob`로 collection path 아래 파일을 찾고, default exclude로 `.git`, `node_modules`, `.cache`, `vendor`, `dist`, `build` 등을 제외한다. 문서는 UTF-8로 읽고, 빈 파일은 건너뛴다. content hash를 기준으로 unchanged/update를 판단하며, 사라진 파일은 inactive 처리한다. 동일 content가 여러 collection/path에 있을 수 있으므로 content table과 document table을 분리한다.

GitHub 대상에서는 이 구조를 그대로 쓸 수 없다. Issues는 파일이 아니라 API resource이고, Comments는 Issue의 child resource이며, Wiki만 Git file에 가깝다. 그러나 **content hash, soft delete, source identity, stale embedding detection, incremental update state**는 그대로 재사용할 가치가 크다.

### 2.3 qmd의 chunking 방식

qmd는 기본적으로 약 900 token chunk, 15% overlap, 200-token window를 사용한다. Markdown heading, code fence, paragraph/list boundary를 고려해 split 지점을 잡는다. TS/JS/Python/Go/Rust 파일에는 tree-sitter 기반 AST chunking을 optional로 제공한다.

GitHub 대상에서는 Wiki Markdown에는 Markdown chunking을 거의 그대로 적용할 수 있다. Issue는 다음처럼 바꿔야 한다.

- Issue title + body: 하나의 parent document, body는 heading/paragraph 기준 chunk
- Comment: 독립 chunk로 저장하되 issue title, labels, repo, issue number를 metadata prefix로 붙임
- 긴 thread: thread summary chunk는 후순위 기능
- Wiki: page path/title + heading section chunk

### 2.4 qmd의 BM25/FTS + vector + reranking 조합

qmd의 hybrid pipeline은 다음 원칙을 쓴다.

- original query를 lexical/vector 양쪽에 넣고 더 높은 가중치를 준다.
- query expansion은 `lex`, `vec`, `hyde`, `intent` 형태의 typed subquery로 나뉜다.
- BM25 결과와 vector 결과를 RRF로 합친다.
- top rank bonus와 rank-position별 blend를 적용한다.
- 상위 candidate만 reranker에 넣어 final score를 조정한다.
- 강한 lexical hit가 있으면 expansion을 생략하는 shortcut이 있다.

이 설계는 GitHub 검색에도 매우 유용하다. 다만 GitHub에서는 정확한 issue number, label, repo, author, state, milestone, timestamp 같은 metadata signal이 강하므로 qmd ranking에 **metadata boost/filter layer**를 추가해야 한다.

### 2.5 qmd의 local-first 설계

qmd는 private/local 문서를 전제로 한다. 모델도 기본적으로 local GGUF를 다운로드해 `~/.cache/qmd/models`에 저장한다. 기본 모델 크기는 README 기준 embedding model 약 300MB, reranker 약 640MB, query expansion model 약 1.1GB다.

GitHub private repo 검색에서도 local-first는 강점이다. private issue/wiki 내용을 외부 embedding API에 보내지 않아도 된다. 반대로 서버형 조직 검색에서는 qmd의 local-first 전제가 약해진다. 서버형은 multi-tenant auth, ACL, background sync, queue, observability, token rotation이 필요하다.

### 2.6 agent workflow 관점 장점

qmd의 가장 큰 장점은 “검색 결과 자체”보다 **agent가 안전하게 원문으로 돌아갈 수 있는 workflow**다.

- search result에 docid, score, source path, snippet을 준다.
- `get`/`multi-get`으로 원문 전체 또는 line range를 가져온다.
- MCP가 `query`, `get`, `multi_get`, `status`를 제공한다.
- JSON/files/Markdown/XML output으로 agent와 shell 모두 다루기 쉽다.

GitHub 대상 시스템도 이 방식을 따라야 한다. 검색 결과에는 반드시 canonical GitHub URL, repo, issue number/comment id/wiki path, updated_at, chunk id, 원문 조회 id가 포함되어야 한다.

## 3. qmd 기능별 적용 가능성

분류 기준:

- **그대로 적용 가능**: 개념과 구현을 거의 그대로 쓸 수 있음
- **수정하면 적용 가능**: 원칙은 좋지만 GitHub source model에 맞게 바꿔야 함
- **적용 불가**: qmd의 전제가 GitHub 대상과 맞지 않음
- **불필요**: MVP 또는 GitHub 전용 목적에는 필요도가 낮음

| qmd 기능 | 내부 구현 방식 | 사용자 가치 | GH Issues/Wiki 적용 | 필요한 변경점 / 주의점 |
|---|---|---:|---|---|
| YAML collection config | `index.yml`, collection별 path/pattern/ignore/context/update | 여러 문서 집합 관리 | 수정하면 적용 가능 | collection을 local path가 아니라 `owner/repo`, source type, auth profile로 모델링 |
| global/context prompt | global context + collection context | 검색 의도/출처 설명 강화 | 수정하면 적용 가능 | repo description, wiki purpose, team ownership metadata로 대체 |
| local glob indexing | filesystem scan + ignore | 빠른 로컬 인덱싱 | 적용 불가 | Issues는 REST/GraphQL sync, Wiki만 git scan |
| update hook | collection path에서 shell command 실행 | pull/rebuild 자동화 | 수정하면 적용 가능 | Wiki `git fetch`, Issues API sync job으로 대체. 임의 shell hook은 서버형에서 위험 |
| content hash table | content hash 기반 dedupe/staleness | 불필요 reindex 방지 | 그대로 적용 가능 | GitHub entity id + body hash + updated_at 함께 저장 |
| soft delete/inactive docs | 사라진 파일 inactive 처리 | stale result 방지 | 수정하면 적용 가능 | deleted comment/wiki page/issue transfer 처리 필요 |
| FTS5 BM25 | path/title/body FTS5, weighted bm25 | keyword 검색 | 수정하면 적용 가능 | title/labels/body/comments/wiki sections 등 field weight 재설계 |
| CJK FTS normalization | unicode61/porter 및 CJK 관련 test/logic | 한국어/일본어/중국어 검색 개선 | 수정하면 적용 가능 | GitHub org의 실제 언어 corpus로 평가 필요 |
| vector chunks | chunk별 embedding + `sqlite-vec` | 의미 기반 검색 | 그대로 적용 가능 | issue/comment/wiki source metadata 포함 |
| embedding fingerprint | model/prompt/chunk config fingerprint | model 변경 시 stale 감지 | 그대로 적용 가능 | source-specific prompt fingerprint 추가 |
| local GGUF model handling | `node-llama-cpp`, HF download/cache | private data local 처리 | 수정하면 적용 가능 | CLI/MCP 로컬 모드에는 적합. 서버형은 별도 model service 고려 |
| query expansion | local generation model로 typed query 생성 | 모호한 질문 보강 | 수정하면 적용 가능 | MVP에서는 optional. GitHub metadata syntax와 충돌 없게 제한 |
| structured query grammar | `intent`, `lex`, `vec`, `hyde` | agent가 검색 전략 명시 | 그대로 적용 가능 | `repo:`, `label:`, `state:`, `author:` 같은 filter syntax 추가 |
| BM25 + vector RRF | lexical/vector result fusion | recall/precision 균형 | 그대로 적용 가능 | metadata boost와 exact lookup 우선순위 추가 |
| LLM rerank | 상위 chunk 후보 reranking/cache | 답변 품질 개선 | 수정하면 적용 가능 | 비용/latency/privacy 때문에 default off 또는 local only 권장 |
| strong BM25 shortcut | 강한 lexical hit면 expansion 생략 | latency 절감 | 수정하면 적용 가능 | issue number/title exact match에는 유용. 일반 metadata hit에는 조심 |
| `search`, `vsearch`, `query` CLI | lexical/vector/hybrid 분리 | 디버깅과 UX 명확 | 그대로 적용 가능 | 명령명은 `ghkb search --mode` 등으로 단순화 가능 |
| JSON output | docid, score, file, line, body/snippet | agent integration | 그대로 적용 가능 | GitHub URL, issue/comment/wiki ids 포함 |
| files output | 파일 경로 목록 출력 | shell pipeline | 수정하면 적용 가능 | `ghdoc://...` URI 또는 GitHub URL 목록으로 대체 |
| Markdown/XML/CSV output | formatter 계층 | 다양한 소비자 | 불필요 | MVP는 JSON + human CLI면 충분 |
| `get` / `multi-get` | docid/path/line range 원문 조회 | hallucination 방지 | 수정하면 적용 가능 | line range 대신 comment id, body section, wiki line/heading range |
| `qmd://` virtual URI | collection/path 추상화 | 안정적 source id | 수정하면 적용 가능 | `ghdoc://owner/repo/issues/123#comment-456` 등 필요 |
| MCP stdio/HTTP | tools `query/get/multi_get/status` | agent에서 바로 사용 | 그대로 적용 가능 | tool schema를 GitHub entity 중심으로 변경 |
| SDK `createStore` | store API 재사용 | 앱/서버 embedding | 수정하면 적용 가능 | sync API, auth, ACL, source adapters 추가 |
| bench/eval | precision/recall/MRR/F1/latency | 품질 회귀 방지 | 수정하면 적용 가능 | 알려진 issue/wiki 질문 세트로 평가 fixture 구성 |
| doctor/status | index/model/vector 상태 점검 | 운영 신뢰성 | 그대로 적용 가능 | API quota, webhook lag, wiki commit, stale count 추가 |
| cleanup/vacuum | cache/orphan cleanup | DB 관리 | 그대로 적용 가능 | deleted GitHub entities cleanup policy 추가 |
| AST chunking | tree-sitter code chunk | code search 품질 | 불필요 | Issues/Wiki MVP에는 과함. Wiki code-heavy org면 later |
| local editor URI | 로컬 파일 editor link | 개발 UX | 불필요 | GitHub HTML URL이 canonical link |
| finetune/eval assets | query expansion/ranking 실험 파일 | 장기 품질 개선 | 불필요 | MVP에서는 제외. 검색 품질 개선 단계에서 검토 |
| package/runtime launcher | Node/Bun, macOS Metal mitigation | 설치 안정성 | 수정하면 적용 가능 | 로컬 모델 사용 시만 필요 |

## 4. qmd에서 재사용할 원칙과 버릴 전제

**재사용할 원칙**

- 검색과 원문 조회를 분리한다. Search result는 답이 아니라 source 후보여야 한다.
- 모든 result에 안정적인 source URI/docid를 부여한다.
- FTS와 vector를 함께 쓰고, RRF로 합친 뒤 optional rerank를 적용한다.
- chunk embedding에는 source metadata와 fingerprint를 저장한다.
- content hash와 updated_at으로 incremental indexing을 한다.
- `status`/`doctor`/`bench`를 처음부터 둔다.
- MCP tool은 `query`, `get`, `multi_get`, `status`처럼 작고 명확하게 유지한다.

**버릴 전제**

- source가 모두 로컬 파일이라는 전제
- path가 canonical identity라는 전제
- auth/ACL/rate limit/staleness를 신경 쓰지 않아도 된다는 전제
- local editor URI가 사용자에게 가장 좋은 링크라는 전제
- 모든 문서가 Markdown body 하나로 표현된다는 전제
- 모델 다운로드와 local inference가 모든 환경에서 자연스럽게 가능하다는 전제

## 5. GitHub Issues 인덱싱 가능성

### 5.1 수집 가능한 데이터

REST API 기준으로 다음 항목 수집은 가능하다.

- Issue: title, body, labels, assignees, milestone, state, author, timestamps, URL, comments count
- Comments: body, author, created_at, updated_at, issue_url, comment id, URL
- Reactions: issue와 issue comment reaction list endpoint를 통해 수집 가능
- Timeline: issue timeline event endpoint로 이벤트 일부 수집 가능

MVP에서 반드시 수집할 것은 issue title/body/state/labels/milestone/assignees/author/updated_at/html_url, comments body/author/updated_at/html_url이다. Reactions와 timeline은 ranking feature 또는 audit feature로 후순위다.

### 5.2 REST API vs GraphQL API

| 항목 | REST API | GraphQL API | 판단 |
|---|---|---|---|
| Bulk issue sync | repo issues endpoint, `since`, `per_page=100` | connection pagination `first/last` 1..100 | REST 우선 |
| Comment sync | repo-level issue comments endpoint가 `since` 지원 | issue별 comments connection | REST 우선 |
| Metadata enrichment | endpoint가 분리되어 많아질 수 있음 | 한 query에서 필요한 field 선택 가능 | GraphQL 보조 |
| Rate model | core requests/hour + secondary limits | points/hour + points/min + query cost | REST가 예측 쉬움 |
| Conditional request | ETag/Last-Modified와 304 활용 가능 | 일반적으로 REST만큼 단순하지 않음 | REST 우선 |
| Project fields 등 | REST만으로 부족할 수 있음 | GraphQL 강점 | later GraphQL |
| GHES 지원 | `/api/v3` | `/api/graphql` | 둘 다 가능 |

**판단**: MVP bulk sync는 REST로 간다. GraphQL은 나중에 project fields, issue relationship, selective enrichment에 붙인다.

### 5.3 Pagination, incremental sync, edit/delete 처리

권장 sync 전략:

1. 최초 backfill: `GET /repos/{owner}/{repo}/issues?state=all&per_page=100`를 Link header로 끝까지 순회한다.
2. Comments backfill: `GET /repos/{owner}/{repo}/issues/comments?per_page=100`를 순회한다.
3. Incremental: Issues와 Comments 모두 `since=<last_successful_sync>`를 사용하고 `updated_at` 기준으로 waterline을 저장한다.
4. Conditional request: endpoint/page별 ETag 또는 Last-Modified를 저장하고 304를 활용한다.
5. Webhook 사용 가능 시: `issues`, `issue_comment` event로 near-real-time update를 받는다.
6. Delete 보정: comment delete는 webhook이 없으면 놓칠 수 있으므로 주기적 reconciliation이 필요하다. 예를 들어 최근 N일 issue의 comment id 목록을 재조회하고 누락된 comment를 inactive 처리한다.

주의점:

- `since`는 “updated after” 성격이므로 clock skew와 중복 처리를 감안해 overlap window를 둔다.
- issue transfer/delete, label rename, milestone change는 webhook 또는 periodic full sweep이 필요하다.
- GitHub secondary rate limit은 사전에 정확히 조회할 수 없으므로 queue/backoff가 필요하다.

### 5.4 Rate limit과 abuse/secondary limit

공식 문서 기준 주요 숫자:

- GitHub App installation token REST primary limit: 기본 5,000 requests/hour. Enterprise Cloud org 설치는 15,000/hour. 비 Enterprise 설치는 repo/user 수에 따라 증가하며 12,500/hour cap이 있다.
- `GITHUB_TOKEN`: repo당 1,000 requests/hour, Enterprise resource는 15,000/hour.
- REST secondary limit: REST+GraphQL 합산 concurrent request는 최대 100. REST endpoint는 900 points/min 제한이 언급된다. 대부분 GET/HEAD/OPTIONS는 1 point, mutation은 5 points다.
- GraphQL primary limit: 일반 user/app 기준 5,000 points/hour, 일부 Enterprise 조건에서 10,000 points/hour. GitHub App installation은 조건에 따라 12,500 cap.
- GraphQL connection pagination: `first` 또는 `last`는 1..100.
- Search API: 인증된 일반 search는 Search docs 기준 분당 30회, unauthenticated search는 분당 10회 수준의 별도 제한을 받는다. code search는 더 낮은 별도 제한이 적용되며, 2026-06-27 확인한 Search docs는 authenticated code search를 분당 9회로 설명했다. issue search의 `semantic`/`hybrid` mode도 인증이 필요하고 분당 10회 제한을 받는다. 또한 search response에는 `incomplete_results`가 있을 수 있고, search scope도 제한된다.

**판단**: 별도 인덱스가 필요한 가장 큰 이유 중 하나가 이 rate/latency/recall 문제다. GitHub Search API를 query-time backend로 쓰면 agent workflow에서 안정적인 `get`/rerank/chunk retrieval을 보장하기 어렵다.

### 5.5 권한, private repo, GHES

- Fine-grained token 또는 GitHub App installation token은 Issues read 권한이 필요하다.
- Private repo는 token이 접근 가능한 repo만 수집해야 한다.
- shared server에서는 ingest 권한과 query 권한을 분리해야 한다. 한 번 수집한 private content를 권한 없는 사용자에게 보여주면 안 된다.
- GHES는 REST base URL이 `https://HOSTNAME/api/v3`, GraphQL endpoint가 `https://HOSTNAME/api/graphql` 형태다. GHES rate limit은 instance 설정에 따라 다르며 기본적으로 GitHub.com과 다를 수 있다.

## 6. GitHub Wiki 인덱싱 가능성

GitHub Wiki는 별도 Git repository로 제공된다. GitHub 공식 문서는 Wiki를 `https://github.com/OWNER/REPO.wiki.git` 형태로 clone할 수 있다고 설명한다. 따라서 Wiki indexing은 API보다 git clone/fetch 기반이 자연스럽다.

권장 sync 전략:

- 최초: wiki repo를 bare 또는 working clone으로 clone한다.
- Incremental: `git fetch` 후 이전 indexed commit과 새 default branch commit 사이의 diff를 계산한다.
- Rename/delete: git diff rename detection과 deleted path를 반영한다.
- History: MVP에서는 최신 default branch만 index한다. page history search는 제외한다.
- Private wiki: repo 접근 권한을 따르므로 clone credential도 repo 접근 권한을 가져야 한다.
- Scale: GitHub 문서상 Wiki는 총 file 수 5,000개 soft limit이 있다. 이 제한을 넘는 Wiki는 일부 page inaccessible 문제가 있을 수 있어, 대형 knowledge base는 Wiki 자체가 병목이다.

GitHub API만으로 Wiki를 다루려는 방향은 추천하지 않는다. Wiki는 Git repo로 보는 것이 source-of-truth와 변경 감지 측면에서 더 정확하다.

## 7. 검색 시스템 설계 후보 비교

| 형태 | 장점 | 단점 | 난이도 | 판단 |
|---|---|---|---:|---|
| 로컬 CLI | 설치/보안 단순, private content 외부 전송 없음, qmd 패턴 재사용 쉬움 | 사용자별 중복 index, 공유 어려움 | 낮음 | MVP 1순위 |
| MCP server | agent workflow와 직접 연결, `query/get/status` 모델이 명확 | long-running process와 model memory 관리 필요 | 낮음~중간 | MVP 포함 |
| 서버형 service | 조직 공유, centralized sync, webhook 수신, UI/API 제공 | ACL/운영/비용/토큰 관리 복잡 | 높음 | MVP 이후 |
| GitHub App | webhook, installation token, repo 권한 모델과 잘 맞음 | App 설정/배포/권한 검토 필요 | 중간~높음 | 서버형 단계에서 권장 |
| GitHub Action | repo 내부 자동 sync 가능 | long-lived search service에는 부적합 | 중간 | 보조 기능만 |

**추천 순서**

1. 로컬 CLI + MCP + SQLite
2. GitHub App 기반 sync worker + 서버 API
3. 조직형 shared search + ACL enforcement + Postgres/OpenSearch 계열

## 8. 하이브리드 검색 설계

### 8.1 Keyword/BM25 대상 필드

FTS/BM25에는 다음 필드를 넣는다.

- Issue: title, body, issue number, labels, milestone title/description, assignees, author, state, repo, URL
- Comment: body, author, parent issue title/number/labels/repo
- Wiki: page title, path, headings, body

권장 weight:

- 매우 높음: issue number exact, title, wiki page title
- 높음: labels, milestone, headings
- 중간: issue body, wiki body
- 낮음: comments body, author, assignee, URLs

### 8.2 Embedding 대상 필드

Embedding은 raw field 전체가 아니라 retrieval에 유리한 chunk text를 만든다.

- Issue body chunk: `repo`, `issue #`, `title`, `labels`, `state` metadata prefix + body section
- Comment chunk: parent issue context prefix + comment body
- Wiki chunk: page title/path + heading chain + section body
- Optional: 긴 issue thread summary chunk

### 8.3 Ranking metadata

qmd RRF score에 다음 boost/filter를 추가한다.

- Exact lookup: `#123`, full URL, issue title exact phrase는 최우선
- Repo filter: 명시된 repo는 hard filter
- Label/state/author/milestone: 명시된 경우 hard filter 또는 high boost
- Recency: 너무 강하면 오래된 결정 문서가 밀리므로 modest boost만
- Reactions/comment count: signal로 쓸 수 있지만 popularity bias를 조심
- Source type: 질문형 query는 wiki/issue body를 우선, “누가/언제 말했나”는 comment도 우선

### 8.4 Reranking 필요 여부

MVP에서는 rerank를 optional로 둔다.

- 필요해지는 경우: “왜 이 결정을 했지?”, “이 에러와 관련된 논의 찾아줘” 같은 semantic query
- 덜 필요한 경우: issue number, label, exact title, wiki page lookup
- local rerank: private data에 적합하지만 latency와 model install 부담
- hosted rerank: 품질/운영은 좋지만 private content 전송 문제가 있다.

### 8.5 답변용 검색과 원문 링크 검색 분리

분리해야 한다.

- **Answer mode**: chunk-level semantic retrieval, rerank, snippet 중심. Agent가 답변 근거를 찾는 목적.
- **Source lookup mode**: exact issue/wiki/comment URL, title, number, label 중심. 개발자가 원문 링크를 빨리 여는 목적.

둘을 한 ranking 함수에 섞으면 exact lookup이 semantic rerank에 밀리거나, 반대로 답변용 검색이 title exact matching에 과적합될 수 있다.

## 9. 저장소/엔진 후보 비교

| 엔진 | 설치/운영 | 성능/확장 | 로컬 적합성 | 서버 적합성 | agent integration | 판단 |
|---|---|---|---|---|---|---|
| SQLite FTS5 + `sqlite-vec` | 가장 낮음. 단일 파일 | 중소 repo에 충분. single-writer와 extension 이슈 | 매우 높음 | 낮음~중간 | 매우 좋음 | MVP 기본값 |
| SQLite FTS5 + `sqlite-vss` | 낮음 | 프로젝트가 `sqlite-vec`로 이동 중이라는 신호가 있음 | 중간 | 낮음 | 중간 | 새 프로젝트에는 비추천 |
| Postgres + pgvector + FTS | 중간 | shared service, ACL, transaction, queue와 잘 맞음 | 낮음 | 높음 | 좋음 | 서버형 1순위 |
| Tantivy/Lucene 계열 | 중간 | lexical search 강함 | 중간 | 중간~높음 | 별도 vector/fusion 필요 | 특수 검색 품질 요구 시 |
| Meilisearch | 낮음~중간 | 빠른 검색 UX, hybrid 기능 | 낮음 | 중간 | API 쉬움 | 운영 단순 서버 후보 |
| Typesense | 낮음~중간 | vector/hybrid/rank fusion 지원 | 낮음 | 중간 | API 쉬움 | 서버 검색 후보 |
| OpenSearch | 높음 | 대규모 vector/neural/hybrid 검색 | 낮음 | 매우 높음 | 무겁지만 강력 | 대규모 org 단계 |

**판단**

- 1~2주 MVP: SQLite FTS5 + `sqlite-vec`
- shared service v1: Postgres + pgvector + Postgres FTS
- 대규모/전담 운영: OpenSearch 또는 Typesense/Meilisearch 검토

## 10. 임베딩 모델, 비용, 프라이버시

### 10.1 Local embedding

장점:

- private issue/wiki가 외부 API로 나가지 않는다.
- qmd처럼 model fingerprint와 local vector store를 결합하기 쉽다.
- GitHub token과 content를 사용자 로컬에 둘 수 있다.

단점:

- 모델 다운로드가 크다. qmd README 기준 embedding model 약 300MB, reranker 약 640MB, query expansion model 약 1.1GB다.
- CPU 환경에서는 초기 embedding과 rerank latency가 부담될 수 있다.
- 한국어/혼합 언어 검색 품질은 모델 선택에 따라 차이가 크다.

### 10.2 Hosted embedding

장점:

- 설치가 단순하고 대량 batch 처리/품질/속도가 안정적일 수 있다.
- OpenAI embedding docs 기준 `text-embedding-3-small` 기본 dimension은 1536, `text-embedding-3-large`는 3072이며, `dimensions` parameter로 줄일 수 있다.
- OpenAI embedding docs는 입력 token 기준 과금을 명시하고, 800 token/page 가정의 pages-per-dollar 예시를 제공한다. 이 예시는 `text-embedding-3-small` 약 62,500 pages/$, `text-embedding-3-large` 약 9,615 pages/$다.
- Voyage AI 공식 pricing은 2026-06-27 확인 기준 `voyage-4-lite` $0.02/1M tokens, `voyage-4` $0.06/1M, `voyage-4-large` $0.12/1M, 일부 모델 200M free tokens를 제시한다.

단점:

- private issue/wiki 내용을 외부 provider로 보낸다.
- token retention, training use, data residency, regional processing, SOC2/enterprise terms를 조직별로 검토해야 한다.
- 비용은 document size, re-embedding 빈도, rerank 사용량에 따라 늘어난다.

**판단**: private repo 기본값은 local embedding이어야 한다. hosted embedding은 `--embedding-provider hosted`처럼 명시적으로 선택하게 하고, repo/org 단위 policy allowlist를 둔다.

### 10.3 권한 분리

local single-user index는 token access가 사실상 ACL이다. 하지만 shared service는 다르다.

- chunk마다 owner/repo, visibility, source entity, permission scope를 저장해야 한다.
- query 시점에 사용자별 GitHub 권한을 재확인하거나, installation/user token 권한을 반영해야 한다.
- 저장된 vector도 원문 복원이 어렵더라도 sensitive derivative data로 취급해야 한다.

## 11. 가장 큰 걸림돌

1. **ACL**: 서버형에서 가장 어렵다. ingest 가능한 content와 사용자에게 보여줄 수 있는 content가 다를 수 있다.
2. **stale index**: GitHub API/webhook을 써도 comment delete, wiki rename/delete, issue transfer는 periodic reconciliation이 필요하다.
3. **secondary rate limit**: 단순히 primary quota만 계산하면 부족하다. queue, backoff, conditional request, low concurrency가 필요하다.
4. **Wiki edge case**: Wiki는 Git repo라 sync는 가능하지만, rename/delete/history/default branch/live page 규칙을 명확히 해야 한다.
5. **검색 품질 평가**: “좋은 답변 근거”와 “정확한 원문 링크”는 평가 기준이 다르다. curated query set이 필요하다.
6. **운영 복잡도**: 서버형은 auth, webhook delivery, retry, embedding job, migration, metrics가 필요하다.
7. **scale**: repo 수가 늘면 full sync와 re-embedding 비용이 커진다. repo allowlist와 incremental sync가 필수다.
8. **privacy/cost**: hosted embedding/rerank를 쓰면 비용보다 privacy review가 먼저 병목이 될 가능성이 높다.

## 12. MVP 제안

### 12.1 가장 작은 구현 단위

**한 repo의 Issues + Wiki를 대상으로 하는 읽기 전용 local index + CLI + MCP prototype**.

범위:

- repo 1개 또는 명시적 repo allowlist 소수
- Issues title/body/state/labels/milestone/assignees/author/updated_at/html_url
- Issue comments body/author/updated_at/html_url
- Wiki latest default branch Markdown files
- SQLite FTS5 + `sqlite-vec`
- local embedding 기본
- CLI: `sync`, `search`, `get`, `status`
- MCP: `query`, `get`, `status`

### 12.2 인증 방식

- 로컬 prototype: fine-grained PAT 또는 GitHub CLI token 사용. 필요한 최소 권한은 repo Issues read 및 Wiki clone 가능한 repo access.
- 서버형 prototype: GitHub App installation token. Webhook을 쓰려면 `issues`, `issue_comment`, `gollum` event 구독.

### 12.3 Sync 방식

- Issues: REST list repository issues, `state=all`, `per_page=100`, Link pagination
- Comments: REST list issue comments for repository, `per_page=100`, `since`
- Conditional request: endpoint/page ETag 저장
- Wiki: `{repo}.wiki.git` clone/fetch, commit diff로 changed/deleted file 반영
- Reconciliation: MVP에서는 daily full metadata sweep 또는 최근 N일 sweep

### 12.4 Prototype plan

1. Source schema 정의: issue, comment, wiki page, chunk, embedding, sync state
2. REST issue/comment backfill 구현
3. Wiki clone/fetch + Markdown chunking 구현
4. SQLite FTS5 lexical search 구현
5. embedding generation + `sqlite-vec` vector search 구현
6. RRF hybrid ranking + exact lookup mode 구현
7. `get`으로 GitHub URL/원문 body/comment/wiki section 조회
8. MCP `query/get/status` 연결
9. 20~30개 curated query로 top-5 quality 평가

### 12.5 반드시 제외할 기능

- 조직 전체 repo 자동 discovery
- shared server ACL
- hosted embedding 기본값
- GitHub write-back, issue 생성/수정
- PR/Discussions/Projects 통합
- 모든 timeline event indexing
- finetuned query expansion
- 웹 UI
- OpenSearch 등 대형 검색 인프라

### 12.6 MVP 성공 기준

- 한 repo의 Issues + Wiki를 backfill하고 GitHub primary/secondary rate limit에 걸리지 않는다.
- issue body/comment edit와 wiki page edit가 incremental sync에 반영된다.
- 20~30개 known query에서 top 5 안에 올바른 issue/comment/wiki page가 나온다.
- `get` 결과가 canonical GitHub URL과 원문 identifier를 제공한다.
- private content가 기본 설정에서 외부 embedding/rerank API로 나가지 않는다.
- `status`가 stale/missing embeddings, last sync, wiki commit, API quota 상태를 보여준다.

## 13. 최종 판단 메모

**결정**: 조건부 Go.

**왜 가능한가**

- GitHub Issues는 REST API로 필요한 대부분의 검색 대상 데이터를 수집할 수 있다.
- GitHub Wiki는 별도 Git repo라 clone/fetch/diff 기반 sync가 가능하다.
- qmd가 이미 local-first, FTS+vector, rerank, MCP, agent workflow의 좋은 reference architecture를 보여준다.
- MVP는 저장소/운영 범위를 줄이면 1~2주 내 검증 가능하다.

**왜 조건부인가**

- shared server와 조직 전체 검색은 ACL 때문에 난이도가 급격히 올라간다.
- GitHub API secondary limit과 삭제/rename/stale 처리 때문에 “완전 실시간/완전 정확”은 초기 목표로 부적절하다.
- hosted embedding은 기술보다 보안/프라이버시 의사결정이 먼저 필요하다.

**추천 architecture**

- 로컬 CLI + MCP
- REST Issues sync + Wiki git sync
- SQLite FTS5 + `sqlite-vec`
- local embedding 기본
- qmd식 `query/get/status` workflow
- 서버형 전환 시 GitHub App + Postgres/pgvector

**피해야 할 architecture**

- GitHub Search API query-time proxy
- Wiki scraping
- 처음부터 organization-wide central index
- ACL 없이 private content를 공유 index에 넣는 구조
- qmd filesystem abstraction을 무리하게 Issues에 그대로 적용하는 구조

**추가 리서치가 필요한 불확실성**

- 실제 조직 repo의 issue/comment volume과 update frequency
- private repo 정책상 hosted embedding 허용 여부
- GHES 사용 여부와 instance별 rate limit 설정
- 한국어/영어 혼합 corpus에서 local embedding 모델 품질
- GitHub Wiki delete/rename webhook coverage의 실제 동작
- 사용자별 ACL을 query-time에 얼마나 엄격히 반영해야 하는지

## 14. 내 판단

나는 이 프로젝트를 **시작할 가치가 있다**고 본다. 단, 시작점은 “GitHub 문서 검색 플랫폼”이 아니라 **qmd의 agent workflow를 GitHub Issues/Wiki에 맞게 이식한 로컬-first 검색 도구**여야 한다.

첫 1~2주는 기술 리스크를 검증하기에 충분하다. Issues REST sync, Wiki git sync, SQLite hybrid search, MCP `query/get/status`까지 붙여 보면 핵심 가설을 확인할 수 있다. 그 결과가 좋으면 GitHub App/server/ACL로 확장하고, 결과가 나쁘면 큰 운영 투자를 하기 전에 멈출 수 있다.

반대로 처음부터 조직 전체 shared service를 만들면 검색 엔진보다 권한/운영/동기화 문제가 먼저 프로젝트를 잡아먹을 가능성이 높다. 그래서 내 결론은 **“MVP는 Go, 서버형 제품화는 MVP 품질과 ACL 요구를 확인한 뒤 재판단”**이다.

## 15. 주요 근거 링크

### qmd

- qmd repository: https://github.com/tobi/qmd
- qmd README at inspected commit: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/README.md
- qmd CLI: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/cli/qmd.ts
- qmd store/index/search: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/store.ts
- qmd DB/sqlite-vec loading: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/db.ts
- qmd local models: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/llm.ts
- qmd MCP server: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/mcp/server.ts
- qmd config/collections: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/collections.ts
- qmd structured syntax: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/docs/SYNTAX.md
- qmd example config: https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/example-index.yml

### GitHub 공식 문서

- REST Issues: https://docs.github.com/rest/issues/issues
- REST Issue comments: https://docs.github.com/rest/issues/comments
- REST Timeline events: https://docs.github.com/en/rest/issues/timeline
- REST Reactions: https://docs.github.com/en/rest/reactions/reactions
- REST rate limits: https://docs.github.com/rest/using-the-rest-api/rate-limits-for-the-rest-api
- REST best practices: https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
- REST pagination: https://docs.github.com/en/rest/using-the-rest-api/using-pagination-in-the-rest-api
- REST Search: https://docs.github.com/en/rest/search/search
- GraphQL rate/query limits: https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api
- GraphQL pagination: https://docs.github.com/en/graphql/guides/using-pagination-in-the-graphql-api
- GraphQL Issues reference: https://docs.github.com/en/graphql/reference/issues
- GitHub Wikis overview: https://docs.github.com/en/communities/documenting-your-project-with-wikis/about-wikis
- Adding/editing Wiki pages: https://docs.github.com/en/communities/documenting-your-project-with-wikis/adding-or-editing-wiki-pages
- Wiki access permissions: https://docs.github.com/en/communities/documenting-your-project-with-wikis/changing-access-permissions-for-wikis
- Webhooks overview: https://docs.github.com/en/webhooks/about-webhooks
- Webhook events: https://docs.github.com/en/webhooks/webhook-events-and-payloads
- GHES REST endpoint: https://docs.github.com/en/enterprise-server@3.19/rest/using-the-rest-api/getting-started-with-the-rest-api
- GHES GraphQL endpoint: https://docs.github.com/en/enterprise-server@3.19/graphql/guides/forming-calls-with-graphql
- GHES REST rate limits: https://docs.github.com/en/enterprise-server@3.19/rest/using-the-rest-api/rate-limits-for-the-rest-api

### Search/storage/model docs

- SQLite FTS5: https://www.sqlite.org/fts5.html
- sqlite-vec: https://github.com/asg017/sqlite-vec
- sqlite-vss: https://github.com/asg017/sqlite-vss
- pgvector: https://github.com/pgvector/pgvector
- PostgreSQL text search functions: https://www.postgresql.org/docs/current/functions-textsearch.html
- Meilisearch hybrid search: https://www.meilisearch.com/docs/capabilities/hybrid_search/overview
- Typesense vector search: https://typesense.org/docs/30.2/api/vector-search.html
- OpenSearch vector search: https://docs.opensearch.org/latest/vector-search
- OpenSearch hybrid search: https://docs.opensearch.org/latest/vector-search/ai-search/hybrid-search/index
- OpenAI embeddings guide: https://developers.openai.com/api/docs/guides/embeddings
- OpenAI API pricing: https://openai.com/api/pricing/
- Voyage AI pricing: https://docs.voyageai.com/docs/pricing

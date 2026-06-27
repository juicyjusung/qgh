# qgh MVP PRD

Status: ready-for-agent
Created: 2026-06-27 KST
Updated: 2026-06-28 KST

Document role: local issue-tracker working PRD for `/to-issues`.
Canonical source: `qgh-prd.md`.
Related brief: `qgh-product-brief.md`.
Update rule: keep this file implementation-ready for agent issue generation. If MVP requirements or acceptance criteria change, update `qgh-prd.md` first, then sync this working PRD.

## Problem Statement

개발자와 AI coding agent는 GitHub Issues, issue comments, Wiki에 흩어진 의사결정, 장애 대응, 작업 맥락, 운영 지식을 빠르게 찾아야 한다. 하지만 GitHub native search만으로는 comments와 Wiki까지 같은 retrieval workflow로 다루기 어렵고, agent가 반복 검색하면 query-time GitHub rate limit과 cloud 의존성이 생기며, snippet만 보고 답하면 원문 확인 없는 citation이 만들어진다.

private repo 사용자는 hosted embedding, hosted rerank, telemetry, shared server에 민감하다. 따라서 초기 문제는 범용 GitHub RAG나 조직형 검색 플랫폼이 아니라, 명시적으로 허용한 repo의 Issues/comments/Wiki를 로컬에 동기화하고, 검색 결과를 answer가 아닌 source candidate로 다루며, stable source identity를 통해 `get`과 canonical GitHub URL citation까지 결정적으로 이어지는 read-only retrieval workflow를 만드는 것이다.

## Solution

qgh는 GitHub Issues/Wiki용 local-first read-only CLI/MCP retrieval tool로 제공한다. 사용자는 profile에 GitHub host, token source, local DB, repo allowlist를 명시하고 CLI에서 `sync`를 실행한다. qgh는 Issues title/body/metadata, issue comments, Wiki latest branch Markdown content를 로컬 SQLite 인덱스로 저장한다.

사용자와 agent는 CLI 또는 MCP에서 `query`, `get`, `status`를 사용한다. `query`는 source candidate만 반환하고, 모든 result는 stable `source_id`, entity type, canonical URL, snippet, `get_args`, source version/staleness metadata를 포함한다. `get`은 authoritative issue body, comment, wiki page/section content와 canonical URL을 반환한다. Citation은 `get` 결과를 근거로 한다.

MVP의 기본 검색 경로는 BM25-only로 완성한다. Vector/hybrid search, hosted provider, shared server, Web UI, write-back은 MVP release gate 밖이다. MCP v1은 read-only `query`, `get`, `status`만 제공한다.

## User Stories

1. As a 개발자, I want 명시한 repo의 Issues를 로컬에 동기화하고 싶다, so that 과거 버그와 결정 배경을 GitHub UI보다 빠르게 찾을 수 있다.
2. As a 개발자, I want issue comments까지 검색하고 싶다, so that issue body에는 없는 토론과 장애 대응 맥락을 찾을 수 있다.
3. As a 개발자, I want GitHub Wiki latest content를 검색하고 싶다, so that runbook과 architecture note를 issue 검색과 같은 흐름으로 확인할 수 있다.
4. As a 개발자, I want `query` 결과가 원문 후보로 표시되길 원한다, so that snippet을 답변으로 착각하지 않는다.
5. As a 개발자, I want 검색 결과에서 바로 `get`을 호출할 수 있다, so that authoritative source를 확인한 뒤 판단할 수 있다.
6. As a 개발자, I want canonical GitHub URL을 함께 받고 싶다, so that browser에서 원문과 토론 맥락을 다시 확인할 수 있다.
7. As a 개발자, I want issue number로 exact lookup을 하고 싶다, so that 특정 issue를 ranking noise 없이 찾을 수 있다.
8. As a 개발자, I want full GitHub URL로 exact lookup을 하고 싶다, so that 외부 문서나 대화에서 받은 링크를 바로 source로 해결할 수 있다.
9. As a 개발자, I want repo, label, state, author, wiki path filter를 hard filter로 쓰고 싶다, so that 검색 범위가 query expansion으로 넓어지지 않는다.
10. As a 개발자, I want 한국어/영어 mixed query가 동작하길 원한다, so that 실제 repo 언어 습관과 맞는 검색 결과를 얻을 수 있다.
11. As a 개발자, I want no-result와 error가 구분되길 원한다, so that 검색 실패와 도구 실패를 다르게 처리할 수 있다.
12. As a 개발자, I want 삭제되거나 이동된 source가 검색에서 사라지길 원한다, so that stale ghost result를 citation하지 않는다.
13. As a 개발자, I want edited issue/comment/wiki content가 다음 sync 뒤 반영되길 원한다, so that 오래된 body를 근거로 삼지 않는다.
14. As a 개발자, I want source version과 indexed time을 보고 싶다, so that local index가 얼마나 stale할 수 있는지 판단할 수 있다.
15. As a 개발자, I want `status`에서 last sync와 source count를 보고 싶다, so that 현재 index 상태를 빠르게 진단할 수 있다.
16. As a 개발자, I want `status`가 network probe나 model load 없이 빠르게 끝나길 원한다, so that agent workflow 중 status 호출이 부작용을 만들지 않는다.
17. As a 개발자, I want malformed config가 실패하길 원한다, so that typo가 wrong repo indexing이나 broad search로 이어지지 않는다.
18. As a 개발자, I want config에 literal GitHub token이 저장되지 않길 원한다, so that local config leak가 repo access leak로 이어지지 않는다.
19. As a 개발자, I want local DB와 log 파일이 single-user permission으로 생성되길 원한다, so that derived private content 노출을 줄일 수 있다.
20. As a 개발자, I want vector runtime이 없어도 core workflow가 동작하길 원한다, so that model install 실패가 MVP 사용을 막지 않는다.
21. As a 개발자, I want query latency가 local workflow에 맞게 낮길 원한다, so that agent와 shell에서 반복 호출해도 부담이 작다.
22. As a 개발자, I want search score가 confidence로 보이지 않길 원한다, so that ranking evidence와 answer correctness를 혼동하지 않는다.
23. As a AI coding agent 사용자, I want MCP에서 `query`, `get`, `status`만 노출되길 원한다, so that agent가 sync, embed, write-back 같은 작업을 임의로 실행하지 않는다.
24. As a AI coding agent 사용자, I want MCP tools가 read-only hint와 strict schemas를 갖길 원한다, so that agent client가 도구를 안전하게 선택할 수 있다.
25. As a AI coding agent 사용자, I want MCP validation error가 structured error로 오길 원한다, so that stale tool call이나 typoed parameter를 성공 결과로 오해하지 않는다.
26. As a AI coding agent 사용자, I want MCP stdout이 protocol message만 포함하길 원한다, so that JSON-RPC framing이 diagnostics로 깨지지 않는다.
27. As a AI coding agent 사용자, I want every query result to round-trip through `get`, so that final answer citations are based on retrieved source content.
28. As a AI coding agent 사용자, I want comment result가 parent issue context를 포함하길 원한다, so that comment-only answer도 issue 맥락을 잃지 않는다.
29. As a AI coding agent 사용자, I want wiki section result가 page path와 heading context를 포함하길 원한다, so that citation이 GitHub Wiki 위치와 연결된다.
30. As a AI coding agent 사용자, I want stale index warning을 schema상 구분하고 싶다, so that answer에 source freshness caveat를 반영할 수 있다.
31. As a maintainer, I want repo allowlist가 explicit이길 원한다, so that 조직 전체 private corpus가 실수로 인덱싱되지 않는다.
32. As a maintainer, I want CLI와 MCP가 같은 profile과 DB를 보길 원한다, so that 두 인터페이스가 서로 다른 corpus를 검색하지 않는다.
33. As a maintainer, I want sync가 GitHub rate limit과 retry headers를 존중하길 원한다, so that backfill 중 token이 차단되거나 abuse limit에 걸리지 않는다.
34. As a maintainer, I want rate-limit/backoff state를 볼 수 있길 원한다, so that sync가 느린 이유를 진단할 수 있다.
35. As a maintainer, I want Wiki disabled/empty/auth failure가 구분되길 원한다, so that Wiki coverage 문제를 정확히 해결할 수 있다.
36. As a maintainer, I want issue/comment/wiki delete와 transfer를 reconciliation으로 감지하길 원한다, so that source lifecycle이 검색 품질과 privacy를 깨지 않는다.
37. As a maintainer, I want acceptance criteria가 schema tests와 fixture tests로 전환 가능하길 원한다, so that AFK agent가 구현 범위를 명확히 잡을 수 있다.
38. As a maintainer, I want release artifact와 generated schemas가 같은 contract를 설명하길 원한다, so that docs drift가 agent misuse를 만들지 않는다.
39. As a maintainer, I want curated 20~30 query eval을 갖고 싶다, so that BM25 baseline이 실제 use case를 통과하는지 확인할 수 있다.
40. As a maintainer, I want negative query class를 평가하고 싶다, so that false-positive top result를 confidence처럼 쓰는 실패를 줄일 수 있다.
41. As a maintainer, I want CJK/mixed query class를 따로 평가하고 싶다, so that Korean corpus 품질을 영어 query와 섞어 숨기지 않는다.
42. As a 보안/플랫폼 담당자, I want default mode에서 GitHub host 외 outbound call이 없길 원한다, so that private repo content가 third-party provider로 나가지 않는다.
43. As a 보안/플랫폼 담당자, I want hosted embedding/rerank가 explicit opt-in 없이는 비활성화되길 원한다, so that 품질 기능이 privacy egress로 바뀌지 않는다.
44. As a 보안/플랫폼 담당자, I want MCP v1이 GitHub write permission 없이 동작하길 원한다, so that least-privilege agent integration이 가능하다.
45. As a 보안/플랫폼 담당자, I want local snippets, DB, logs, embeddings를 sensitive derivative data로 취급하길 원한다, so that private content의 2차 노출 리스크를 낮출 수 있다.
46. As a post-MVP planner, I want vector/hybrid가 BM25 path를 깨뜨리지 않길 원한다, so that semantic recall 개선이 core workflow 안정성을 해치지 않는다.
47. As a post-MVP planner, I want embedding fingerprint와 partial coverage state가 정의되길 원한다, so that later vector mode가 mixed model/schema 상태로 잘못 ranking하지 않는다.
48. As a GHES 사용자, I want GHES endpoint가 profile capability로 표현되길 원한다, so that github.com이 아닌 환경도 later 검증할 수 있다.
49. As a shell automation 사용자, I want machine-readable JSON과 human-readable output이 분리되길 원한다, so that automation이 human text로 깨지지 않는다.
50. As a first-time user, I want setup error가 actionable하게 실패하길 원한다, so that missing token source, malformed repo, unknown key를 빠르게 고칠 수 있다.

## Implementation Decisions

- MVP는 GitHub.com first-class로 구현하고 GHES는 profile-level capability로 표현하되 release gate에서 제외한다.
- Scope는 Issues title/body/metadata, issue comments, Wiki latest branch content로 제한한다.
- Repo selection은 explicit allowlist만 지원한다. Organization-wide discovery, implicit fallback, current-directory inference는 제외한다.
- Profile은 GitHub host, token source, local DB path, repo allowlist, schema/profile id를 고정한다.
- Token은 config에 literal value로 저장하지 않고 source reference로만 다룬다. MVP profile은 `token_source`를 명시해야 하며, `qgh init`의 추천값은 `github_cli`다. Runtime fallback으로 GitHub CLI -> environment -> OS credential store를 자동 순회하지 않는다. 다른 source를 쓰려면 profile에 `env` 또는 `credential_store`를 명시한다. GitHub App installation token은 서버형/post-MVP capability로 둔다.
- Sync는 CLI explicit command로만 제공한다. MCP는 sync, embed, delete, update, write-back tool을 제공하지 않는다.
- Issues/comments sync는 REST 기반 backfill과 `since` incremental sync를 기본으로 한다.
- Wiki sync는 `{repo}.wiki.git` clone/fetch 기반 latest branch content를 기본으로 한다.
- Source identity는 URL, title, issue number, wiki path와 분리한다. Mutable locator는 alias/version/lifecycle state로 다룬다.
- Source model은 issue, comment, wiki page/section의 parent-child 관계를 표현한다.
- Source version은 body hash, updated timestamp 또는 wiki commit, indexed timestamp를 포함한다.
- Deletion and transfer handling은 tombstone, last-seen tracking, `get` 404/410/redirect handling, bounded reconciliation으로 처리한다. 일반 `sync`는 cheap lifecycle check(issue `updated_at`/comment count diff, wiki git tree diff)를 수행하고, full reconciliation은 기본 manual command(`qgh sync --reconcile full`)로 둔다. Profile은 optional `reconcile_after_days`를 가질 수 있으며, 기본값은 자동 실행이 아니라 stale warning이다.
- BM25-only path는 `sync`, `query`, `get`, `status` 전체 workflow를 완성해야 한다.
- Vector/hybrid search는 post-MVP capability다. M6에서만 prototype 여부를 결정하며, first candidate는 local ONNX embedding runtime + SQLite vector index다. Hosted embedding/rerank는 explicit policy/opt-in 전에는 후보가 아니다. Fingerprint와 partial coverage schema는 BM25 path를 깨뜨리지 않는 범위에서만 설계한다.
- Query response는 `source_id`, `entity_type`, canonical URL, snippet, `get_args`, parent context, ranking evidence, source version/staleness metadata를 포함한다.
- `get` response는 issue body, comment body, wiki page/section content와 canonical URL, parent context, source version을 반환한다.
- Snippet은 preview이며 citation 근거가 아니다. Citation contract는 `query -> get -> cite`다.
- Exact lookup과 structured filters는 semantic rewrite/query expansion의 영향을 받지 않는 hard constraints로 처리한다.
- Ranking signals는 lexical, vector, rerank, final ordering evidence처럼 typed field로 분리하고 confidence/probability로 명명하지 않는다.
- CLI는 `sync`, `query`, `get`, `status`를 제공한다. `search`는 `query` alias로 둘 수 있다.
- CLI output은 machine-readable JSON과 human-readable output을 명확히 분리한다.
- MCP v1은 read-only `query`, `get`, `status` tools만 expose하고 strict input/output schemas와 `readOnlyHint`를 제공한다.
- Config, CLI, MCP schemas는 strict validation을 적용한다. Unknown keys, typoed params, malformed JSON, invalid enum은 structured error로 실패한다.
- MCP stdout은 protocol messages only로 유지하고 diagnostics는 stderr/log channel로 보낸다.
- `status`는 last sync, source count, stale/tombstone count, DB/schema version, profile id, Wiki commit, reconciliation age를 표시한다.
- `status`는 network probe, model load, expensive doctor check를 수행하지 않는다.
- Local DB, logs, cache files는 single-user permission으로 생성하고 sensitive derivative data로 문서화한다.
- Default mode는 GitHub host 외 outbound call을 하지 않는다. Hosted provider는 explicit opt-in 없이는 코드 경로가 활성화되지 않는다.
- SQLite local storage는 concurrent read와 explicit sync write를 안전하게 처리하도록 WAL, busy timeout, single-writer queue, migration lock, shadow publish 같은 운영 규칙을 갖는다.
- Search quality gate는 exact lookup, keyword/body/comment/wiki query, CJK/mixed query, negative query, `get` round-trip을 class별로 평가한다.

## Grilled Lock Decisions

이 섹션은 더 물어볼 필요 없이 implementation planning에서 그대로 쓰는 결정이다.

1. Token source default는 `github_cli`다. 단, profile에는 source reference가 명시되어야 하며 runtime fallback은 금지한다. Silent fallback은 wrong-account/wrong-repo indexing 위험이 크다.
2. Initial eval corpus는 synthetic fixture repo로 시작한다. 실제 private repo 익명화 corpus는 user validation용 보조 자료이며 release gate가 아니다. Synthetic fixture는 issue body, comment-only answer, wiki page/section, exact lookup, CJK/mixed, negative query를 모두 포함하고 gold `source_id`를 repo 안에 둔다.
3. CJK baseline은 FTS5 trigram + 1~2자 LIKE fallback이다. 형태소 tokenizer(예: Korean analyzer)는 CJK/mixed top-5 target 미달 또는 false-positive 분석에서 trigram 한계가 확인될 때 optional tier로 올린다.
4. Full reconciliation은 hidden background work가 아니다. MVP는 explicit CLI sync product이므로 full reconciliation도 explicit/manual command로 시작한다. `status`는 마지막 full reconciliation 시각, stale warning, estimated API cost class를 보여준다.
5. Wiki status vocabulary는 `ok`, `disabled`, `empty`, `auth_error`, `not_found`, `clone_error`, `too_large_warning`으로 고정한다. `disabled`는 repo metadata가 wiki off임을 확인한 경우에만 쓰고, clone 401/403은 `auth_error`, clone 404/ambiguous failure는 `not_found` 또는 `clone_error`로 구분한다.
6. Server productization은 MVP 후 gate다. 최소 조건은 local MVP에서 search quality gate 통과, 3~5명 user validation에서 repeated workflow 가치 확인, shared index/ACL 요구가 local duplicate-index 비용보다 큰 evidence 확보, GitHub App/token 운영 모델 정의다.
7. GitHub native search 대비 positioning은 privacy가 아니라 coverage/comments/Wiki, local repeat query, offline/local operation, deterministic `query -> get -> cite`다. Privacy는 third-party hosted RAG/embedding 대비 차별점으로만 주장한다.
8. Snippet-only 억제는 protocol hard guarantee가 아니다. Schema, docs, eval, MCP prompt contract로 유도하되 final citation correctness gate는 `get` round-trip이다.

## Testing Decisions

- 가장 높은 test seam은 product contract seam이다: `sync -> query -> get -> cite -> status`를 CLI와 MCP 양쪽에서 검증한다.
- 좋은 테스트는 implementation detail이 아니라 외부 행동을 검증한다. Result shape, structured error, source identity stability, citation round-trip, staleness metadata, no-egress behavior를 관찰한다.
- Sync tests는 fake GitHub REST server와 local Wiki git fixture를 사용한다.
- Sync fixture는 pagination, backfill, `since` incremental update, issue body edit, comment edit, wiki edit, comment delete, wiki rename/delete, issue transfer/unavailable state, rate-limit/backoff, ETag/304를 포함한다.
- Retrieval contract tests는 모든 top-k query result가 `get`으로 round-trip하고 canonical URL, parent context, source version을 포함하는지 검증한다.
- Schema tests는 config, CLI JSON, MCP input/output, no-result, validation error, auth error, stale warning, rate-limit/backoff state를 snapshot 또는 generated schema로 검증한다.
- MCP tests는 exposed tool list가 `query`, `get`, `status`만 포함하고 each tool이 read-only annotation과 schemas를 갖는지 검증한다.
- MCP stdio tests는 stdout에 protocol message 외 diagnostics가 섞이지 않는지 검증한다.
- Privacy tests는 mocked network로 default sync/search 중 GitHub host 외 outbound call이 발생하지 않는지 검증한다.
- Token tests는 config, fixtures, logs, generated docs에 literal GitHub token이 기록되지 않는지 검증한다.
- File permission tests는 DB/log/cache가 single-user permission으로 생성되는지 검증한다.
- DB safety tests는 concurrent CLI sync와 MCP query/status, migration race, crash during FTS rebuild, shadow publish behavior를 검증한다.
- Search quality tests는 curated 20~30 query set으로 exact lookup top-1, keyword/comment/wiki top-5, CJK/mixed top-5, negative query abstention, top-k `get` round-trip을 측정한다.
- BM25 fallback tests는 vector extension, local model cache, GPU/runtime이 없는 환경에서도 `sync`, BM25 `query`, `get`, `status`가 통과하는지 검증한다.
- Performance tests는 warm local DB에서 10k sources 또는 50k chunks 기준 BM25 p95 query latency를 측정하고 cold-start latency를 별도로 기록한다.
- Prior art는 기존 source truth의 qmd evidence에서 온다: source identity vs locator 분리, query-result round-trip, strict schema, BM25-first graceful degradation, MCP stdout cleanliness, local DB migration safety, privacy no-egress.
- External contract tests should pin assumptions from primary docs: GitHub issue/comment REST pagination and `since`, GitHub REST rate-limit/retry behavior, GitHub Wiki git clone behavior, MCP tool schemas/annotations, and SQLite FTS5 trigram behavior.

## Out of Scope

- Organization-wide repo auto-discovery
- Shared server, central index, team index, remote sync
- Full ACL enforcement or query-time GitHub permission recheck
- GitHub write-back, issue create/edit, label mutation, Wiki edit
- PR, Discussions, Projects, Actions logs, full timeline events
- Web UI
- Watch daemon or webhook-based near-real-time sync
- Hosted embedding, hosted rerank, hosted query expansion as default
- OpenSearch, Meilisearch, Typesense, or other external search server
- Fine-tuned ranking or query expansion
- Wiki full history search
- Answer generation or autonomous summarization
- Vector/hybrid search as a release gate
- GHES as a release gate

## Further Notes

- MVP release gate should include the acceptance criteria from the existing source PRD except post-MVP vector and best-effort GHES gates.
- Search result success means `get` round-trip success. A result that cannot retrieve authoritative source content is not successful.
- Privacy positioning must stay precise: local-first privacy is a differentiator against third-party hosted RAG/embedding tools, not against GitHub native search where content already lives in GitHub.
- GitHub native semantic/hybrid issue search changes the positioning. qgh should emphasize comments/Wiki coverage, rate-limit-free local query, offline/local behavior, deterministic citation, and read-only MCP workflow.
- No open product decisions remain for MVP planning. Remaining uncertainty is validation work: real token/wiki behavior, actual CJK quality on fixture/eval corpus, and post-MVP vector/server gates.

## Validated External References

Checked 2026-06-27 while grilling this PRD. These are product-contract inputs, not implementation shortcuts.

- GitHub issue search GA positioning: https://github.blog/changelog/2026-04-02-improved-search-for-github-issues-is-now-generally-available/
- GitHub REST issues: https://docs.github.com/en/rest/issues/issues
- GitHub REST issue comments: https://docs.github.com/en/rest/issues/comments
- GitHub REST search/rate limits/best practices: https://docs.github.com/en/rest/search/search, https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api, https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
- GitHub Wiki git workflow: https://docs.github.com/en/communities/documenting-your-project-with-wikis/adding-or-editing-wiki-pages
- MCP tools/schema: https://modelcontextprotocol.io/specification/2025-11-25/server/tools, https://modelcontextprotocol.io/specification/2025-11-25/schema
- SQLite FTS5 trigram/unicode tokenizers: https://www.sqlite.org/fts5.html

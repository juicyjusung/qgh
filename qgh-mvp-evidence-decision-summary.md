# qgh MVP Evidence Decision Summary

작성일: 2026-06-27 KST
개정: 2026-06-29 KST — MVP scope reset: GitHub Issues와 issue comments만 포함하고 Wiki는 post-MVP source connector로 연기한다.
개정: 2026-06-29 KST — architecture lock: Rust single-binary, XDG explicit profile store, bundled SQLite authoritative store, Tantivy derived BM25 index.
개정: 2026-06-29 KST — grill closure: CLI-only doctor, REST sync scheduler contract, user-facing eval deferral.

대상 문서:

- `qmd-qgh-research-ledger.md`
- `qmd-qgh-loop-21-100-evidence.md`
- 참고 baseline: `github-issues-wiki-hybrid-search-go-no-go.md`

목적:

qmd의 실패 패턴을 qgh MVP 설계, 구현 범위, guardrail, 테스트로 번역한다. 이 문서는 두 evidence ledger의 단순 요약이 아니라, qgh의 초기 구조에 반영해야 하는 high-signal evidence만 남긴 decision document다.

## 1. 핵심 결론

- qgh는 GitHub URL, title, issue number를 primary identity로 쓰면 안 된다. MVP 스키마의 중심은 GitHub `node_id` 기반 qgh URI `source_id`, `source_alias`, `source_version`이어야 하며, URL/title/number는 mutable locator로만 저장한다.
- MVP 범위는 `Issues + issue comments`, `read-only`, `local-first`, `explicit repo allowlist`, `CLI/MCP`로 고정한다. Wiki connector, org-wide discovery, shared server, full ACL, hosted provider default는 MVP 밖이다.
- 제품 CLI/MCP는 Rust single-binary로 구현한다. agent 반복 호출 제품이므로 런타임 설치와 native extension drift를 MVP 리스크로 만들지 않는다.
- SQLite는 authoritative source/lifecycle store이고 Tantivy는 재생성 가능한 derived BM25 search index다. query 성능과 검색 품질은 Tantivy에서 얻고, source correctness는 SQLite에서 보장한다.
- Profile은 XDG config/data/cache 아래에 분리하고 모든 command는 `--profile`을 요구한다. SQLite/Tantivy data path는 profile id에서 파생하며 MVP에서는 DB path override를 제공하지 않는다. CWD, HOME, token env 같은 implicit fallback으로 corpus를 선택하지 않는다.
- Sync는 단순 incremental fetch가 아니라 lifecycle correctness 문제다. `watermark`, `last_seen_at`, `deleted_at`, tombstone, periodic reconciliation을 초기 설계에 넣어야 한다.
- Sync scheduler는 GitHub REST를 직접 다루며 `state=all`, `pull_request` 제외, Link pagination, ETag/304, 60초 watermark overlap, low-concurrency, bounded backoff를 MVP 계약으로 둔다.
- Tantivy BM25-only 경로가 반드시 동작해야 한다. sqlite-vec, local embedding model, reranker, GPU backend는 optional capability이며 `sync`, `query`, `get`, `status`의 필수 의존성이 되면 안 된다.
- MCP/CLI/config는 strict schema와 versioned output envelope로 간다. unknown param 무시, implicit CWD/repo/profile, malformed arg 성공, zero-entity sync 성공은 private GitHub 검색에서 치명적이다.
- `status`는 local-only snapshot이고 `doctor`는 CLI-only opt-in probe다. `eval`은 release/test harness로 남기고 user-facing CLI/MCP surface에는 넣지 않는다.
- 검색 결과는 답이 아니라 source 후보여야 한다. 모든 result는 `source_id`와 `get_args`를 포함하고, `get(source_id)`가 canonical URL과 authoritative issue body/comment를 돌려줘야 한다.
- Ranking score를 confidence로 노출하지 않는다. exact lookup, metadata filter, semantic rationale, negative query, CJK/mixed-language query를 분리 평가하고 score field도 typed로 분리한다.
- SQLite는 local authoritative store로 적합하지만 검색 인덱스와 lifecycle store를 섞으면 재빌드/마이그레이션 리스크가 커진다. MVP라도 WAL, busy timeout, single-writer queue, migration lock, Tantivy shadow rebuild, crash recovery test를 넣어야 한다.
- Hosted embedding/rerank/query expansion은 MVP default에서 제외한다. private issue/comment text와 metadata의 external egress는 explicit opt-in과 repo policy 이후에만 허용한다.
- qmd의 좋은 원칙은 `query -> get -> cite`, BM25-first graceful degradation, optional vector fingerprint, status/doctor/eval이다. 버릴 전제는 local file path 중심 abstraction, broad config surface, model stack hard dependency다.

## 2. 반드시 반영할 설계 원칙

| 원칙 | 근거 | qgh 적용 방식 | 안 지켰을 때 리스크 |
|---|---|---|---|
| Source identity와 locator 분리 | Research Ledger Loop 1-2, #520/#698/#717 | `source_id`는 GitHub `node_id` 기반 qgh URI. URL, title, issue number는 `source_alias`로 저장 | transfer, title edit 후 다른 원문을 반환하거나 기존 source를 overwrite |
| Source lifecycle first-class | Research Loop 4-5, Loop 82-83, #585 | `source_entity`, `source_version`, `source_alias`, tombstone, `sync_run`, `last_seen_at`, `deleted_at` 테이블 | 삭제된 private comment가 계속 검색되고 citation됨 |
| Explicit profile resolver | Research Loop 3, Loop 24-25, #343/#495/#615 | profile이 GitHub host, repo allowlist, token source, derived XDG data path, schema/profile id를 고정하고 `--profile` 없이 실패 | CLI와 MCP가 서로 다른 private repo corpus를 검색 |
| Read-only MCP v1 | Loop 30, 36, 91, PR #632/#646 | MCP tool은 `query`, `get`, `status`로 제한. `sync/embed/update`는 CLI explicit command | agent가 rate-limit, stale write, embedding job, private data egress를 유발 |
| Query-result round-trip contract | Research Loop 8, 13-14, Loop 94, #706 | 모든 search result에 `source_id`, `entity_type`, parent chain, `get_args`, canonical URL 포함 | snippet만 보고 hallucinated citation이 생기거나 원문 조회가 실패 |
| Strict schema and generated docs | Loop 12-13, 23, 95-96, #741/PR #715 | CLI/MCP/config schema에서 docs 생성. unknown key/param은 versioned error envelope로 실패 | agent가 stale tool name, typoed param, silent broad search를 수행 |
| BM25-first graceful degradation | Research Loop 17, Loop 41-42, 99, #699/#498 | no sqlite-vec/no model/no GPU 환경에서도 Tantivy BM25로 `sync/query/get/status` 통과 | 설치와 native model stack 문제가 MVP adoption을 막음 |
| Typed ranking signals | Research Loop 9-11, Loop 71, #591/#697/#747 | `lexical_score`, `vector_distance`, `rrf_rank_score`, `rerank_score`, `final_order_score` 분리 | irrelevant top result가 high-confidence처럼 보임 |
| Filter immutability | Research Loop 12, Loop 63, 74-75 | query expansion은 free-text에만 적용. `repo`, `state`, `label`, `author`, `issue`는 rewrite 금지 | expansion이 private repo scope나 metadata filter를 넓힘 |
| Safe local store/index operations | Research Loop 19, Adversarial Loop 11-12, #710/#736/PR #737/#745 | SQLite WAL, busy timeout, single-writer queue, migration lock, Tantivy shadow rebuild, atomic publish | `status` 또는 DB open 중 search index가 empty/corrupt |
| Versioned docs/schema/release | Adversarial Loop 10, Loop 85, PR #746 | `doctor`가 binary version, DB schema, docs/schema version, release/tag 상태 표시 | docs가 unreleased behavior를 설명해 agent와 user가 잘못 호출 |
| Privacy by default | Research Loop 18, Loop 39-40, Adversarial Loop 4 | hosted provider는 default off. repo policy와 explicit opt-in 없이는 GitHub 외 egress 금지 | private issue/comment content와 metadata가 외부 provider로 전송 |

## 3. MVP에서 잘라야 할 것

| 제외/연기할 기능 | 왜 위험한가 | qmd evidence | qgh 대안 |
|---|---|---|---|
| Organization-wide repo auto-discovery | wrong repo/private repo indexing, rate-limit, storage 폭발 | Loop 32, Loop 100 | 명시적 `repos` allowlist만 지원 |
| Shared server/central index/ACL | vectors와 snippets도 derivative private data라 query-time auth가 어려움 | Loop 37, Server/Org Later Risks | local single-user DB. 서버형은 MVP 품질 확인 후 별도 설계 |
| Hosted embedding/rerank default | private text/metadata external egress | Research Loop 18, Loop 39-40, PR #705/#725 | local/BM25 default. hosted는 policy allowlist와 opt-in 이후 |
| MCP mutation tools | agent가 expensive sync/embed를 반복하거나 잘못된 profile에서 mutation | Loop 30, 36, 91, PR #632 | CLI `sync`만 explicit repo/profile로 제공 |
| Watch daemon/webhook freshness | delete, retry, secondary rate limit, idempotency 모델이 먼저 필요 | Loop 36, Adversarial Loop 3 | manual sync + bounded reconciliation |
| PR/Discussions/Projects/Actions logs | source model과 permissions가 크게 달라 MVP 검증을 흐림 | Loop 67, Loop 100 | Issues와 issue comments만 |
| GitHub Wiki connector | git clone auth, page lifecycle, rename/delete diff가 별도 source connector 리스크를 만든다 | Baseline section 6, Scope reset 2026-06-29 | post-MVP connector로 연기 |
| Write-back, issue edit/create, label mutation | read 권한 제품에서 auth/audit/write safety로 범위 폭발 | Loop 100 | read-only retrieval |
| Remote sync/SSH/rsync/team index | credential, ACL, supply-chain, stale index risk 증가 | Loop 37, PR #725 | per-user local DB |
| Web UI | 핵심 source correctness, sync, retrieval contract와 무관 | Loop 100 | CLI + MCP first |
| OpenSearch/Typesense/Meilisearch | MVP 검증 전에 infra 의존성과 운영 비용 증가 | Baseline section 9, Loop 100 | bundled SQLite + Tantivy local first. 서버형 전환 때 재평가 |
| Query expansion/HyDE default | metadata filter 오염, exact lookup 방해, eval 필요 | Research Loop 12, Loop 74-76 | off by default. later opt-in free-text only |
| Finetuned expansion/ranking | data balance, overfitting, dependency/security surface | Loop 76 | curated eval 먼저 구축 |

## 4. MVP에 반드시 넣어야 할 guardrail

| guardrail | 막는 실패 모드 | 관련 evidence | 테스트 방법 |
|---|---|---|---|
| `source_id/source_alias/source_version` invariant | title/url drift, issue transfer | Research Loop 1-2, 5, #520/#698 | issue title edit와 transfer 후 source identity가 유지되고 locator만 alias로 갱신되는지 확인 |
| Tombstone + reconciliation | deleted comment ghost result | Research Loop 4, Loop 82-83, #585 | webhook 없이 comment 삭제를 fake server에 반영. incremental 후 uncertain, reconciliation 후 tombstone/search exclusion 확인 |
| Explicit profile guard | wrong index/private corpus | Research Loop 3, Loop 24-25, #343/#495 | CLI/MCP를 다른 `HOME`, `XDG_*`, token env로 실행. profile id 불일치 시 fail |
| Strict CLI/MCP/config validation | typoed param이 unscoped query로 확장 | Research Loop 12-13, Loop 21, 23 | singular/plural typo, unknown param, malformed repo, missing repo, zero entity sync는 structured error와 non-zero exit |
| Query to get contract suite | snippet-only citation, stale source handle | Research Loop 8, 13-14, 94 | eval top-k 모든 result를 `get`으로 재조회. snippet span, canonical URL, parent issue context 확인 |
| BM25-only required path | native/vector/model install failure | Research Loop 17, Loop 41-42, 99 | sqlite-vec/node-llama/model cache 없는 CI에서 `sync/query/get/status` pass. vector command는 actionable error |
| Embedding fingerprint gate | model/chunker/provider/source schema mismatch | Research Loop 6, Loop 57 | chunker/source prefix/model/dim 변경 후 old vectors pending 처리, mixed fingerprint search reject |
| Partial coverage tracking | issue body만 embedded되고 comments 누락 | Research Loop 7, Loop 52 | 100-comment issue에서 comment 50 embedding failure 주입. `status` partial, vector result exclude 또는 partial 표시, retry resume |
| Ranking/eval gates | high-looking false positive | Research Loop 9-11, 20, Loop 71 | exact issue lookup, label/state filters, semantic rationale, comment-only answer, CJK/mixed, negative queries로 per-class NDCG/FPR 측정 |
| SQLite/Tantivy migration safety | startup/status 중 empty/corrupt search index | Research Loop 19, Adversarial 11-12 | old profile을 N processes로 동시에 open, migration/rebuild 중 crash 주입. SQLite schema와 active Tantivy generation 확인 |
| JSON output envelope tests | human text가 agent JSON을 오염 | Loop 78-89, #594/#183 | no-result success, validation/auth/source/rate-limit error, status, query JSON 모두 envelope snapshot 검증 |
| Status/doctor split | `status`가 model build/probe를 수행 | Loop 48, 79, #491 | `status`는 no native load/no network except DB. `doctor`만 opt-in probes |
| Privacy no-egress | private content 외부 provider 전송 | Research Loop 18, Loop 39-40 | mocked network에서 default private repo indexing/search 중 GitHub host 외 호출 금지 |
| CJK/tokenizer eval | Korean/version/branch/code token miss | Loop 58-61, 75 | Tantivy tokenizer + CJK n-gram fallback으로 Korean/English mixed issue, `v1.2.3`, `CVE-2026`, `owner/repo`, `snake_case`, hyphenated tokens query |
| Release/schema package gate | docs/tool drift | Loop 85-97 | package version, changelog, tag, MCP tool snapshot, generated docs consistency check |

## 5. 가장 중요한 evidence 20개

| # | Evidence 요약 | source loop 또는 문서 위치 | 관련 qmd issue/PR/changelog/code 링크 | qgh에 주는 의미 |
|---:|---|---|---|---|
| 1 | qmd가 normalized filename을 저장해 원래 path와 collision risk를 만들었고, 뒤늦게 literal path 저장으로 고침 | Research Ledger Loop 1 | https://github.com/tobi/qmd/issues/520, https://github.com/tobi/qmd/pull/698, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/store.ts#L1401-L1407 | qgh는 URL/title/issue number를 identity로 쓰면 안 된다. identity와 display/locator를 분리 |
| 2 | path normalization 때문에 space/underscore/special-char path가 `get`, `ls`, editor URI에서 깨짐 | Research Ledger Loop 2 | https://github.com/tobi/qmd/issues/717, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/CHANGELOG.md#L66-L72 | title, labels, author names는 Unicode/special-token safe locator로만 취급 |
| 3 | MCP가 `--index`를 무시해 default DB를 열었고, 같은 class가 재발 | Research Ledger Loop 3, Loop 24 | https://github.com/tobi/qmd/issues/343, https://github.com/tobi/qmd/issues/691 | profile id가 CLI, daemon, MCP, SQLite store, Tantivy index 전체를 관통해야 함 |
| 4 | config path와 DB path의 home-directory semantics 차이로 CLI는 docs를 보고 MCP는 empty index를 봄 | Research Loop 3, Loop 25 | https://github.com/tobi/qmd/issues/495, https://github.com/tobi/qmd/issues/615 | qgh profile resolver는 XDG path를 명시적으로 resolve하고 env-dependent fallback 없이 실패해야 함 |
| 5 | qmd update가 removed files를 active로 남겨 756 ghost docs가 검색됨 | Research Loop 4, Loop 82-83 | https://github.com/tobi/qmd/issues/585, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/CHANGELOG.md#L263-L265 | qgh sync는 deletion/tombstone/reconciliation이 핵심 correctness path |
| 6 | legacy bad path를 고친 뒤에도 migration/alias handling이 필요했음 | Research Loop 5 | https://github.com/tobi/qmd/pull/698, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/store.ts#L1426-L1427 | 한번 잘못 저장한 identity는 장기 부채가 된다. alias/event history를 초기부터 설계 |
| 7 | embedding model이 다른 경로에서 무시되어 dimension mismatch가 발생 | Research Loop 6 | https://github.com/tobi/qmd/issues/497, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/src/store.ts#L1017-L1025 | fingerprint는 model string만이 아니라 provider/dim/tokenizer/chunker/source schema 포함 |
| 8 | long embedding run이 session timeout과 partial chunks를 만들었고, later fix는 complete chunk coverage를 요구 | Research Loop 7, Loop 51-52 | https://github.com/tobi/qmd/issues/724, https://github.com/tobi/qmd/issues/637, https://github.com/tobi/qmd/pull/654 | issue thread는 per-comment/per-version chunk completeness를 가져야 함 |
| 9 | qmd의 path/docid retrieval grammar가 issue/comment hierarchy와 맞지 않음 | Research Loop 8, 13-14 | https://github.com/tobi/qmd/issues/706, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/README.md#L154-L161 | qgh `get`은 qgh URI `source_id`와 parent context를 지원해야 함 |
| 10 | RRF weight bug가 original vector/FTS보다 expansion을 잘못 우선시함 | Research Loop 9 | https://github.com/tobi/qmd/issues/591, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/CHANGELOG.md#L245-L245 | qgh ranking candidate에는 source type, field hit, query mode provenance가 필요 |
| 11 | nonsense/off-topic query에도 blended score가 높아 threshold rejection이 불가능 | Research Loop 10, Loop 73 | https://github.com/tobi/qmd/issues/697 | final order score를 confidence로 쓰지 말고 negative-query abstention eval 필요 |
| 12 | reranker double-sigmoid로 score가 좁은 range에 압축됐지만 order-only test가 못 잡음 | Research Loop 11, 20, Loop 56 | https://github.com/tobi/qmd/issues/747 | score magnitude/distribution test가 필요. calibrated confidence는 eval 이후만 |
| 13 | query expansion/filter grammar와 docs drift가 agent misuse를 만들 수 있음 | Research Loop 12, Loop 74-75 | https://github.com/tobi/qmd/issues/741, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/docs/SYNTAX.md#L140-L150 | unknown MCP params reject, hard filters immutable |
| 14 | malformed Windows trailing backslash, comma mask, missing path가 false success 또는 implicit CWD를 만들었음 | Loop 21-22 | https://github.com/tobi/qmd/issues/738, https://github.com/tobi/qmd/issues/557, https://github.com/tobi/qmd/issues/684 | qgh `sync/init`은 malformed repo/profile과 zero-source sync를 fail loud 처리 |
| 15 | MCP stdout pollution이 JSON-RPC framing을 깨뜨림 | Loop 26 | https://github.com/tobi/qmd/issues/593, https://github.com/tobi/qmd/blob/e428df76bc0274d9e93eb7ca3e95673315c42e90/CHANGELOG.md#L246-L246 | MCP diagnostics는 stdout이 아니라 stderr/log channel만 사용 |
| 16 | MCP HTTP session/concurrency lifecycle bugs가 반복됨 | Loop 27-28 | https://github.com/tobi/qmd/issues/607, https://github.com/tobi/qmd/issues/163, https://github.com/tobi/qmd/pull/286 | qgh HTTP/server mode는 MVP later 또는 session/concurrency test 후만 |
| 17 | external provider, remote backend, hosted embedding 요구가 계속 나왔지만 privacy posture를 바꿈 | Research Loop 18, Loop 38-40 | https://github.com/tobi/qmd/issues/521, https://github.com/tobi/qmd/pull/705, https://github.com/tobi/qmd/pull/725 | hosted provider는 default가 아니라 governed opt-in |
| 18 | install/native/model/GPU failures가 반복되어 core UX를 막음 | Research Loop 17, Loop 41-42, 47-54, 99 | https://github.com/tobi/qmd/issues/699, https://github.com/tobi/qmd/issues/498, https://github.com/tobi/qmd/issues/735, https://github.com/tobi/qmd/pull/733 | BM25-only MVP와 graceful degradation이 필수 |
| 19 | large DB FTS rebuild가 OOM 후 BM25 empty를 만들었고, fix는 streaming/shadow/atomic publish를 사용 | Research Loop 19, Adversarial Loop 11 | https://github.com/tobi/qmd/issues/736, https://github.com/tobi/qmd/pull/737 | qgh Tantivy rebuild도 shadow generation과 atomic publish 없이는 active index를 비울 수 있음 |
| 20 | qmd ecosystem에 watch, remote sync, MCP mutation, Postgres, plugin, graph 등 scope creep pressure가 반복 | Loop 30, 36-37, 67, 100 | https://github.com/tobi/qmd/pull/632, https://github.com/tobi/qmd/pull/646, https://github.com/tobi/qmd/pull/725, https://github.com/tobi/qmd/pull/375 | qgh MVP scope는 Issues+comments read-only local search로 엄격히 제한 |

## 6. 중복/저신호 제거 목록

| 분석했지만 제외한 항목 | 제외 이유 |
|---|---|
| AST chunking, Java/Kotlin tree-sitter, code-aware chunking | Issues/comments MVP의 핵심 failure mode가 아니다. Wiki/code-heavy corpus에서 later 검토 |
| PDF/PNG/JSON/Bear Notes/source plugin 요구 | generic ingestion wishlist이며 qgh MVP source scope와 약함 |
| Knowledge graph layer | qmd wishlist 성격이 강하고 MVP retrieval correctness와 직접 관련 낮음 |
| CSV/XML/Markdown output variants | agent safety에는 strict JSON schema와 `get` contract가 훨씬 중요 |
| Local editor URI integration | qgh canonical locator는 GitHub issue URL/comment URL이다 |
| 개별 GPU backend 선택 세부(CUDA/Vulkan/Metal 우선순위) | `vector optional + doctor probes + no hard dependency` 원칙으로 흡수 가능 |
| Nix/Bun/Windows packaging의 모든 개별 사건 | 중요한 결론은 one supported runtime, platform smoke, packaged docs/schema 포함이다 |
| Recency boost/reaction count/comment count ranking | useful signal일 수 있으나 early correctness guardrail은 아니다. popularity bias risk가 있어 later eval 후 결정 |
| HTTP REST endpoint surface | MVP는 CLI/MCP thin consumers가 먼저다. HTTP는 server/org phase에서 schema equivalence test와 함께 검토 |
| OpenSearch/Typesense/Meilisearch 후보 | local MVP가 bundled SQLite + Tantivy로 검증되기 전에는 운영 복잡도만 늘린다 |
| Full timeline events | issue decision history에는 유용할 수 있으나 permissions/source model/rate가 커져 MVP를 흐림 |
| Fine-tuned expansion/ranking | qmd evidence는 overfitting, prompt mismatch, dependency/security surface를 보여준다. curated eval 이후 별도 판단 |

## Backlog/Test로 바로 옮길 MVP Gate

1. Schema gate: `source_entity`, `source_version`, `source_alias`, `chunk`, `sync_run`, `tombstone`, `index_generation`, `embedding_fingerprint` 설계와 invariant tests.
2. Sync gate: REST issue/comment fake server로 `state=all`, `pull_request` 제외, Link pagination, edit, delete, transfer, ETag/304, 60초 watermark overlap, rate-limit retry, reconciliation tests.
3. Retrieval gate: every query result round-trips through `get`, including comment-only answers.
4. CLI/MCP gate: strict schema, no unknown params, no implicit repo/profile, versioned JSON/error envelope snapshots, stdout cleanliness, CLI-only `doctor`, no MCP `doctor/eval`.
5. Search gate: Tantivy BM25 exact lookup, structured filters, semantic query, negative query, CJK/mixed token eval.
6. Vector gate: optional install, fingerprint rejection, partial coverage, no-vector fallback.
7. Store/index gate: SQLite WAL/busy timeout/single-writer, migration race, Tantivy shadow rebuild, crash recovery.
8. Privacy gate: default no network egress except GitHub.

## Final Scope Decision

qgh MVP는 진행한다. 여기서 진행 판단은 go/no-go의 Go였으며, 구현 언어는 Rust로 고정한다. 성공 조건은 "GitHub 문서 검색 플랫폼"이 아니라 "qmd의 retrieval-first agent workflow를 GitHub Issues와 issue comments에 맞게 local-first로 이식한 read-only tool"이다.

초기 구현자는 다음을 변경 불가 constraint로 취급해야 한다.

- explicit repo allowlist
- read-only source model
- Tantivy BM25-only usable path
- source identity independent from URL/title/issue number
- tombstone/reconciliation
- query result round-trip through `get`
- strict MCP/CLI/config validation
- no hosted provider by default

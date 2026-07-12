# qgh 하이브리드 서치 PRD — 로컬 vector 임베딩 기반 BM25+vector 검색

- 상태: 구현 완료 후 release-hardening 중 (v0.2, 2026-07-13)
- tracker SSOT: GitHub issue #47 (`https://github.com/juicyjusung/qgh/issues/47`)
- 문서 역할: #47의 detailed appendix. #47 body가 하이브리드 프로그램의 tracker source of truth이고, 이 파일은 상세 스펙·결정 로그·리서치 근거를 보관한다.
- 선행 문서: `qgh-prd.md` (MVP PRD), `qgh-product-brief.md`, `qgh-mvp-evidence-decision-summary.md`, `github-issues-wiki-hybrid-search-go-no-go.md`, `docs/adr/0003-bm25-only-mvp-vector-post-mvp.md`
- 이 문서는 MVP PRD §6.1이 "post-MVP로 명시적 연기"한 vector/hybrid capability를 정식 스코프로 승격하는 explicit scope change다. 채택 시 ADR-0003 개정 + vector 스토리지/런타임 신규 ADR 작성이 선행 조건이다.

## 1. Executive Summary

qgh MVP는 Tantivy BM25-only 경로(`sync → query → get → cite → status`)를 완성했고 release gate(AC-01~27, 24 curated query eval 전부 통과)를 넘었다. 하이브리드 챕터는 로컬 임베딩 기반 vector 검색을 **optional capability**로 추가하고 BM25 보호형 융합으로 키워드가 어긋나는 자연어/의역/교차언어 쿼리의 recall을 보완한다.

핵심 계약은 변하지 않는다: BM25-only는 계속 완전 동작하는 필수 경로이고, 임베딩은 config opt-in이며, 기본 모드에서 private repo 내용은 어떤 hosted 서비스로도 나가지 않고, 모든 검색 결과는 `get`으로 round-trip 되는 source 후보다.

확정된 기술 경로 (2026-07-02, 1차 소스 검증 완료):

| 축 | 결정 | 근거 요약 |
|---|---|---|
| 임베딩 런타임 | **fastembed-rs (ONNX Runtime/ort 백엔드, 정적 링크)** | strip 후 +16MB 실측, 런타임 시스템 의존성 0, CPU 성능 candle 대비 ~14배 근거, 동기 API |
| 새 config 기본 모델 | **`qwen3-embedding-0.6b`** (Qwen3-Embedding-0.6B, 384d, ctx 1024, last-token pooling) | ADR-0016 사용자 결정. BM25 top-5를 보존하면서 Recall@10 miss 3건을 rescue했고 Apple Silicon Metal F16 경로가 실용 범위였다. 정식 blind/resource promotion gate 미완료 위험은 별도 유지 |
| 모델 배포/교체 | **번들 미포함 + `qgh model install qwen3-embedding-0.6b` 명시 다운로드 + pinned manifest/fingerprint** | `init`/`sync`/`query`/MCP 자동 다운로드 금지. 기존 BM25/Arctic/custom config는 자동 변경하지 않음 |
| vector 저장 | **sqlite-vec v0.1.9 stable, 정적 링크(`sqlite3_auto_extension`), brute-force KNN** | 10k~100k chunk 규모에 충분, rusqlite bundled 호환, ANN alpha 라인은 채택 금지 |
| 융합 | **`lexical_guard_v1`**: BM25 top-5 고정 + 아래 후보에 weighted RRF(k=60, lexical 2, dense 1, dense window 80) | exact/identifier와 강한 lexical head를 지키면서 vector를 BM25 miss 보완에 사용. 사용자 임의 boost knob 없음 |
| 청킹/인용 | **청크 단위 검색(~900 token, 15% overlap) + source 단위 인용(dedupe)** | 품질은 청크에서, citation 계약 무변화. 예약된 `chunks` 테이블 사용 |

## 2. 문제 정의

BM25는 용어 일치 검색이다. 다음 쿼리 클래스에서 구조적으로 실패한다:

- **의역/자연어 질문**: "sync 도중 API 한도 걸리면 어떻게 되나" → 이슈 본문은 "secondary rate limit backoff"라고만 씀. 용어 교집합 없음.
- **교차언어**: 한국어 쿼리로 영어 이슈 검색(또는 역방향). qgh 자체 트래커부터 한국어 본문 + 영어 식별자 혼합.
- **증상↔원인 어긋남**: 사용자는 증상("검색 결과가 비어 나옴")으로 묻고, 이슈는 원인("FTS rebuild OOM")으로 기록됨.

반대로 vector 단독은 정확 식별자 매치(`ambiguous_locator`, `#42`, camelCase 심볼)에서 BM25보다 약하다. 두 신호는 상보적이며, 융합이 표준 해법이다. GitHub native semantic search(2026-04 GA)는 title/body만 인덱싱하고 분당 10회 제한·클라우드 처리라, comments+로컬+rate-limit-free+deterministic citation이라는 qgh wedge는 유지된다(product brief 참조).

## 3. 목표 / 비목표

### 목표

1. 임베딩 파이프라인을 opt-in optional capability로 추가 — 설치·모델·GPU 없이도 qgh 전 기능 동작.
2. BM25 보호형 하이브리드 검색으로 semantic/의역/교차언어 쿼리 클래스의 recall 개선을 **측정 가능하게** 달성.
3. 모델 선택은 strict config와 pinned manifest로 명시하고, 다운로드·재임베딩은 사용자에게 보이는 CLI 작업으로 유지.
4. 기존 BM25 release gate 전부 유지(회귀 0) + 하이브리드 전용 quality gate 신설.

### 비목표 (Non-Goals)

- **hosted embedding/rerank의 기본값화 또는 필수화.** hosted는 명시적 opt-in provider로만 허용하며(§8 FR-H2), 기본 모드는 로컬 전용. 이는 MVP PRD의 "hosted 금지"를 "기본값·필수화 금지(opt-in 허용)"로 정정하는 것이다.
- generic RAG 확장 — 답변 생성, LLM 쿼리 확장, HyDE, 요약. 결과는 계속 source 후보다.
- 스코프 밖 인덱싱 — PR/Discussions/Projects/코드/org-wide discovery. Wiki는 별도 커넥터 결정(MVP PRD §17.6) 유지.
- MCP write/embed tool — MCP v1은 `query`/`get`/`status` read-only 유지. 임베딩 작업은 CLI/sync에서만 트리거.
- vector 전용 신규 검색 제품화 — vector-only 모드는 내부/실험 플래그이지 사용자 대상 1급 모드가 아니다.
- ANN 인덱스(sqlite-vec alpha의 rescore/IVF/DiskANN) 채택 — pre-1.0 미문서화. brute-force + (필요 시) MRL 차원 축소로 버틴다.

## 4. 사용자 시나리오

1. **에이전트 semantic 조회 (MCP)**: 코딩 에이전트가 `query("sync가 중간에 끊겼을 때 이어받는 로직 논의된 이슈")` 호출. BM25는 "이어받는"에 걸리는 게 없지만 vector가 resume/checkpoint/cursor 논의 이슈를 후보로 올림. 에이전트는 `get_args`로 본문을 가져와 인용. 결과 스키마는 기존과 동일 + typed ranking field 추가.
2. **교차언어 검색 (CLI)**: 한국어로 `qgh query "임베딩 모델 교체하면 재색인 필요한가"` — 영어로 작성된 이슈 코멘트가 상위에 옴.
3. **정확 조회는 그대로 (회귀 없음)**: `qgh query "#42"`, 에러 코드 문자열 검색 — exact/BM25 경로가 기존과 동일하게 응답. 하이브리드가 hard filter(repo/label/state)를 절대 우회하지 않음.
4. **오프라인/에어갭 사용자**: 임베딩 미설정 → 지금과 완전 동일한 BM25-only 동작. 또는 수동 배치한 ONNX 파일(`model_path`)로 네트워크 없이 하이브리드 사용.
5. **모델 교체**: 사용자가 config를 `model = "hf:dragonkue/snowflake-arctic-embed-l-v2.0-ko"`로 변경 → 다음 명령이 fingerprint 불일치를 감지하고 structured error로 `qgh embed --force` 안내 → 재임베딩 후 하이브리드 재개.

## 5. 기능 요구사항 (FR-H)

| ID | 요구사항 | 왜 필요한가 | Acceptance |
|---|---|---|---|
| FR-H1 | `[embedding]` config 섹션(opt-in). 부재 시 임베딩 코드 경로 완전 비활성, 기존 4 명령 무변화 | BM25-only 필수 경로 보존 (ADR-0003 계승) | AC-H1 |
| FR-H2 | `EmbeddingProvider` trait. v1 구현은 `local`(fastembed-rs)만. config에 `provider` 필드 예약, `openai-compatible` HTTP provider(ollama/vLLM/OpenAI/Voyage 호환)는 후속 슬라이스 | 클라우드/외부 데몬 확장을 스키마 파괴 없이 수용. Qwen류 last-token 모델도 HTTP 경유로 해소 | AC-H2 |
| FR-H3 | 새 config는 `model = "qwen3-embedding-0.6b"`; 모델은 `qgh model install`에서만 pinned XDG snapshot으로 명시 설치한다. Qwen 이외의 기존 prepared manifest/`model_path` config는 계속 지원하지만 자동 획득·침묵 fallback은 금지한다. gated credential은 token source 참조만 허용한다 | 사용자 통제 다운로드 + strict config + 에어갭 경로 | AC-H3 |
| FR-H4 | Qwen preset은 고정 query instruction, last-token pooling, L2 normalization, 384d output, 1024-token fitted input을 사용한다. `device = "auto"`는 Apple Silicon Metal F16, 그 외 지원 환경은 CPU F32로 해석하고 runtime profile을 fingerprint에 포함한다 | 실제 native adapter 계약과 CPU/GPU 결과 재현 | AC-H4 |
| FR-H5 | embedding fingerprint = provider + model id + revision + dimension + pooling + prefix + chunker 버전 + source schema 버전. 저장된 fingerprint와 불일치하는 embedding은 검색에 사용 금지, `qgh embed --force`로 전량 재임베딩 | DSR-06 계승. 모델 교체·청커 변경 시 무결성 보장 | AC-H5 |
| FR-H6 | 청킹: 이슈 body/코멘트/(후속)위키를 ~900 token, 15% overlap, 200-token 탐색 윈도우, Markdown 경계(heading/code-fence/paragraph) 인식으로 분할. token 카운팅은 **선택된 임베딩 모델의 tokenizer** 기준. `chunks` 테이블에 chunk_id ↔ source_id/source_version_id 매핑 저장 | go/no-go 확정 파라미터. 문자 휴리스틱은 한국어에서 2배 오차 | AC-H6 |
| FR-H7 | sync 파이프라인 훅: Store-owned source chunk count/digest manifest와 generation fingerprint로 미변경 소스를 스킵하고 신규/변경분만 임베딩한다. manifest는 chunk mutation trigger로 무효화하며 legacy evidence 부재는 재청킹한다. 완료 batch는 중단 뒤 새 sync run에서도 깊은 검증 후 재사용한다 | 전량 재계산 방지, 무결성 fail-closed, rate limit 무관 | AC-H7 |
| FR-H8 | vector 저장: sqlite-vec vec0 virtual table은 optional `vector-search` capability에 둔다. `Store::open()`은 base migration만 수행하고, embedding-enabled command가 명시적으로 vector capability를 연 뒤 해당 connection에 sqlite-vec을 등록하고 additive/idempotent vector migration을 실행한다 — 기존 DB drop/재sync 불필요 | BM25 base store와 vector migration 분리 | AC-H8 |
| FR-H9 | 하이브리드 query: source dedupe된 BM25/vector 후보에서 BM25 top-5 순서를 고정하고 나머지를 weighted RRF(k=60, lexical 2, dense 1, dense window 80)로 융합한다. repo/label/state/author 등 hard filter는 양쪽 후보 생성 단계에서 pre-filter — 융합/rerank가 filter를 우회·완화 금지 | Filter immutability + exact/lexical hard gate + BM25 miss rescue | AC-H9 |
| FR-H10 | typed ranking: `ranking.kind = hybrid` 추가, `lexical_score`/`vector_distance`/`rrf_rank_score`/`final_order_score` 분리 노출. confidence/probability 명명 금지 | SQR-04/AC-22 계승 | AC-H10 |
| FR-H11 | coverage gating: embedding coverage 미완(백필 중/실패분 존재) 시 하이브리드는 자동 비활성 또는 명시 경고와 함께 BM25 fallback. `status`에 embedding coverage(임베딩 완료/누락 청크 수, fingerprint, 모델)를 로컬 정보만으로 표시 | AC-13/FR-10 계승, 부분 커버리지에서 조용한 품질 저하 방지 | AC-H11 |
| FR-H12 | graceful degradation: 런타임 init 실패/모델 파일 없음/fingerprint 불일치 → structured warning + BM25 결과 반환. 임베딩 실패가 `sync`/`query`/`get`/`status`를 중단시키지 않음 | qmd evidence #18/#19 교훈 | AC-H12 |
| FR-H13 | `[reranker]`는 별도 설치한 `qwen3-reranker-0.6b`를 지정하되 기본 config에는 쓰지 않는다. `query --rerank`/MCP `rerank: true`에서만 최대 top-10 후보를 재정렬하고, exact locator는 bypass하며 실패 시 원래 순서를 전부 보존한다. 후보 추가·filter 완화·사용자 depth/weight knob는 금지한다 | 고비용 cross-encoder를 선택적으로만 사용 | AC-H13 |
| FR-H14 | strong-lexical shortcut(latency 최적화, optional): BM25 top 정규화 점수·갭이 임계값 이상이면 vector 검색 생략 가능. 기본 off, config 노출 | qmd 검증 패턴, CPU 쿼리 비용 절감 | AC-H14 |

## 6. 비기능 요구사항

| ID | 요구사항 |
|---|---|
| NFR-H1 | 바이너리 크기 증가 ≤ +25MB (ort 정적 링크 실측 +16MB 기준) |
| NFR-H2 | 하이브리드 warm query p95 ≤ 1.5s @ 10k sources/50k chunks, CPU-only (BM25 단독 gate 500ms는 별도 유지). 초과 시 shortcut/MRL 축소 검토 |
| NFR-H3 | 임베딩 백필 처리량: 50k chunks 초회 백필이 일반 노트북 CPU에서 실용 시간 내 완료(목표치는 slice 1 실측 후 이 PRD를 개정해 고정) |
| NFR-H4 | Qwen vector 스토리지: 384d f32 기준 약 1.5KB/chunk(부가 mapping/checksum 제외). dimension별 vec0 generation ownership과 checksum을 검증하고 임의 MRL 절단은 하지 않음 |
| NFR-H5 | 빌드: `cargo build`만으로 완결(ort는 빌드 시 프리빌트 다운로드). 재현/오프라인 빌드는 `ORT_LIB_PATH` 경로 문서화 |

## 7. 프라이버시/보안 요구사항

| ID | 요구사항 |
|---|---|
| PSR-H1 | 기본 모드에서 repo 내용·파생 데이터(청크/임베딩)의 외부 전송 0. Qwen model egress는 사용자가 `qgh model install`을 명시 실행한 수신 다운로드뿐이며 `init`/`sync`/`query`/MCP는 다운로드하지 않음. `model_path` 사용 시 egress 0 |
| PSR-H2 | hosted provider(후속)는 명시적 opt-in + config 검증 시점에 "이슈/코멘트 본문이 해당 endpoint로 전송됨" 경고 노출 + token_source 참조만 허용 |
| PSR-H3 | 임베딩 벡터·청크는 민감 파생 데이터 — DB와 동일한 single-user 파일 권한, 로그에 청크 본문 미기록 |
| PSR-H4 | HF 토큰 포함 모든 자격증명은 기존 token_source 패턴(literal 저장 금지) |

## 8. 검색 품질 요구사항과 수용 기준

### 8.1 측정 방법

기존 eval harness(`tests/search_quality_eval.rs`, synthetic fixture, `sync → query → get` round-trip) 확장:

1. **기존 24 query = BM25 회귀 gate**: 하이브리드 활성 상태에서도 기존 numeric target 전부 유지 (exact top-1 ≥ 0.95, keyword top-5 ≥ 0.80, CJK top-5 ≥ 0.70, negative abstention ≥ 0.80, round-trip 1.00).
2. **신규 semantic query class 추가** (초기 15~20개, directional gate): 의역(paraphrase), 자연어 질문, 교차언어(ko 쿼리→en 문서, en→ko), 증상→원인. 각 query에 gold source 라벨.
3. **A/B 프로토콜**: 동일 fixture에서 BM25-only vs hybrid 비교 리포트를 eval 출력에 포함. 모델 A/B(arctic-l 기본 vs dragonkue-ko vs gte-modernbert-base)도 동일 프로토콜.

### 8.2 초기 numeric target (첫 eval 후 PRD/ADR 경유로만 재보정)

| Gate | Target |
|---|---|
| 기존 BM25 gate 전부 | 무회귀 (하이브리드 on/off 모두) |
| semantic/paraphrase top-5 recall | ≥ 0.70 (hybrid), BM25-only 대비 명시적 개선 폭 리포트 |
| 교차언어 top-5 recall | ≥ 0.60 (initial, directional) |
| hard filter 위반 | 0 (filter 밖 결과 노출 즉시 fail) |
| top-k `get` round-trip | = 1.00 (hard, vector 유래 결과 포함) |
| coverage gating | 부분 커버리지 상태에서 하이브리드 결과에 경고 누락 0 |

### 8.3 rerank / 융합 방식 재평가 트리거

- semantic gate가 rerank 없이 미달 → rerank 슬라이스(FR-H13 활성화) 착수.
- eval셋 축적 후 normalized weighted sum(convex combination)을 RRF와 A/B — 상회 시 PRD 개정으로 전환.
- 분기별 모델 재평가 후보: pplx-embed-v1-0.6b(2026-01, MIT, mean pooling, 공식 ONNX — kor 미검증이라 보류 중).

## 9. 슬라이스 / 마일스톤

| # | 슬라이스 | Deliverable | Exit criteria |
|---|---|---|---|
| H0 | 스코프 개정 | ADR-0003 개정 + vector 런타임/스토리지 신규 ADR, 이 PRD 확정·등록, #46 트리아지, #1/#2 mirror 정책 결정 | ADR/PRD 머지, 트래커 반영 |
| H1 | 임베딩 파이프라인 | `[embedding]` config + provider trait + local runtime + 명시 model install + 청킹/manifest attestation + fingerprint + resumable sync + `embed --force` + 저비용 `status` coverage | fixture에서 coverage 100% 도달, BM25 경로 무영향 회귀 green, 임베딩 미설정 시 기존 스냅샷 동일 |
| H2 | vector 검색 단독 | sqlite-vec 정적 링크 + vec0(동적 차원) + 내부 vector-only 쿼리 모드 + `vector_distance` field | vector-only smoke eval 동작, round-trip 1.00, Tantivy 세대 스왑↔vec 테이블 정합 설계 문서화 |
| H3 | 보호형 융합 | over-fetch + pre-filter 푸시다운 + `lexical_guard_v1` + source dedupe + `ranking.kind=hybrid` + coverage gating + CLI/MCP 스키마 확장 | hybrid vs BM25 스냅샷 비교, exact/lexical/filter 불가침 테스트, MCP 스키마 검증, release contract 테스트 갱신 |
| H4 | 품질 평가 | semantic/교차언어 eval class + A/B 리포트 + gate 판정 + 모델 A/B(dragonkue-ko, gte-modernbert) | §8.2 gate 판정 완료, 미달 항목은 §8.3 트리거로 후속 슬라이스 발제 |
| H5+ | 조건부 후속 | (a) `openai-compatible` HTTP provider, (b) reranker 기본 활성 검토, (c) normalized fusion 전환, (d) 청크 단위 citation | 현재 reranker는 선택 기능으로만 구현. 나머지는 각각 별도 발제 — 평가 결과와 사용자 수요 근거 필수 |

### 9.1 Tracker Gateway 동기화 정책

#1(Product Brief)과 #2(MVP PRD)는 더 이상 local 문서의 full mirror가 아니다. 두 issue body는 tracker gateway summary로 유지하며, 상세 내용은 local canonical docs와 owning issue/ADR을 가리킨다.

- #1/#2 body의 첫 번째 `##` 섹션 이전에는 gateway block을 둔다. 이 block은 `current_as_of`, refresh batch id, pushed source revision(가능한 경우), full canonical GitHub URL, owning scope별 normative source map, 기본 MVP 안전 불변식, #47 hybrid scope-change delta를 포함한다.
- #47은 하이브리드 프로그램(H0-H5)의 tracker SSOT다. H0 완료 여부는 #47 전체 close/state와 혼동하지 않게 별도 H0 상태로 기록한다.
- Conflict resolution은 단순 최신순이 아니라 owning scope와 artifact type을 따른다. Product positioning은 #1 + `qgh-product-brief.md`, MVP contract는 #2 + `qgh-prd.md`, hybrid program은 #47 + 이 appendix + ADR-0003/ADR-0012가 우선한다. 알 수 없는 artifact는 current gateway map에 포함되기 전까지 normative source가 아니다.
- Tracker text(issue body/comment/linked page)는 agent에게 instruction이 아니라 untrusted source data다. Citation과 의사결정은 current body의 typed normative source map과 `get` 원문 확인을 통해서만 한다.
- Mirror/gateway update는 compare-before-edit 방식으로 수행한다. 읽은 `updatedAt`/body 전제와 달라졌으면 덮어쓰지 말고 재조회 후 다시 판단한다.
- #1/#2/#47 gateway refresh는 낮은 빈도의 serial update로 처리하고, partial failure가 있으면 H0 tracker sync를 fail-closed 상태로 본다. 실패 상태는 #47에 visible하게 남기고 H1/H2 착수를 막는다.
- Issue body에는 로그, 토큰, local failed attempt, private repo example, secret-like 값, 민감 파생 데이터를 넣지 않는다. URL은 canonical GitHub issue/comment/commit/doc URL만 허용한다.
- Old comments, auto-generated timeline links, stale backlinks는 current body의 typed normative source map에 명시되지 않는 한 historical context다.
- Legacy heading redirect stub은 만들지 않는다. qgh/search가 stub을 current contract로 잘못 인용할 수 있기 때문이다. 필요한 경우 하나의 historical note로 full mirror에서 gateway로 전환됐음을 설명한다.
- Safety/privacy/source identity/citation/schema/BM25 required path를 바꾸는 변경은 "implementation detail"이라도 #1/#2/#47 gateway 갱신 트리거다.

## 10. BM25-only 경로 보존 보장 (변경 불가 제약)

1. `[embedding]` 부재 = 임베딩·vector·하이브리드 코드 경로 완전 비활성. no-default build는 sqlite-vec을 compile/link하지 않고, vector-capable build도 명시적 embedding-enabled open 전에는 sqlite-vec 등록이나 embedding/vector schema migration을 실행하지 않는다.
2. release contract 테스트를 "vector 미채용" 단정에서 **"임베딩 비활성 시 BM25-only 완전 동작 + 스키마 무변화"** 단정으로 교체하고 매 슬라이스 CI 강제.
3. 임베딩 관련 모든 실패는 warning + BM25 fallback (FR-H12). 필수 의존화 금지.
4. eval은 H4부터 BM25 baseline과 hybrid를 항상 병행 측정 — BM25 회귀 즉시 검출.

## 11. 리스크와 완화

| 리스크 | 완화 |
|---|---|
| sqlite-vec 유지보수 정체 (2026-05-18 이후 커밋 없음, 이슈 무응답) | stable v0.1.9 + brute-force만 사용. vector store 접근을 얇은 모듈로 격리해 usearch/hnsw_rs 교체 가능 유지 |
| sqlite-vec #297 크로스 플랫폼 DB 복사 corruption | qgh DB는 재생성 가능한 파생 데이터 — "DB 파일의 플랫폼 간 복사 비지원" 문서화 |
| sqlite-vec DELETE 후 공간 미회수 (#220/#54) | 전량 재빌드 시 vec 테이블 drop&recreate 경로 채택 검토 (Tantivy 세대 재빌드와 동형) |
| fastembed가 ort 2.0-rc exact-pin | 버전 잠금 + fastembed 릴리스 추적. 병목 시 ort 직접 사용 경로(2순위)로 전환 |
| arctic-l-v2.0 코드 혼합 텍스트 성능 미측정 (CoIR 미등재) | 하이브리드 구조상 식별자 정확 매치는 BM25 담당. H4 eval에 코드 식별자 쿼리 포함, 미달 시 gte-modernbert A/B |
| 한국어 벤치가 전부 self-report | H4 자체 eval이 최종 판정 |
| 초회 백필 시간(모델 다운로드 570MB + 50k chunk 임베딩) | 진행률 표시, `--if-stale`류 재개 가능 설계, NFR-H3 실측 후 목표 고정 |
| Tantivy 세대 스왑 vs in-place vec 테이블 정합 | H2에서 정합 설계 문서화를 exit criteria로 강제 (fingerprint+generation 매핑) |
| 저자원 언어 품질 (커뮤니티 보고: 우크라이나어 등 실패 사례) | 지원 언어 명시 문서화 + config 모델 교체 경로 안내 |

## 12. Historical Planning Decisions (2026-07-02)

이 표는 최초 계획 스냅샷이다. 현재 기본 모델·배포·융합·reranker 결정은
ADR-0016/0017과 이 문서의 §1/§5가 supersede한다. 특히 D-H2/D-H4/D-H5/D-H6은
현재 production 계약이 아니다.

| # | 결정 | 근거 |
|---|---|---|
| D-H1 | 런타임 = fastembed-rs, ollama/클라우드는 HTTP provider로 후속 | 실측(+16MB, CPU 최상), 단일 바이너리 철학. qmd의 llama.cpp 선택은 Node 생태 제약으로 비이식 |
| D-H2 | 기본 모델 = arctic-embed-l-v2.0, m-v2.0 아님 | 커뮤니티 검증량 6배, 표준 XLM-R 아키텍처(m은 trust_remote_code/attention quirk), kor 73.27, MRL 256 공식 학습 |
| D-H3 | bge-m3 기본값 탈락 | 영어 리더보드 미등재(검증 불가), CoIR 39.3(코드 혼합 붕괴), fp32 2.27GB 공식 양자화 없음 |
| D-H4 | Qwen3-Embedding류 v1 미지원 | last-token pooling이 fastembed Pooling(Cls/Mean) 밖. HTTP provider 슬라이스에서 ollama 경유로 자연 해소 |
| D-H5 | 융합 = RRF k=60 선행, normalized weighted sum은 eval셋 확보 후 A/B | Bruch et al.(TOIS 2023): 튜닝 데이터 없으면 RRF 강건, 있으면 convex combination 상한 |
| D-H6 | rerank v1 off | RRF 대비 이득 직접 실증 부재, CPU 비용, qmd CJK 크래시 전력. seam/config 완비로 착수 비용 최소화 |
| D-H7 | 청크 검색 + source 인용 | 품질은 청크에서, 계약 파괴는 인용에서 — 전자만 취함. 청크 인용은 조건부 후속 |
| D-H8 | 2024-12 모델을 2026-07 기본값으로 채택 | 12개월 신모델 전수 스윕 결과 제약 클래스 내 파레토 프런티어 유지 — kor sub-1B 1위 불변, 신모델 우위는 비호환 pooling 또는 벤치 오염에서만 발생 |

## 13. Current Technology Stack 추가분

- 로컬 runtime: pinned Rust/Candle Qwen adapter + legacy prepared-ONNX/fastembed compatibility + `sqlite-vec` 정적 링크
- 새 config 기본 모델: `Qwen/Qwen3-Embedding-0.6B` pinned snapshot, 384d, last-token pooling; 번들·자동 다운로드 없음
- 실행 profile: Apple Silicon `auto` → Metal F16, 그 외 지원 환경 → CPU F32. profile/adapter revision은 fingerprint에 포함
- production fusion: `lexical_guard_v1`; lexical profile은 `production_v1`
- 선택 reranker: `Qwen/Qwen3-Reranker-0.6B`, 별도 설치, per-query top-10, 기본 off
- Snowflake/Dragonkue/GTE 결과는 historical compatibility/evaluation control이며 새 config 기본값이 아님

## Sources

- 리서치 스냅샷 2026-07-02: MTEB leaderboard backend API 직접 추출(eng v2/Multilingual v2/kor v1/Code v1), HF API 파일 트리, docs.rs/fastembed 5.17.2, sqlite-vec releases/issues(#297/#220/#302/#308), tobi/qmd main(v2.6.3) 소스, Snowflake arctic-embed-2.0 blog/arXiv:2412.04506, Bruch et al. TOIS 2023(arXiv:2210.11934), CoIR(arXiv:2407.02883)
- 선행 결정: `docs/adr/0003`, `docs/adr/0006`, MVP PRD §6.1/§13/§17, evidence decision summary, go/no-go 리포트(2026-06-27, §0 override 2026-06-29)

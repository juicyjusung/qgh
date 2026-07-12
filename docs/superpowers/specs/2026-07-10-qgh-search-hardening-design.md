# qgh 검색 하드닝 및 경량 다국어 모델 평가 설계

- 상태: 자체 검토 완료, 사용자 서면 검토 대기
- 작성일: 2026-07-10
- owning tracker: GitHub issue #47 (`https://github.com/juicyjusung/qgh/issues/47`)
- 관련 문서: `qgh-hybrid-search-prd.md`, `docs/adr/0003-bm25-only-mvp-vector-post-mvp.md`, `docs/adr/0012-optional-local-vector-runtime-and-storage.md`

## 1. 목적

qgh의 최종 제품 목표는 AI coding agent가 GitHub Issue와 issue comment에서 필요한 근거를 빠르고 정확하게 찾고, `query -> get -> cite`로 원문을 검증할 수 있게 하는 것이다. 현재 구현된 BM25+vector 하이브리드 경로를 배포 가능한 수준으로 하드닝하고, 영어 우선·한국어 차순위의 실제 검색 품질과 로컬 CPU 비용을 함께 만족하는 가장 작은 모델을 선택한다.

현재 구현 범위는 GitHub Issues와 issue comments다. Wiki는 후속 connector이며, 추가되더라도 같은 source identity, source version, local-first, read-only, `get` round-trip 계약을 사용한다.

## 2. 성공 정의

다음 조건을 모두 만족하면 이 프로그램을 완료한 것으로 본다.

1. `[embedding]`이 없을 때 `sync`, `query`, `get`, `status`의 BM25-only 경로가 vector runtime, sqlite-vec, 모델 파일, 네트워크에 의존하지 않는다.
2. 기본 제공 모델 preset은 immutable revision, artifact checksum, tokenizer, query/document prompt, pooling, normalization, native/output dimension, max length를 명시하며 추측으로 실행하지 않는다.
3. `query`는 준비된 로컬 snapshot만 열고 Hugging Face나 다른 외부 호스트를 호출하지 않는다. 모델 취득과 추론은 분리된다.
4. vector가 맞춘 chunk를 source dedupe 후에도 내부적으로 보존하고, 기존 `snippet` 필드의 preview를 실제 매치 구간에서 만든다. 공개 result field 집합, citation, `get_args`는 source-level로 유지한다.
5. 모델·chunker·context template 변경은 staging generation을 완성·검증한 뒤 원자적으로 활성화한다. 실패 중인 generation은 검색에 노출하지 않는다.
6. 실제 모델과 실제 tokenizer를 사용하는 재현 가능한 영어·한국어·교차언어 qrels 평가가 존재한다. 합성 vector는 fusion 계약 테스트에만 사용한다.
7. hard quality/latency/privacy gate를 통과한 모델 중 가장 작은 모델을 기본 경량 preset으로, 최고 품질 통과 모델을 quality preset으로 기록한다.
8. reranker, late chunking, Korean 형태소 tokenizer는 사전에 정의한 실패 trigger가 발생한 경우에만 구현하거나 채택한다.

## 3. 변경 불가 제약

- SQLite는 source identity, source version, alias, tombstone, sync truth의 authoritative store다.
- Tantivy와 vector index는 삭제 후 재생성 가능한 파생 데이터다.
- 모든 성공 결과는 stable `source_id`, canonical URL, staleness metadata, `get_args`를 가지며 `get`으로 round-trip 해야 한다.
- 결과는 답변이 아니라 source candidate다. snippet과 matched chunk는 citation evidence가 아니다.
- hard filter는 각 retriever의 candidate 생성 전에 적용한다. fusion과 rerank가 filter를 완화하지 않는다.
- 기본 모드에서 private source content, chunk, query, embedding을 GitHub 이외의 외부 서비스로 보내지 않는다.
- MCP v1 tool은 read-only `query`, `get`, `status`만 유지한다.
- strict config와 strict JSON/MCP schema를 유지한다. unknown key나 호환되지 않는 모델 동작은 structured error다.
- 임베딩 실패는 required BM25 path를 중단시키지 않는다.
- 사용자 소유의 현재 `qgh-hybrid-search-prd.md` 변경을 덮어쓰거나 정리하지 않는다.

### 3.1 BM25 독립성의 정확한 의미

BM25 독립성은 다음 세 단계 모두를 뜻한다.

| 실행 형태 | 필수 동작 |
|---|---|
| `--no-default-features` BM25 build | sqlite-vec과 fastembed/ORT를 compile/link하지 않고 BM25 release-contract가 통과한다. |
| vector capability가 포함된 release build + `[embedding]` 없음 | sqlite-vec을 등록하지 않고 vector table migration/model acquisition/runtime init을 실행하지 않는다. 기존 BM25 JSON/MCP field 집합과 hard gate가 동일하다. |
| vector capability가 포함된 build + `[embedding]` 있음 + vector init 실패 | base SQLite/Tantivy를 열어 `query`는 structured warning과 BM25 결과를 반환한다. 명시적 `embed --force`만 non-zero structured error가 된다. |

이를 위해 sqlite-vec을 optional Cargo feature로 이동하고, store open을 base store와 optional vector capability 초기화로 분리한다. ADR-0012와 PRD FR-H8의 "모든 `Store::open()`에서 무조건 등록" 문구는 구현 전에 "vector-enabled open에서 base migration 뒤, vector migration 전에 등록"으로 개정한다.

## 4. 검토한 접근과 선택

### 접근 A: 현재 구현에 모델만 교체

가장 빠르지만 기각한다. 현재 provider는 사실상 `CLS|mean`, query-only `query: `, document no-prefix만 표현한다. Granite, EmbeddingGemma, Qwen, Jina처럼 다른 prompt/pooling 계약을 가진 모델을 잘못 실행하게 된다. 현재 default Arctic-L의 external ONNX data와 custom revision 처리에도 correctness 결함이 있다.

### 접근 B: 검색 엔진이나 vector DB 교체

Elasticsearch, Meilisearch, Qdrant 등은 좋은 참고 구현이지만 qgh에는 shared server, 운영 복잡성, 배포 크기, privacy 표면을 추가한다. 10k sources/50k chunks 목표에서 Tantivy+SQLite+sqlite-vec brute-force를 교체할 성능 근거도 없다. 기각한다.

### 접근 C: 기존 엔진을 유지하고 model/retrieval/index generation 경계를 하드닝

채택한다. BM25 required path를 보존하면서 model manifest, matched chunk, deterministic context, atomic activation, real eval을 작은 vertical slice로 추가할 수 있다. 유명 검색 프로젝트의 개념만 가져오고 운영 모델은 가져오지 않는다.

## 5. 목표 구조

```text
ModelAcquirer (network-capable; `qgh embed` and embedding-enabled `qgh sync` only)
  -> PreparedModelSnapshot (immutable local files + manifest)
       -> EmbeddingRuntimeCache (local-only, profile/fingerprint keyed)

RetrievalEngine
  -> RetrievalReadSnapshot (SQLite source snapshot + immutable Tantivy generation)
  -> ExactLocatorRetriever
  -> LexicalRetriever (Tantivy BM25)
  -> VectorRetriever (active embedding generation)
  -> Fusion (RRF baseline)
  -> SourceCollapse (matched chunks retained)
  -> GetEligibilityFilter
  -> RetrievalOutcome
```

`RetrievalOutcome`은 requested/used mode, fallback cause, candidate counts, stage timings, typed ranking, matched retrieval evidence를 내부적으로 가진다. 기본 CLI/MCP 출력과 field 집합은 유지한다. 상세 trace는 민감 본문이나 raw query를 기록하지 않는 test/eval harness에만 노출한다. 이 프로그램에서는 공개 `query --explain` flag나 MCP schema를 추가하지 않는다.

## 6. 구현 wave

### Wave 1: 기반 계약, correctness와 BM25 독립성

먼저 기존 경로의 release blocker를 고친다.

- 아래 Wave 2의 `ModelManifestV1` 중 첫 네 preset에 필요한 최소 field와 `PreparedModelSnapshot` interface를 먼저 구현하고 schema를 고정한다. 이후 Wave 1 작업은 임시 resolver나 임시 fingerprint를 만들지 않고 이 interface만 사용한다.
- external-data ONNX를 manifest artifact로 취급하고 runtime에 실제로 연결한다. graph만 열어 성공으로 간주하지 않는다.
- built-in preset은 commit SHA를 고정한다. custom HF model은 명시 revision을 받거나 acquisition 시 default branch를 commit SHA로 resolve해 local manifest에 기록한다. 다른 repo에 Snowflake revision을 재사용하지 않는다.
- freshly prepared runtime fingerprint와 저장된 active generation fingerprint를 query encoding 전에 정확 비교한다. mutable revision wildcard는 허용하지 않는다.
- local `model_path` identity는 경로 문자열이 아니라 파일 checksum을 포함한다.
- chunker version의 source of truth를 하나로 만들고 chunk rows에 version/config와 offsets를 기록한다. version이 바뀌면 재청킹한다. byte offset은 contextual prefix나 tokenizer-normalized text가 아닌 authoritative source의 원본 UTF-8 body를 기준으로 한다. token offset은 contextual prefix를 제외한 source text의 선택 모델 tokenizer token stream을 기준으로 한다.
- sqlite-vec 등록과 vector migration은 embedding-enabled open path 뒤에 둔다. BM25-only open은 vector extension failure로 깨지지 않는다.
- vec table dimension, row coverage, active generation 일치를 `status`와 query 전에 검증한다. corrupt/missing state를 조용한 empty vector result로 처리하지 않는다.
- qgh.v1 warning closed schema를 유지하고 현재 schema 밖으로 새는 `details`/`hint`를 제거한다. 원인은 stable warning `code`, `severity`, 안전한 `message`로 구분한다.
- 실제 prepared model을 여는 smoke, cached-offline query, external-data, immutable revision, corrupt vector state 테스트를 추가한다.

### Wave 2: model manifest와 local-only runtime

모델 특수사항을 qgh core에서 격리한다.

```text
ModelManifestV1
  schema_version
  preset_id
  provider
  model_source: hf { model_id, resolved_revision } | local { declared_id }
  artifacts[]: role, relative_path, sha256, byte_size
  tokenizer
  query_prefix
  document_prefix
  pooling: cls | mean
  normalize
  native_dimension
  output_dimension
  max_length
  quantization
  context_template_version
```

- 빈 prefix와 query/document 양쪽 prefix를 구분한다. `null`, `""`, 값 있음은 의미가 다르다.
- 첫 평가 preset에 필요한 CLS/mean pooling만 구현한다. `model_output`, output-key selection, last-token pooling은 초기 schema에 예약하지 않는다. 해당 동작이 필요한 challenger가 실제 trigger될 때 manifest/runtime schema를 함께 개정한다.
- MRL dimension은 truncation 후 재정규화하며 fingerprint에 포함한다.
- `quantization`은 `none | static | dynamic`을 명시한다. bounded batch 사이 embedding comparability를 보장하지 않는 dynamic artifact는 v1 index generation에서 거부한다. `none`/`static` artifact도 동일 text를 단독·다른 위치·다른 batch size로 인코딩한 cosine similarity가 0.99999 이상인지 preset smoke로 검증한다.
- built-in preset registry와 explicit custom manifest를 제공한다. 임의 HF repo 구조를 silent inference하지 않는다. custom manifest artifact는 manifest directory 아래의 canonical regular file만 허용하고 absolute path, `..`, symlink escape를 거부하며 모든 파일의 SHA-256과 byte size를 검증한다.
- legacy config는 호환 가능한 경우 명시적 manifest로 변환하고 deprecation warning을 낸다. 모호하면 fail closed한다.
- acquisition은 모델 파일 수신만 수행하며 source content를 보내지 않는다. `qgh embed`와 embedding-enabled `qgh sync`만 acquisition을 호출할 수 있다. `query`, `get`, `status`, `doctor`의 기본 검사와 MCP server는 network-capable model client를 소유하지 않는다.
- CLI 프로세스 안에서는 runtime을 재사용하고, MCP는 profile+fingerprint별 runtime을 유지한다.
- v1 runtime adapter는 fastembed `UserDefinedEmbeddingModel` 하나로 제한한다. `PreparedModelSnapshot`은 graph, tokenizer/config, ONNX graph가 선언한 external initializer를 모두 제공하며 fastembed smoke가 통과해야 `ready`가 된다. 네 preset 중 이 경로로 정확히 실행할 수 없는 후보는 direct ORT 같은 두 번째 adapter를 즉석 추가하지 않고 이번 평가에서 제외한다.
- external initializer의 실제 graph name, relative path, checksum, byte size를 manifest에 기록한다. graph와 companion file은 같은 prepared snapshot root 아래에 있어야 하며 일부 파일만 존재하는 snapshot은 실행하지 않는다.
- MRL은 model output을 앞에서 `output_dimension`만큼 자른 뒤 L2 재정규화한다. model-specific output key나 last-token pooling이 필요한 모델은 v1 preset에 넣지 않는다.

첫 평가 preset은 다음으로 제한한다.

- `arctic-m-v2-fp32`: 현 provider 동작과 맞는 drop-in control
- `granite-97m-multilingual-r2-fp32`: 경량 최우선 challenger. vendor AVX2 INT8 artifact는 실제 graph가 dynamic quantization이므로 preset에서 제외하고 fp32 artifact만 평가한다.
- `granite-311m-multilingual-r2-fp32`: multilingual quality ceiling. 같은 이유로 fp32 artifact만 허용하며 97M이 충분하지 않을 때만 평가한다.
- `arctic-l-v2-fp32`: 기존 quality control

Jina v5는 non-commercial license 때문에 built-in preset에서 제외한다. EmbeddingGemma, BGE-M3, Harrier, Qwen3는 이 네 후보로 결론이 나지 않을 때만 후속 challenger다.

### Wave 3: matched chunk와 match-aware preview

vector source collapse에서 best chunk를 버리지 않는다.

```text
MatchedChunkV1
  generation_id
  source_id
  source_version_hash
  chunk_index
  token_start
  token_end
  byte_start
  byte_end
  heading_path
  retriever_kind
  rank
  score_or_distance
```

- chunk identity는 `source_id + source_version_hash + chunker fingerprint + chunk_index`에 묶는다.
- source dedupe 뒤 best vector chunk와 best lexical match를 내부 trace에 각각 보존한다.
- `snippet`은 원본 UTF-8 body에서 실제 matched span 주변의 plain text로 만든다. match가 없거나 span이 무효하면 기존 head preview로 안전하게 fallback한다.
- `MatchedChunkV1`은 internal test/eval trace다. qgh.v1 result schema에 `matched_chunk` field를 추가하지 않는다. `source_id`, canonical URL, `get_args`, citation semantics와 공개 field 집합은 바뀌지 않는다.
- stale source version의 matched chunk는 반환하지 않는다.
- NFC/NFD Hangul, CRLF, tokenizer whitespace normalization, multi-byte Unicode, Markdown heading/code fence fixture에서 `snippet`이 원본 body의 유효한 UTF-8 slice임을 검증한다. tokenizer offset을 원본 byte span으로 역매핑할 수 없으면 해당 chunk materialization을 structured failure로 처리하고 근사 offset을 저장하지 않는다.

### Wave 4: resumable staging generation과 atomic activation

모델·prompt·dimension·chunker·context template 변경은 모두 embedding 의미 변경이다.

활성화 CAS identity는 embedding만이 아니라 전체 retrieval publication을 묶는 다음 tuple이다.

```text
(publication_id,
 source_snapshot_sync_run_id,
 tantivy_generation_id,
 embedding: null
   | { generation_id,
       model_manifest_hash,
       chunker_fingerprint,
       context_template_version,
       output_dimension })
```

`[embedding]`이 없는 BM25-only publication은 `embedding=null`이며 vector table, model manifest, chunker/context fingerprint를 읽거나 검증하지 않는다. optional group을 일부만 채운 상태는 invalid publication이다.

1. authoritative source snapshot과 성공한 `sync_run_id`를 고정한다.
2. 새 generation을 `building` 상태로 만들고 bounded batch로 chunk/vector를 기록한다.
3. 각 batch를 checkpoint해 중단 후 재개한다.
4. 전체 active source coverage, dimension, checksum, source version, context hash, smoke query를 검증한다.
5. 구축 중 source snapshot이 바뀌었으면 successor snapshot까지 delta를 적용해 다시 전체 검증하거나 activation을 취소한다. 검증 대상을 임의로 줄이지 않는다.
6. SQLite transaction/CAS로 위 tuple과 active retrieval publication pointer를 한 번에 전환한다. 시작 시 pointer와 tuple을 재검증하고 불일치하면 vector channel 전체를 끈다.
7. 실패하면 이전 active generation을 유지한다.
8. 단, active generation의 모든 vector-eligible source가 현재 authoritative `source_version`과 `context_hash`에 일치할 때만 그 generation을 query에 사용한다. 하나라도 어긋나면 stale vector를 일부 섞지 않고 hybrid 전체를 BM25로 fallback한다.
9. query는 SQLite read transaction에서 publication tuple을 고정하고, 그 tuple이 가리키는 immutable Tantivy generation과 vector generation만 사용한다. vector eligibility 검증, BM25/vector retrieval, source resolution, snippet/output materialization을 같은 source read snapshot에 묶는다. Tantivy open이나 tuple 검증이 실패하면 source-safe BM25 fallback도 불가능한지 판정해 structured error를 내고, 서로 다른 source snapshot의 ranking과 output을 섞지 않는다.
10. 보존 상한은 active 1개와 직전 ready 1개다. 직전 ready generation은 최대 7일 보존하고 새 activation 시 더 오래된 generation을 즉시 정리한다. 24시간 넘게 멈춘 `building` generation은 `status`/`doctor`에서 report만 하고, 다음 성공한 `sync` 또는 `embed --force`가 안전하게 제거한다.

source lifecycle별 계약은 다음과 같다.

| 사건 | query 가능 generation | 처리 |
|---|---|---|
| ordinary edit | 새 source snapshot과 완전히 일치하는 generation만 | successor가 ready 전이면 vector 전체를 끄고 BM25로 fallback한다. 이전 vector는 rollback 용도로만 남고 query에는 쓰지 않는다. |
| confirmed delete/tombstone | 없음 | 모든 generation에서 body, chunk, vector, snippet을 즉시 purge하고 최소 identity/tombstone만 남긴다. |
| repo가 configured profile allowlist에서 명시 제거되거나 permission loss가 확인됨 | 없음 | 다음 config reconciliation/sync에서 해당 repo의 민감 파생 데이터를 모든 generation과 cache에서 purge한다. cwd, repo policy, command별 Effective Scope가 일시적으로 좁아진 것은 purge trigger가 아니다. permission loss는 인증된 sync가 retry 정책 뒤 명시적 403/404를 확인한 경우만 해당하며 rate limit, timeout, 5xx를 permission loss로 추정하지 않는다. |
| build 중 sync drift | 기존 tuple이 현재 snapshot과 일치할 때만 기존 active | delta 재검증 또는 새 build 취소. partial generation은 절대 활성화하지 않는다. |
| activation transaction 중 crash | transaction 전 또는 후의 완전한 pointer 하나 | 시작 시 tuple 검증에 실패하면 BM25 fallback하고 `doctor` 복구 지침을 낸다. |

purge는 먼저 해당 source/repo를 `purge_pending`으로 표시해 `query`와 `get`에서 즉시 fail closed한 뒤 물리 삭제를 시도한다. qgh가 관리하는 SQLite source body/version, chunk/snippet, legacy JSON vector, generation BLOB, 모든 vec0 row, Tantivy/vector generation과 content-bearing cache/log가 대상이다. SQLite는 `secure_delete=ON`과 WAL checkpoint/truncate를 적용한다. source를 포함한 immutable derived generation은 통째로 제거하고 남은 source로 successor를 재구축한다. 모델 artifact cache는 source content가 아니므로 대상이 아니다. qgh가 만들지 않은 사용자의 filesystem snapshot/backup은 자동 삭제할 수 없음을 진단에 명시한다. 일부 삭제가 실패하면 `purge_pending`을 유지하고 profile retrieval을 비활성화하며, 다음 성공한 `sync`가 재시도하고 `doctor`가 실패 항목을 본문 없이 보고한다.

vector payload의 authoritative staging representation은 새 generation table의 compact little-endian f32 BLOB다. migration은 additive·idempotent하게 새 table/column만 추가하고 기존 JSON schema는 이 프로그램에서 drop하지 않는다. legacy row는 bounded batch로 checksum과 dimension을 검증해 옮기며 실패하면 active pointer를 바꾸지 않는다. 첫 성공 activation 뒤에도 직전 legacy data를 7일 rollback 기간 동안 유지하고, 이후 row data만 정리할 수 있다. 이전 binary로의 downgrade 호환은 보장하지 않으므로 migration 전 DB backup과 복구 절차를 release note에 기록한다. active sqlite-vec table은 선택된 generation에서 재구축 가능한 파생 index다.

### Wave 5: deterministic metadata context와 lexical field boost

hosted LLM 없이 원본 metadata로 chunk 의미를 보강한다. production context re-embedding은 Wave 4 generation 경로만 사용하며 in-place partial update를 허용하지 않는다.

Issue vector input:

```text
Repository: {host}/{owner}/{repo}
Issue #{number}: {title}

{chunk}
```

Comment vector input:

```text
Repository: {host}/{owner}/{repo}
Comment on issue #{number}: {parent_issue_title}

{chunk}
```

- 초기 template에는 labels, author, state를 넣지 않는다.
- prefix는 vector input에만 사용하고 stored source/body/snippet에는 섞지 않는다.
- `context_template_version`과 per-source `context_hash`를 저장한다.
- parent title 변경은 해당 issue comments의 context hash를 무효화해 재임베딩한다.
- Wiki connector가 후속 추가되면 page title과 heading path를 같은 방식으로 사용한다.

BM25는 metadata text를 body에 중복하지 않고 field boost를 사용한다. body=1.0을 기준으로 title `{1.5, 2.0, 3.0}`, parent title `{1.25, 1.5, 2.0}`, CJK ngram `{0.1, 0.25, 0.5, 1.0}` grid를 qrels에서 평가한다. labels/repo는 1.0 이하로 제한하며 issue number와 stable locator는 기존 exact route를 유지한다.

field boost는 required BM25 ranking을 의도적으로 바꾸는 별도 lexical-profile scope다. 먼저 eval 전용 profile로만 A/B한다. dev와 held-out test에서 baseline을 이기고 exact, filter, round-trip, stale leakage, class별 BM25 hard gate를 모두 통과한 경우에만 ADR-0003과 #47을 명시적으로 개정하고 versioned lexical ranking profile로 배포한다. 그 전에는 기존 BM25 profile을 유지한다. 선택한 값은 임의 사용자 knob로 노출하지 않는다.

### Wave 6: 실제 qgh retrieval eval과 preset 선택

합성 vector fixture는 filter/fusion/schema/round-trip 계약 테스트로 유지한다. 모델 선택은 실제 tokenizer와 runtime으로 별도 release eval을 수행한다.

Corpus와 qrels는 public qgh issue/comment를 재현 가능한 snapshot으로 `tests/fixtures/live-model-eval/`에 정리하고 private content를 포함하지 않는다. gold record는 query, relevant source identity, relevance grade, class, 근거 설명, 판정자를 가진다. 모호한 query는 두 번째 판정으로 합의하거나 제외한다. source identity 기준으로 tuning dev 40개와 한 번만 여는 held-out test 최소 80개를 분리한다. 같은 issue와 그 comments는 서로 다른 split에 넣지 않는다. machine-readable run artifact는 `target/qgh-eval/`에 만들고, 채택 근거가 되는 요약과 provenance만 `docs/search-quality-live-model-eval.md`에 기록한다. query class는 다음을 별도 집계한다.

- exact issue number, source locator, error code, symbol
- English semantic/paraphrase
- Korean semantic/paraphrase
- Korean query -> English source
- English query -> Korean source
- symptom -> cause
- comment-only answer
- long issue/comment and context-dependent chunk
- hard filters
- negative/no-relevant-source control
- edit/delete/tombstone/stale source

필수 metrics:

- Recall@5, Recall@20, MRR@10, nDCG@10
- exact top-1 and evidence source hit@k
- hard filter violation = 0
- `get` round-trip = 1.0
- stale/deleted leakage = 0
- duplicate-source crowding
- query p50/p95, cold start, peak RSS
- embedding throughput, resumability, model bytes, DB bytes/chunk

각 run은 corpus/qrels version, git SHA, model manifest hash, schema/chunker/context fingerprint, RRF k, candidate window, field boost profile을 기록한다. 기본 로그에는 raw query나 body를 남기지 않는다.

held-out test의 최소 class 구성은 English semantic 20, Korean semantic 15, Korean query -> English source 10, English query -> Korean source 10, exact/identifier 10, comment-only 5, long/context-dependent 5, negative/no-relevant-source control 5다. hard filter, edit/delete/tombstone, round-trip은 별도 contract fixture로 모두 실행한다. boost, RRF, candidate window, chunk parameter는 dev에서만 고르고 test 결과를 본 뒤 다시 조정하지 않는다. qgh에 confidence/answer abstention을 추가하지 않으며 negative control은 irrelevant top-result rate만 report한다.

품질은 query class별 macro metric으로 집계한다. 기본 선택 score는 English semantic nDCG@10 0.50, Korean semantic 0.20, Korean->English 0.15, English->Korean 0.10, comment-only/long-context 평균 0.05의 가중합이다. exact/identifier는 가중 score가 아니라 hard gate다.

모델 선택 규칙:

1. privacy, round-trip=1.0, filter violation=0, stale/deleted leakage=0, exact top-1>=0.95, 기존 BM25 test hard gate를 먼저 통과한다.
2. held-out Recall@5가 English semantic>=0.75, Korean semantic>=0.65, 각 교차언어 class>=0.60이고 reference host warm p95<=1.5s인 모델만 통과한다.
3. 통과 모델 중 최고 가중 nDCG@10보다 절대 0.02 이내인 가장 작은 전체 snapshot을 경량 기본 preset으로 선택한다. 지정한 Granite 311 후보가 탈락해도 같은 규칙을 적용한다.
4. 통과 모델 중 가중 nDCG@10이 가장 높은 모델을 quality preset으로 선택한다. 차이가 0.005 이하면 가중 MRR@10, 다시 동률이면 전체 snapshot byte가 작은 모델을 선택한다.
5. 어떤 모델도 통과하지 못하면 새 preset 승격과 vector-default release를 막고 실패 원인별 conditional follow-up을 실행한다. required BM25-only release는 별도 gate가 green이면 계속 배포할 수 있다.

전체 snapshot byte는 graph, external data, tokenizer, vocab, config, manifest 등 실행에 필요한 모든 파일의 uncompressed disk byte 합이다. 다운로드 전송 byte도 별도로 기록한다.

release reference host는 `Mac16,8 / Apple M4 Pro 14-core / 48 GB / macOS 26.5.1`이며 전원 연결·Low Power Mode off로 고정한다. compiler, ORT/fastembed version도 run manifest에 기록하고 fastembed가 노출하는 intra-op threads=4, batch=8로 실행한다. fastembed가 직접 노출하지 않는 inter-op/execution mode는 해당 version의 effective default를 기록하고 run 중 바꾸지 않는다. 전체 query set 1회 warmup 뒤 최소 3회 측정하고 held-out query 80개 이상을 매 회 실행한다. cold start는 새 process 5회다. reference host나 OS가 바뀌면 이전 baseline과 새 baseline을 함께 기록하고 gate 변경은 별도 승인한다.

light/default preset은 warm p95<=1.5s, cold-start p95<=5s, peak RSS<=1 GiB, 전체 snapshot<=500 MiB, 900-token 기준 50k chunk backfill>=10 chunks/s, vector DB 증가<=3 KiB/chunk를 모두 통과해야 한다. quality preset은 warm p95<=1.5s, cold-start p95<=10s, peak RSS<=2.5 GiB, backfill>=3 chunks/s를 통과해야 한다. 실패 항목은 report-only로 낮추지 않고 해당 preset 승격을 막는다.

RRF는 baseline으로 유지하되 real qrels에서 `k={20,60,100}`과 candidate window를 함께 평가한다. weighted RRF나 normalized fusion은 equal RRF를 명확히 이긴 경우에만 ADR/PRD 변경으로 채택한다.

2026-07-13 결정: 재현 가능한 80-query multilingual regression에서 equal
RRF는 aggregate를 높였지만 BM25 hit를 @5에서 5건, @10에서 4건
손상했고 comment-only Recall@5를 1.0에서 0.0으로 낮췄다. 따라서
ADR-0017의 고정 `lexical_guard_v1`을 채택한다. BM25 top 5를 그대로
보호하고 그 아래에서만 `k=60`, lexical weight 2, dense weight 1,
dense window 80을 적용한다. 사용자 boost knob는 추가하지 않으며 fresh
blind qualification 위험은 계속 기록한다.

### Wave 7: conditional follow-up

- recall은 통과하지만 top-rank precision/MRR이 미달하면 top-20 local cross-encoder reranker를 prototype한다.
- 긴 source의 대명사·문맥 누락이 주요 실패 원인이면 overlap=0과 작은 chunk를 먼저 A/B하고, 이후에만 late chunking을 prototype한다.
- Korean lexical class가 목표 미달이면 NFC-normalized derived field와 현재 2~3글자 ngram을 먼저 튜닝하고, 이후에만 Lindera ko-dic을 optional tier로 비교한다.
- BM25+dense가 특정 lexical expansion 실패를 반복할 때만 BGE-M3 sparse/SPLADE 같은 세 번째 retriever를 검토한다.
- 50k chunk brute-force가 실제 latency gate를 넘을 때만 ANN/vector store 교체를 별도 ADR로 검토한다.

Trigger가 발생하지 않으면 해당 기능을 구현하지 않는 것이 완료다.

## 7. 오류 처리와 진단

- model acquisition error와 local runtime error를 다른 structured code로 구분한다.
- 내부/eval trace는 `requested_mode`, `used_mode`, fallback reason을 machine-readable하게 제공한다. 기본 query와 MCP schema에는 이 필드를 추가하지 않는다.
- `status`는 model load나 network probe 없이 prepared snapshot, active generation, coverage, mismatch, corruption을 검사한다.
- `doctor`는 명시 실행 시 artifact checksum, runtime smoke, vector dimension/coverage를 검사한다. private body를 출력하지 않는다.
- success warning은 qgh.v1의 closed `{code, severity, message}` object만 사용한다. 추가 진단은 명시적 `doctor` data나 실패한 command의 기존 error `details`/`hint`에 둔다.
- partial generation과 corrupt vec state는 hybrid-ready로 보고하지 않는다.

명령별 결과는 다음으로 고정한다.

| 상태 | `query`/MCP `query` | `get`/MCP `get` | `sync` | `embed --force` | `status` | `doctor` |
|---|---|---|---|---|---|---|
| invalid/unknown config, incompatible explicit manifest | non-zero structured error, fallback 없음 | profile resolution이 불가능하므로 non-zero structured error | non-zero structured error | non-zero structured error | non-zero structured error | failed check/non-zero |
| prepared snapshot 없음 또는 acquisition 실패 | warning + BM25; query는 network를 호출하지 않음 | embedding 초기화 없이 SQLite 원문 반환 | source sync는 완료 가능, embedding warning | non-zero acquisition error | success + `degraded` | failed check/non-zero |
| runtime init/checksum/dimension 실패 | warning + BM25 | embedding 초기화 없이 SQLite 원문 반환 | source sync는 완료 가능, generation은 미활성 | non-zero runtime error | success + `degraded`/`corrupt` | failed check/non-zero |
| fingerprint mismatch 또는 partial/stale generation | warning + BM25 | embedding 초기화 없이 SQLite 원문 반환 | successor build 또는 warning | rebuild 시도, 실패 시 non-zero | success + `mismatch`/`partial` | failed check/non-zero |
| source sync 자체 실패 | 기존 유효 publication만 사용 가능 | 기존 local source와 freshness metadata 반환 | non-zero sync error | 기존 snapshot 기준만 가능 | last successful sync와 failure 표시 | sync check failed/non-zero |

qgh.v1 result와 envelope warning field 집합은 바꾸지 않는다. warning은 required `code`, `severity`, `message`와 `additionalProperties: false`를 유지한다. 현재 embedding warning이 출력하는 schema 밖 `details`/`hint`는 제거하고 cause별 stable warning code/message로 대체한다. human output도 같은 code/message를 표시한다. 각 행의 exit code, stdout/stderr, CLI/MCP warning shape와 `get`의 embedding 독립성을 release-contract test로 고정한다.

## 8. 테스트와 리뷰

각 wave는 red-green-refactor로 진행하고 task-scoped review를 통과한 뒤 다음 wave로 이동한다.

- unit: manifest parsing, prompt construction, pooling, normalization, chunk identity, matched span, field boost, fingerprint
- integration: acquisition -> offline runtime, sync -> generation build -> activation -> query -> get
- migration: 기존 DB, partial generation, dimension change, legacy JSON -> BLOB additive copy, 중단/재개, 검증 실패, rollback
- reconciliation: edit/delete/rename/tombstone, parent title change, stale source
- privacy: unexpected egress 0, token persistence 0, raw content logging 0, single-user permissions
- CLI/MCP: strict schema, stdout/stderr, warning shape, read-only tool list
- release matrix: no-default-features BM25 binary, vector build/embedding 미설정, vector init 실패, offline query
- performance: 10k sources/50k chunks에서 BM25와 hybrid gate를 별도 측정
- full verification: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`, release contract, live-model eval report

전체 diff는 마지막에 별도 code review를 받고 Critical/Important finding을 모두 수정한 뒤 완료한다.

## 9. Tracker와 배포

- 이 문서는 #47의 hardening appendix다. 사용자 서면 승인 뒤 #47에 approved design과 implementation plan 링크를 기록한다.
- 각 wave의 scope, acceptance, blocker, verification은 #47 또는 새로 만든 owning child issue에 한국어로 기록한다.
- default/light/quality preset 변경은 실제 eval artifact와 함께 #47 및 관련 ADR을 갱신해야 한다.
- release claim은 실제 shipped artifact smoke와 release matrix가 통과하기 전에는 하지 않는다.

## 10. 명시적 비목표

- Elasticsearch, OpenSearch, Meilisearch, Typesense, Qdrant, Weaviate sidecar 도입
- shared server, Web UI, multi-tenancy, org-wide discovery
- hosted embedding/rerank/query expansion을 기본값으로 사용
- answer generation, HyDE, generic RAG
- PR, Discussions, Projects, code indexing
- MCP sync/embed/write/provider-management tool 추가
- 근거 없이 ANN, ColBERT, learned sparse, reranker를 기본 활성화

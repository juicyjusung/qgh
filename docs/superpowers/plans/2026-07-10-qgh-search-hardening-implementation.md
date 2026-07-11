# qgh 검색 하드닝 구현 계획

- 작성일: 2026-07-10
- 설계: `docs/superpowers/specs/2026-07-10-qgh-search-hardening-design.md`
- owning tracker: GitHub issue #47
- 시작 commit: `d75bf08`
- 작업 branch: `feat/47-search-hardening`

## 목표

기존 Tantivy+SQLite+sqlite-vec 하이브리드 검색을 유지하면서 BM25 필수 경로를 vector stack에서 분리하고, source-faithful matched preview, immutable prepared model, atomic retrieval publication, 실제 영어·한국어·교차언어 평가를 배포 가능한 수준으로 완성한다.

## Global Constraints

- SQLite는 source identity/version/lifecycle의 authoritative store이고 Tantivy/vector는 재생성 가능한 파생 데이터다.
- `[embedding]`이 없는 `sync`, `query`, `get`, `status`는 sqlite-vec, fastembed/ORT, model file, network에 의존하지 않는다.
- 기본 query와 MCP 성공 envelope는 qgh.v1 closed schema를 유지한다. result field를 추가하지 않고 warning은 `{code,severity,message}`만 허용한다.
- 모든 결과는 stable `source_id`, canonical URL, source version, `get_args`를 유지하고 `get`으로 round-trip한다.
- query는 model acquisition이나 외부 network를 호출하지 않는다. private source/query/embedding은 GitHub 외부로 전송하지 않는다.
- hard filter는 각 retriever candidate 생성 전에 적용하며 fusion이 완화하지 않는다.
- schema migration은 additive/idempotent다. legacy table/column을 drop하지 않고 기존 DB drop/resync를 요구하지 않는다.
- dynamic quantization artifact는 bounded batch generation에 사용하지 않는다. none/static artifact만 batch comparability smoke 뒤 허용한다.
- partial/stale/corrupt vector state는 hybrid 결과를 만들지 않고 source-safe BM25로 전체 fallback한다.
- MCP v1 tool은 read-only `query`, `get`, `status`만 유지한다.
- reranker, late chunking, 형태소 분석, sparse/ColBERT/ANN은 Task 7의 사전 정의 trigger가 발생하지 않으면 구현하지 않는다.
- 각 task는 공개 interface를 통한 RED→GREEN vertical slice, task-scoped review, commit을 완료한 뒤 다음 task로 이동한다.

## Deep Module Seams

### PreparedModelStore

작은 interface 두 개만 외부에 둔다.

- `acquire(config) -> PreparedModelSnapshot`: network-capable, `sync`/`embed`만 호출한다.
- `load(config) -> PreparedModelSnapshot`: local-only, query/MCP runtime이 호출한다.

HF resolution, immutable revision, artifact checksum, path confinement, external initializer, legacy conversion은 implementation 안에 숨긴다.

### RetrievalPublicationStore

- `build(...) -> StagedPublication`
- `activate(staged) -> ActivePublication`
- `read_snapshot(|snapshot| ...)`

source snapshot, immutable Tantivy generation, optional embedding generation, CAS, recovery, retention을 implementation 안에 숨긴다. `[embedding]`이 없으면 publication의 embedding group은 `null`이고 vector metadata를 읽지 않는다.

## Task 1: BM25 capability 분리와 qgh.v1 warning 계약 복구

### 사용자 관찰 행동

1. `--no-default-features` build/test에서 sqlite-vec과 fastembed/ORT가 compile/link되지 않는다.
2. vector-capable binary라도 `[embedding]`이 없으면 base SQLite/Tantivy만 열고 vector registration/migration/runtime을 실행하지 않는다.
3. vector open/init이 실패하면 `query`는 정확히 `{code,severity,message}` warning과 BM25 결과를 반환한다.
4. `get`과 `status`는 embedding artifact가 없거나 손상돼도 model/vector 초기화 없이 동작한다.

### 구현

- `sqlite-vec`을 optional `vector-search` Cargo feature로 이동하고 `fastembed-provider`가 이를 명시적으로 포함하게 한다.
- `Store::open`은 base store만 열고, vector capability open은 별도 interface로 둔다. base migration은 `chunks`, fingerprint, embedding/vector table까지 만들지 않으며 embedding-enabled open이 additive `migrate_vector_schema`를 실행한다.
- query/sync/embed의 vector initialization과 fallback을 command별 오류 행렬에 맞춘다.
- 현재 embedding success warning의 schema 밖 `details`/`hint`를 제거하고 cause별 stable warning code/message를 사용한다.
- ADR-0012와 `qgh-hybrid-search-prd.md`의 unconditional registration 문구를 승인된 conditional contract로 개정한다.

### TDD와 검증

- RED: no-default build의 dependency graph, embedding 미설정 CLI/MCP, injected vector-init failure, warning closed schema, `get` 독립성.
- GREEN focused: `cargo test --test release_contract`, 관련 `issue_body_tracer` test, store unit tests.
- task gate: `cargo build --no-default-features`, `cargo tree --no-default-features`, `cargo test --no-default-features`, `cargo test --all-features`.

## Task 2: ModelManifestV1, PreparedModelStore와 local-only runtime

### 사용자 관찰 행동

1. `sync`/`embed`가 모델을 취득해 immutable local snapshot과 strict manifest를 만든다.
2. cached snapshot이 있으면 `query`는 network 없이 실행되고, 없으면 warning+BM25로 fallback한다.
3. built-in preset과 explicit custom manifest는 prompt/pooling/normalization/dimension/max-length/quantization을 추측하지 않는다.

### 구현

- `ModelManifestV1`과 tagged `hf|local` source, artifact role/path/SHA-256/byte-size, query/document prefix, CLS/mean, normalization, native/output dimension, max length, quantization, context template version을 추가한다.
- config에 strict `manifest_path`를 추가해 explicit custom manifest를 선택한다. legacy `model`/`model_path`와 동시에 지정하면 fail closed한다.
- `PreparedModelStore::acquire/load`를 구현한다. artifact는 manifest root 아래 canonical regular file만 허용하고 absolute path, `..`, symlink escape, checksum/size mismatch를 거부한다.
- preset registry를 `arctic-m-v2-fp32`, `granite-97m-multilingual-r2-fp32`, `granite-311m-multilingual-r2-fp32`, `arctic-l-v2-fp32`로 제한한다. revision은 commit SHA로 고정하고 acquisition 결과의 모든 runtime artifact checksum을 manifest에 기록한다. `DynamicQuantizeLinear`를 포함한 ONNX graph는 manifest가 static/none으로 선언되어도 fail closed한다.
- custom `hf:`에서 revision을 생략하면 Snowflake preset SHA를 재사용하지 않고 해당 repo default branch를 acquisition 시 commit SHA로 resolve해 manifest에 기록한다.
- external ONNX initializer를 `UserDefinedEmbeddingModel::with_external_initializer`로 실제 연결한다.
- document/query prefix, output truncation 후 L2 normalization, max length를 runtime에 적용한다. dynamic artifact는 fail closed한다.
- 동일 text를 단독/다른 위치/다른 batch size로 embed한 cosine>=0.99999 smoke를 none/static preset에 적용한다.
- legacy `hf:`/`model_path` config는 표현 가능한 경우 prepared manifest로 변환하고 closed warning을 내며, 모호하면 structured error다.
- CLI process runtime reuse와 MCP profile+manifest-hash runtime cache를 구현한다.

### TDD와 검증

- RED: strict manifest, path traversal/symlink, immutable revision, checksum/size, local checksum identity, external initializer, empty prefix, document prefix, MRL+renormalization, dynamic rejection, batch comparability, offline query.
- GREEN focused: embedding/config unit tests와 cached-offline CLI integration test.
- task gate: `cargo test embedding`, `cargo test --test issue_body_tracer --all-features`, `cargo test --all-features`.

## Task 3: 원문 좌표 chunk와 matched preview

### 사용자 관찰 행동

1. vector가 맞춘 best chunk가 source collapse 뒤에도 내부 evidence로 남는다.
2. 기존 `snippet` field가 source head가 아니라 실제 matched 원문 주변을 보여준다.
3. qgh.v1 result field, citation, `get_args`는 바뀌지 않는다.

### 구현

- tokenizer normalized offset을 `NormalizedString` alignment로 원본 UTF-8 byte span에 역매핑한다. 정확한 역매핑이 불가능하면 chunk materialization을 실패시키고 근사값을 저장하지 않는다.
- chunks에 additive columns로 chunk index, original byte/token offsets, chunker fingerprint/version, heading path를 저장한다.
- `StoredChunk`와 vector search hit가 generation/source-version/chunk identity와 best chunk evidence를 보존하게 한다.
- RRF source collapse와 `QueryHit` 내부에 lexical/vector evidence를 유지하고 원문 byte span 기반 match-aware snippet을 만든다.
- stale source version span은 결과에서 제외한다.

### TDD와 검증

- RED: NFC/NFD Hangul, CRLF, multibyte Unicode, whitespace normalization, Markdown heading/code fence, vector source dedupe, stale span, public result schema snapshot.
- GREEN focused: `cargo test chunking`, store/vector unit tests, matched preview integration test.
- task gate: `cargo test --test release_contract`, `cargo test --test issue_body_tracer`, `cargo test --all-features`.

## Task 4: resumable embedding generation과 atomic retrieval publication

### 사용자 관찰 행동

1. model/prompt/dimension/chunker/context 변경은 complete generation만 활성화한다.
2. 중단된 build는 checkpoint에서 재개하며 partial generation은 query에 보이지 않는다.
3. dimension 변경이나 migration 실패가 legacy data를 drop하지 않는다.

### 구현

- additive tables로 embedding generation state(`building|ready|active|failed`), generation chunk/vector BLOB, checkpoint, retrieval publication을 추가한다.
- BLOB은 little-endian f32로 저장하고 checksum/dimension을 검증한다. legacy JSON은 bounded batch로 복사하되 schema/row를 이 프로그램에서 drop하지 않는다.
- sqlite-vec는 dimension-specific table과 generation-row mapping을 사용하고 dimension 변경 시 기존 table을 drop하지 않는다.
- publication tuple은 `publication_id`, source snapshot sync run, Tantivy generation, optional embedding group(manifest hash/chunker/context/output dimension)을 가진다.
- sync/embed는 bounded batches로 generation을 만들고 coverage/checksum/source version/context hash/smoke를 검증한 뒤 transaction/CAS로 활성화한다.
- active 1개+previous ready 1개를 보존하고 previous는 최대 7일, stale building은 24시간 뒤 `sync`/`embed --force`에서만 정리한다. `status`/`doctor`는 report-only다.

### TDD와 검증

- RED: legacy DB upgrade, different dimensions coexist, interrupted resume, validation failure, concurrent activation CAS, crash before/after pointer, retention, BM25 publication `embedding=null`.
- GREEN focused: storage/publication unit tests와 sync→build→activate integration test.
- task gate: migration/store tests, `cargo test --test issue_body_tracer --all-features`, `cargo test --all-features`.

## Task 5: snapshot-safe query와 fail-closed purge

### 사용자 관찰 행동

1. 동시 sync 중 query는 서로 다른 source/Tantivy/vector generation을 섞지 않는다.
2. 한 source라도 embedding generation과 authoritative snapshot이 어긋나면 hybrid 전체가 BM25로 fallback한다.
3. confirmed delete/permission loss/allowlist removal은 target content를 즉시 query/get에서 차단하고 qgh-managed 파생 데이터를 제거한다.

### 구현

- `RetrievalPublicationStore::read_snapshot`에서 SQLite read transaction, immutable Tantivy path, optional vector generation을 고정하고 eligibility→retrieval→source resolution→snippet/output을 같은 source snapshot에서 수행한다.
- query runtime/network failure, corrupt/partial/mismatch state, source sync failure의 command matrix를 구현한다. `get`은 embedding 독립성을 유지한다.
- purge는 먼저 `purge_pending`을 기록해 target/profile retrieval을 fail closed하고 source body/title/labels/author, chunks, legacy JSON, generation BLOB/vec rows, content cache/log를 삭제한다.
- `secure_delete=ON`, WAL checkpoint/truncate를 사용하고 source 포함 immutable derived generation을 제거한 뒤 successor publication을 재구축한다.
- temporary Effective Scope 축소, timeout/rate-limit/5xx는 purge trigger가 아니다. configured allowlist 명시 제거, confirmed tombstone, retry 뒤 authenticated 403/404만 trigger다.
- partial purge 실패는 pending을 유지하며 `doctor`가 content 없이 실패 위치를 보고한다.

### TDD와 검증

- RED: query-vs-sync TOCTOU barrier test, edit during build, delete, permission loss, allowlist removal, temporary scope narrowing, purge failure retry, WAL/derived generation cleanup, `get` embedding independence.
- GREEN focused: store lifecycle tests와 CLI integration tests.
- task gate: `cargo test --test issue_body_tracer`, concurrency regression, `cargo test --all-features`.

## Task 6: deterministic metadata context와 lexical profile 실험

### 사용자 관찰 행동

1. issue/comment vector 입력은 원문 metadata만 사용한 versioned context를 가진다.
2. parent issue title 변경은 comment body가 같아도 새 embedding generation을 요구한다.
3. BM25 기본 ranking은 eval 승격 전까지 그대로다.

### 구현

- issue는 repository+issue number+title, comment는 repository+parent issue number+title prefix를 사용한다. stored body/snippet에는 prefix를 섞지 않는다.
- `context_template_version`과 per-source `context_hash`를 generation row에 저장한다.
- title/parent-title 변화가 successor generation coverage에 반영되게 한다.
- `LexicalRankingProfile`을 eval 내부 interface로 추가하고 title/parent-title/CJK grid를 `QueryParser::set_field_boost`로 평가할 수 있게 한다. production은 기존 v1 profile을 유지하고 사용자 knob를 추가하지 않는다.

### TDD와 검증

- RED: issue/comment exact template, no mutable metadata, source body unchanged, parent-title invalidation, field boost profile isolation, comment-only regression.
- GREEN focused: context unit tests, index profile tests, generation integration test.
- task gate: search-quality synthetic gate, `cargo test --all-features`.

## Task 7: 실제 qrels eval, preset 선택과 release 증거

### 사용자 관찰 행동

1. 실제 tokenizer/runtime을 사용하는 재현 가능한 dev 40/held-out 80 qrels가 있다.
2. 영어 우선·한국어·양방향 교차언어 품질과 CPU 비용 hard gate로 light/quality preset을 결정한다.
3. gate를 통과하지 못한 기능은 default로 승격하지 않는다.

### 구현

- public issue/comment snapshot과 gold rationale/grade/class/labeler를 `tests/fixtures/live-model-eval/`에 고정한다. 같은 issue+comments는 같은 split에 둔다.
- held-out 최소 구성: English semantic 20, Korean semantic 15, KO→EN 10, EN→KO 10, exact 10, comment-only 5, long-context 5, negative 5.
- source-level Recall@5/20, MRR@10, nDCG@10, exact top-1, hard-filter violation, get round-trip, stale leakage, duplicate crowding과 resource metrics를 계산한다.
- tuning은 dev에서만 하고 held-out은 한 번 실행한다. weighted nDCG는 EN .50, KO .20, KO→EN .15, EN→KO .10, comment/long .05다.
- reference host `Mac16,8 / M4 Pro 14-core / 48 GB / macOS 26.5.1`, intra-op=4, batch=8, warmup 1회, measured 3회, cold process 5회로 실행한다.
- light gate: warm p95<=1.5s, cold p95<=5s, RSS<=1GiB, snapshot<=500MiB, 50k backfill>=10 chunks/s, DB<=3KiB/chunk. quality gate: warm p95<=1.5s, cold<=10s, RSS<=2.5GiB, backfill>=3 chunks/s.
- hard quality gate: round-trip=1, filter/stale leakage=0, exact top1>=.95, EN Recall5>=.75, KO>=.65, 각 cross>=.60.
- 최고 weighted nDCG에서 .02 이내인 가장 작은 snapshot을 light로, 최고 nDCG(MRR, size tie-break)를 quality로 선택한다. 네 후보 모두 실패하면 vector preset 승격을 막고 BM25 release만 허용한다.
- machine artifact는 `target/qgh-eval/`, 채택 요약/provenance는 `docs/search-quality-live-model-eval.md`에 기록한다. 실제 결과에 따라 preset/ADR/PRD/#47을 갱신한다.
- recall 통과+MRR 실패, long-context 실패, Korean lexical 실패, brute-force latency 실패 trigger가 실제 발생한 경우에만 각각 reranker, late chunking, Lindera, ANN을 별도 ADR 후보로 기록한다. 이 task에서 자동 구현하지 않는다.

### TDD와 검증

- RED: qrels schema/split leakage/class counts/metric math/provenance/resource gate와 실제 runtime smoke.
- GREEN focused: deterministic metric tests, synthetic contract gate, ignored opt-in live-model run.
- task gate: 네 preset live run, `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, `cargo test --all-features`, release artifact/install smoke.

## 완료 조건

- Task 1~7의 task review에서 Critical/Important 0건.
- whole-branch review Critical/Important 0건.
- 관련 full Rust/release/live-eval gate 통과.
- #47에 승인 설계, 구현 commit, 실제 eval 결과, 남은 conditional follow-up을 한국어로 동기화.
- commit은 branch에 남기고 push/PR/merge/deploy는 사용자 지시 없이는 수행하지 않는다.

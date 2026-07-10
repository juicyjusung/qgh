# Task 3 구현 보고서

## 결과

- `TokenizedText`가 normalized text/token span과 원문 text/token span을 함께 보유한다.
- fastembed tokenizer는 `NormalizedString::convert_offsets`로 normalized byte range를 원문 UTF-8 range로 역매핑한다. 매핑 실패, 잘못된 UTF-8 boundary, token/span cardinality 불일치는 structured failure로 닫힌다.
- `MarkdownChunk`와 SQLite `chunks`에 원문 byte/token offset, chunk index, chunker version/fingerprint, heading path를 additive하게 저장한다. 기존 vector schema가 없는 BM25-only profile은 여전히 `chunks`를 읽지 않는다.
- vector source collapse는 source id만 버리지 않고 best chunk와 source-version hash를 `VectorSearchHit` 내부에 보존한다.
- RRF `QueryHit`에는 lexical/vector evidence seam이 남고, source version hash와 원문 span이 유효할 때만 matched snippet을 사용한다. stale/invalid span은 기존 head preview로 안전하게 fallback한다.
- 공개 qgh.v1 result field, citation semantics, `get_args`는 변경하지 않았다.

## TDD RED → GREEN

1. RED: normalized/original span fields가 없어 whitespace-normalizing chunk test가 compile-fail했다. GREEN: `rtk cargo test --all-features chunking::tests -- --nocapture` 7 passed.
2. RED: 기존 canonical-body assertion이 원문 body를 기대하도록 바뀐 계약에서 실패했다. GREEN: CRLF/whitespace normalization fixture가 원문 slice와 offset을 검증한다.
3. RED: unmappable tokenizer가 근사 offset을 저장할 수 있는 경로를 가정했다. GREEN: original span cardinality mismatch를 structured chunking failure로 검증한다.
4. RED: `VectorSearchHit`가 source id/distance만 보유해 best chunk를 잃었다. GREEN: vector smoke test가 best chunk body와 source-version hash를 검증한다.
5. RED: matched span/stale span preview 계약이 없었다. GREEN: current span은 matched text를, stale span은 head preview를 반환하는 regression test를 추가했다.

## 검증

- `rtk cargo test --all-features chunking::tests -- --nocapture` — 7 passed
- `rtk cargo test --all-features commands::tests -- --nocapture` — 5 passed
- `rtk cargo test --all-features store::tests -- --nocapture` — 7 passed
- `rtk cargo test --all-features --test issue_body_tracer` — 117 passed
- `rtk cargo test --all-features --test release_contract` — 2 passed
- `rtk cargo test --all-features --lib` — 53 passed, 1 ignored
- `rtk cargo test --no-default-features` — 149 passed
- `rtk cargo clippy --all-targets --all-features -- -D warnings` — passed
- `rtk cargo fmt --all`, `rtk git diff --check` — passed

## Concern

`search_quality_eval::curated_search_quality_eval_gate_passes`는 Task 3 코드 경로가 아니라 Task 2의 prepared-model cache가 deterministic fixture fingerprint 대신 manifest-hash revision을 선택하는 기존 cache/environment 결합으로 실패했다. 실패 관찰값은 active hash `3af509…` 대 fixture hash `4b3123…`이며, 이 task에서는 model acquisition/fingerprint 정책을 변경하지 않았다.


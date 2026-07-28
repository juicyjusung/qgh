<!--
Closes 링크는 필수다. 이 줄이 비어 있으면 머지 후 이슈가 자동으로 닫히지 않고
트래커에 좀비 open issue가 남는다. 이슈 없는 변경이면 `Closes:` 대신
`No issue: <이유>`를 쓴다.
-->

Closes #

## 무엇을 바꿨나

<!-- 변경의 목적. PRD/ADR/이슈 AC 중 어떤 요구를 만족시키는지. -->

## 어떻게 검증했나

<!-- 실행한 명령과 결과. "테스트 통과" 같은 요약이 아니라 근거를 남긴다. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features`

변경 성격에 따라 추가:

- [ ] CLI/MCP 변경 → JSON 스키마, 구조화 에러, stdout/stderr 분리, `query -> get -> cite` 라운드트립 확인
- [ ] sync/search/storage 변경 → pagination, edit/delete/rename, tombstone/reconciliation, rate-limit, stale 결과 동작 확인
- [ ] privacy 민감 변경 → 예상 밖 네트워크 egress 없음, 토큰 미저장, 로그/픽스처에 private 내용 없음
- [ ] docs-only → 렌더링 결과와 참조 경로/링크 존재 확인

## 가드레일 확인

- [ ] MVP 범위(GitHub Issues/comments/Wiki 검색)를 넘지 않는다
- [ ] BM25-only 경로가 `sync`/`query`/`get`/`status`에서 그대로 동작한다
- [ ] MCP는 read-only `query`/`get`/`status`만 유지한다
- [ ] config/CLI/MCP 스키마가 strict하다(unknown key·오타·잘못된 enum은 구조화 에러로 실패)
- [ ] GitHub 토큰 리터럴을 config/픽스처/로그/문서에 넣지 않았다

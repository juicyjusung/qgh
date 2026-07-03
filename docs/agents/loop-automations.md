# Loop Automations 운영 가이드 (Codex 기준)

qgh loop-engineering 운영 표면은 두 개다. 규칙의 원본은 `LOOP.md`와
`loop-constraints.md`이며, 이 문서는 등록/운영 절차만 다룬다.

## 1. Implementation Lane (`codex exec` + launchd) — L2

드라이버: `scripts/loop/qgh-loop.sh`. 흐름은 `LOOP.md`의
"L2 Implementation Lane" 참고.

수동 1회 실행:

```bash
scripts/loop/qgh-loop.sh
```

launchd 등록 (3시간 간격, 스모크 런 통과 후에만):

```bash
cp scripts/loop/com.juicyjusung.qgh-loop.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.juicyjusung.qgh-loop.plist
```

해제:

```bash
launchctl unload ~/Library/LaunchAgents/com.juicyjusung.qgh-loop.plist
```

로그: dispatcher는 `~/Library/Logs/qgh-loop.log`, 이슈별 worker는
`~/Library/Logs/qgh-loop/issue-<n>.log`. 런 이력: GitHub #19 comments.
동시 처리: dispatcher가 최대 3 lane까지 병렬 spawn. 병렬로 풀 이슈
세트는 파일 충돌 없는 조합으로 — `/orchestrating-qgh-worktrees`로 선정
후 라벨 부여 권장.

재시도 정책: 레인 실패 시 이슈에 `needs-info` 라벨이 붙고 큐에서
빠진다. 사람이 원인 확인 후 `ready-for-agent`를 다시 붙여야 재시도된다.
이 라벨 사이클이 "max 3 attempts"의 실질 게이트다.

## 2. Triage 루프 (Codex 앱 Automations 탭) — L1

Codex 앱 > Automations에서 아래 두 개 등록. Environment는 local
checkout(`~/projects/juicyjusung/juicy-qgh`).

### Issue Triage — cadence 1d

```text
Before anything: read loop-constraints.md and enforce every rule.
Run $issue-triage on this project. GitHub Issues for juicyjusung/qgh are
the tracker source of truth. Read issue #18 body for current loop state.

Report only — do not modify issues, labels, or source files:
- Open actionable count + delta since last run
- Top 5 prioritized issues, one-sentence summaries
- Suggested labels (proposed only)
- "needs human" bucket for ambiguous or guardrail-sensitive items
Append the run to issue #19 as a comment and update the #18 snapshot.
```

### Daily Triage — cadence 1d (Issue Triage 이후)

```text
Before anything: read loop-constraints.md and enforce every rule.
Run $loop-triage on this project. Read issue #18 body first.

Report only — do not modify source files:
- High-Priority items / Watch list / Noise
- State updates worth remembering
Append the run to issue #19 as a comment and update the #18 snapshot.
```

## 3. Kill switch

#18 body에 `Loop status: paused` 한 줄 추가 → 레인 스크립트와
Automations 전부 즉시 no-op. 해제는 해당 줄 제거.

## 4. 사람 리듬 (2주 집중 운영 기준)

- 아침: draft PR 리뷰 → ready 전환/머지, `needs-info` 원인 확인
- 저녁: `ready-for-agent` 큐 재장전 (슬라이스 이슈 라벨 부여)
- 주 1회: `npx @cobusgreyling/loop-audit . --suggest`, #19 이력 회고

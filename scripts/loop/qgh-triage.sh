#!/usr/bin/env bash
# qgh Daily/Issue Triage (L1 report-only) — see LOOP.md.
# The script gathers tracker/repo inventory, a read-only codex session
# analyzes it, and the script posts the report to #18 (comment) and #19.
# The agent never writes; the script writes only to #18/#19.
set -euo pipefail

REPO="juicyjusung/qgh"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STATE_ISSUE=18
RUNLOG_ISSUE=19
TRIAGE_TIMEOUT_SECS=900
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

notify() {
  command -v osascript >/dev/null 2>&1 || return 0
  osascript -e "display notification \"$2\" with title \"qgh-triage\" subtitle \"$1\" sound name \"Glass\"" 2>/dev/null || true
}

run_timed() { # watchdog: $1 = timeout secs, rest = command
  local secs="$1"; shift
  "$@" &
  local cmd=$!
  ( sleep "$secs" && kill "$cmd" 2>/dev/null ) &
  local dog=$!
  local rc=0
  wait "$cmd" || rc=$?
  kill "$dog" 2>/dev/null; wait "$dog" 2>/dev/null || true
  return $rc
}

# --- kill switch (fail closed) ------------------------------------------------
state_body="$(gh issue view "$STATE_ISSUE" -R "$REPO" --json body -q .body)" \
  || { log "cannot read #$STATE_ISSUE — fail closed, exit"; exit 0; }
if printf '%s\n' "$state_body" | grep -q '^Loop status: paused'; then
  log "kill switch active — exit"; exit 0
fi

# --- gather inventory (script-side, deterministic) ------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

{
  echo "## open issues (number/state/labels/updated/title)"
  gh issue list -R "$REPO" --state open --limit 100 \
    --json number,title,labels,updatedAt \
    --jq '.[] | "#\(.number)\t\([.labels[].name] | join(","))\t\(.updatedAt)\t\(.title)"'
  echo
  echo "## open PRs"
  gh pr list -R "$REPO" --state open --json number,title,isDraft,headRefName \
    --jq '.[] | "PR #\(.number)\t\(.headRefName)\tdraft=\(.isDraft)\t\(.title)"'
  echo
  echo "## recently merged PRs (last 5)"
  gh pr list -R "$REPO" --state merged --limit 5 --json number,title,mergedAt \
    --jq '.[] | "PR #\(.number)\t\(.mergedAt)\t\(.title)"'
  echo
  echo "## loop state snapshot (#18 body)"
  printf '%s\n' "$state_body"
  echo
  echo "## recent main commits"
  git -C "$ROOT" log origin/main --oneline -15
  echo
  echo "## active lanes (worktrees/claims)"
  ls "$ROOT/.worktrees" 2>/dev/null || echo "(none)"
} > "$TMP/inventory.md" 2>&1

# --- read-only triage session ---------------------------------------------------
log "triage session start"
run_timed "$TRIAGE_TIMEOUT_SECS" codex exec \
  -C "$TMP" \
  -s read-only \
  --skip-git-repo-check \
  -o "$TMP/report.md" \
  "You are the L1 report-only triage agent for the qgh project
(loop-engineering Issue Triage + Daily Triage combined). You must not
modify anything anywhere; produce a report only.

Analyze this tracker/repo inventory:

$(cat "$TMP/inventory.md")

Produce a concise Korean markdown report with exactly these sections:
## 우선순위 Top 5
- 이슈/PR별 한 줄 요약 + 왜 지금 중요한지 + 권장 다음 행동
## 라벨 제안 (제안만 — 적용 금지)
- 예: '#NN에 ready-for-agent 부여 검토 (의존 충족됨)' / 근거 한 줄
## Needs Human
- 사람 판단 필요 항목 (스코프/프라이버시/스키마/릴리즈/모호함)
## Watch
- 지금 행동 불필요, 관찰 대상
## 상태 변화
- 지난 스냅샷(#18) 대비 달라진 사실

Rules: be brutally concise; no invention beyond the inventory; dependency
order for hybrid slices follows the #47 slice map; issues labeled
needs-info are parked pending human action." \
  || { notify "triage 실패" "codex 세션 에러/타임아웃"; log "triage session failed"; exit 1; }

# --- post (script is the only writer; #18 comment + #19 pointer) ----------------
REPORT="$(cat "$TMP/report.md")"
gh issue comment "$STATE_ISSUE" -R "$REPO" --body "**📋 Daily Triage (L1 report-only)** ($TS)

$REPORT" >/dev/null
gh issue comment "$RUNLOG_ISSUE" -R "$REPO" --body "**Daily Triage 실행** ($TS) — 리포트는 #$STATE_ISSUE 코멘트" >/dev/null
notify "triage 완료" "리포트: #$STATE_ISSUE 코멘트"
log "done — report posted to #$STATE_ISSUE"

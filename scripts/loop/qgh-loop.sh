#!/usr/bin/env bash
# qgh Implementation Lane (L2 assisted) — dispatcher + parallel workers.
# See LOOP.md "L2 Implementation Lane".
#
# Dispatcher (no args): kill-switch check, then fills up to MAX_LANES slots —
#   pick ready-for-agent issue (oldest first) -> claim -> create worktree
#   (serial git ops) -> spawn a detached worker per issue.
# Worker (--worker <issue> <worktree>): maker -> independent gates -> checker
#   -> draft PR for exactly one issue. Logs to ~/Library/Logs/qgh-loop/.
# Attempt limiting: a failed issue gets needs-info and is skipped until a
# human re-adds ready-for-agent.
set -euo pipefail

REPO="juicyjusung/qgh"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MAX_LANES=3
MAKER_TIMEOUT_SECS=2700    # 45m — a hung codex stream must not pin a lane slot
CHECKER_TIMEOUT_SECS=1200  # 20m
STATE_ISSUE=18
RUNLOG_ISSUE=19
LOGDIR="$HOME/Library/Logs/qgh-loop"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

runlog() { # append one run entry to #19 (append-only run history)
  gh issue comment "$RUNLOG_ISSUE" -R "$REPO" --body "$1" >/dev/null || true
}

notify() { # best-effort macOS desktop notification
  command -v osascript >/dev/null 2>&1 || return 0
  osascript -e "display notification \"$2\" with title \"qgh-loop\" subtitle \"$1\" sound name \"Glass\"" 2>/dev/null || true
}

run_timed() { # watchdog: $1 = timeout secs, rest = command; kills on timeout
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

# =========================== status mode ====================================
if [ "${1:-}" = "status" ]; then
  printf '=== qgh-loop status %s ===\n' "$(date +%H:%M:%S)"
  body="$(gh issue view "$STATE_ISSUE" -R "$REPO" --json body -q .body 2>/dev/null || true)"
  if printf '%s\n' "$body" | grep -q '^Loop status: paused'; then
    echo "⛔ kill switch: PAUSED (#$STATE_ISSUE)"
  else
    echo "🟢 loop: active  (pause: #$STATE_ISSUE body에 'Loop status: paused' 행 추가)"
  fi
  echo
  echo "--- lanes (max $MAX_LANES) ---"
  found=0
  for c in "$ROOT/.worktrees"/.claim-*; do
    [ -d "$c" ] || continue
    found=1
    n="${c##*/.claim-}"
    pid="$(cat "$c/pid" 2>/dev/null || echo '?')"
    if kill -0 "$pid" 2>/dev/null; then
      elapsed="$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ')"
      stage="$(grep -E '^\[[0-9:]+\]' "$LOGDIR/issue-$n.log" 2>/dev/null | tail -1)"
      echo "🔄 #$n  elapsed ${elapsed:-?}  ${stage:-starting…}"
    else
      echo "💀 #$n  worker dead — stale claim (다음 dispatcher가 정리)"
    fi
  done
  for w in "$ROOT/.worktrees"/issue-*; do
    [ -d "$w" ] || continue
    n="${w##*/issue-}"
    [ -d "$ROOT/.worktrees/.claim-$n" ] && continue
    echo "🟡 #$n  실패 작업물 보존 worktree (사람 확인 대기)"
    found=1
  done
  [ "$found" -eq 0 ] && echo "(idle — 레인 없음)"
  echo
  echo "--- queue (ready-for-agent) ---"
  gh issue list -R "$REPO" --label ready-for-agent --state open --json number,title,labels \
    --jq '.[] | if ([.labels[].name] | index("needs-info")) then "⏸ #\(.number) \(.title)  ← needs-info 파킹" else "▶ #\(.number) \(.title)" end' \
    2>/dev/null || echo "(gh 조회 실패)"
  echo
  echo "--- recent outcomes ---"
  grep -h -E 'FAIL #|done: #' "$LOGDIR"/issue-*.log 2>/dev/null | tail -5 || true
  echo
  echo "--- open PRs ---"
  gh pr list -R "$REPO" --state open \
    --json number,title,isDraft \
    --jq '.[] | "PR #\(.number) \(.title)\(if .isDraft then "  (draft — 리뷰 대기)" else "" end)"' 2>/dev/null || true
  exit 0
fi

# =========================== worker mode ====================================
if [ "${1:-}" = "--worker" ]; then
  ISSUE="$2"; WT="$3"
  BR="agent/issue-$ISSUE"
  CLAIM="$ROOT/.worktrees/.claim-$ISSUE"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$CLAIM" "$TMP" 2>/dev/null || true' EXIT

  fail() { # label needs-info, comment outcome on the issue, record, clean up
    local reason="$1" detail="${2:-}" disposition
    log "FAIL #$ISSUE: $reason"
    gh issue edit "$ISSUE" -R "$REPO" --add-label needs-info >/dev/null || true
    # preserve the worktree if it holds any work (commits or dirty tree)
    if [ -d "$WT" ] && { [ "$(git -C "$WT" rev-list --count origin/main..HEAD 2>/dev/null || echo 0)" -gt 0 ] \
        || [ -n "$(git -C "$WT" status --porcelain 2>/dev/null)" ]; }; then
      disposition="worktree 보존: \`.worktrees/issue-$ISSUE\` (작업물 있음 — 사람이 확인)"
    else
      git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
      git -C "$ROOT" branch -D "$BR" 2>/dev/null || true
      disposition="worktree 정리 완료 (보존할 작업물 없음)"
    fi
    local outcome="**🔴 Implementation Lane 실패** ($TS)
- stage: $reason
- $disposition
- 재시도: 원인 해결 후 \`needs-info\` 라벨 제거 (\`ready-for-agent\` 유지)"
    if [ -n "$detail" ]; then
      outcome="$outcome

<details><summary>에이전트 마지막 메시지</summary>

\`\`\`
$detail
\`\`\`
</details>"
    fi
    gh issue comment "$ISSUE" -R "$REPO" --body "$outcome" >/dev/null || true
    runlog "**Implementation Lane 실패** ($TS) — #$ISSUE, stage: $reason (상세는 #$ISSUE 코멘트)"
    notify "#$ISSUE 실패" "$reason"
    exit 1
  }

  gh issue view "$ISSUE" -R "$REPO" --json title,body,comments \
    --jq '{title: .title, body: .body, comments: [.comments[].body]}' > "$TMP/issue.json"

  MAKER_RULES="Rules:
- Touch only files required by this issue. Never touch denylist paths in loop-constraints.md.
- All acceptance criteria in the issue must be satisfied.
- Run: cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test — all must pass.
- If the crate defines cargo features, also: cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features. Feature-gated code that never compiles in gates is a REJECT.
- Commit with Conventional Commits (English type/subject). Do NOT push. Do NOT open PRs.
- If the issue is ambiguous or requires denylist changes, stop and explain instead of guessing."

  MAKER_PROMPT="You are the maker in a maker/checker loop for the qgh project.
Read AGENTS.md and loop-constraints.md first and obey both.

Implement GitHub issue #$ISSUE. Full issue JSON (title/body/comments):
$(cat "$TMP/issue.json")

$MAKER_RULES"

  gate() { # run a gate, keep its tail for the failure comment
    local name="$1"; shift
    ( cd "$WT" && "$@" ) > "$TMP/gate.log" 2>&1 \
      || fail "gate: $name" "$(tail -c 1500 "$TMP/gate.log")"
  }

  # --- maker -> gates -> checker, with one repair round on REJECT -------------
  round=1
  while :; do
    log "maker session start (#$ISSUE, round $round)"
    run_timed "$MAKER_TIMEOUT_SECS" codex exec \
      -C "$WT" \
      -s workspace-write \
      --add-dir "$ROOT/.git" \
      -c 'sandbox_workspace_write.network_access=true' \
      -o "$TMP/maker-last.txt" \
      "$MAKER_PROMPT" \
      || fail "maker: codex exec error or timeout (round $round)" "$(tail -c 1500 "$TMP/maker-last.txt" 2>/dev/null || true)"

    if [ "$(git -C "$WT" rev-list --count origin/main..HEAD)" -eq 0 ]; then
      fail "maker: no commits produced" "$(tail -c 1500 "$TMP/maker-last.txt" 2>/dev/null || true)"
    fi

    log "verification gates (#$ISSUE, round $round)"
    GATES_RUN="cargo fmt --check, clippy -D warnings, cargo test, clippy --all-features -D warnings, cargo test --all-features"
    gate "cargo fmt"                    cargo fmt --all --check
    gate "cargo clippy"                 cargo clippy --all-targets -- -D warnings
    gate "cargo test"                   cargo test
    # feature-gated code must be exercised too (BM25 default path + hybrid path)
    gate "cargo clippy (all-features)"  cargo clippy --all-targets --all-features -- -D warnings
    gate "cargo test (all-features)"    cargo test --all-features
    # release tooling present -> prove the dist plan too (checker sandbox has no network)
    if [ -f "$WT/dist-workspace.toml" ] || grep -q 'workspace.metadata.dist' "$WT/Cargo.toml" 2>/dev/null; then
      if command -v dist >/dev/null 2>&1; then
        gate "cargo dist plan"          dist plan
        GATES_RUN="$GATES_RUN, cargo dist plan"
      fi
    fi

    log "checker session start (#$ISSUE, round $round)"
    git -C "$WT" diff origin/main...HEAD > "$TMP/lane.diff"
    run_timed "$CHECKER_TIMEOUT_SECS" codex exec \
      -C "$WT" \
      -s read-only \
      -o "$TMP/verdict.txt" \
      "$(sed -n '/instructions = """/,/"""/p' "$ROOT/.codex/agents/verifier.toml" | sed '1d;$d')

Context: maker implemented GitHub issue #$ISSUE on branch $BR (review round $round).
Issue JSON: $(cat "$TMP/issue.json")
Gates already passed independently: $GATES_RUN.
Your sandbox has no network access — do not attempt network commands; trust the gate evidence above for anything requiring network.
Diff to review:
$(cat "$TMP/lane.diff")

End your reply with exactly one final line: 'VERDICT: APPROVE' or 'VERDICT: REJECT' or 'VERDICT: ESCALATE_HUMAN'." \
      || fail "checker: codex exec error or timeout (round $round)"

    grep -q 'VERDICT: APPROVE' "$TMP/verdict.txt" && break

    if [ "$round" -ge 2 ]; then
      fail "checker: verdict not APPROVE after repair round" "$(tail -c 1500 "$TMP/verdict.txt")"
    fi
    log "checker REJECT — repair round (#$ISSUE)"
    round=2
    MAKER_PROMPT="You are the maker in a maker/checker loop for the qgh project.
Read AGENTS.md and loop-constraints.md first and obey both.

You already implemented GitHub issue #$ISSUE in this worktree, but the
independent checker REJECTED it. Fix EVERY finding below. Keep the valid
existing work; make targeted fixes and commit them.

Checker findings:
$(tail -c 2500 "$TMP/verdict.txt")

Issue JSON for reference:
$(cat "$TMP/issue.json")

$MAKER_RULES"
  done

  # --- draft PR ---------------------------------------------------------------
  log "opening draft PR (#$ISSUE)"
  git -C "$WT" push -u origin "$BR" --quiet
  TITLE="$(git -C "$WT" log -1 --format=%s)"
  PR_URL=$(gh pr create -R "$REPO" --draft --base main --head "$BR" \
    --title "$TITLE" \
    --body "Closes #$ISSUE

Implementation Lane (L2 assisted) 자동 생성 draft PR.
- maker/checker: codex exec 분리 세션, checker VERDICT: APPROVE
- 게이트: cargo fmt --check / clippy -D warnings / cargo test 전부 green (스크립트 독립 재검증)
- 머지는 사람이 리뷰 후 진행. LOOP.md 참고.")

  gh issue edit "$ISSUE" -R "$REPO" --remove-label ready-for-agent >/dev/null || true
  gh issue comment "$ISSUE" -R "$REPO" --body "**🟢 Implementation Lane 성공** ($TS)
- draft PR: $PR_URL — 사람 리뷰 후 ready 전환·머지
- 검증: fmt / clippy / test / all-features 게이트 green + checker APPROVE" >/dev/null || true
  runlog "**Implementation Lane 성공** ($TS) — #$ISSUE → $PR_URL (draft)"
  notify "#$ISSUE 성공" "draft PR 생성 — 리뷰 대기"

  # lane slot 반환 — 브랜치는 origin에 있음
  git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  git -C "$ROOT" branch -D "$BR" 2>/dev/null || true
  log "done: #$ISSUE -> $PR_URL"
  exit 0
fi

# ========================== dispatcher mode ==================================
mkdir -p "$ROOT/.worktrees" "$LOGDIR"

open_worktree_in_herdr() { # best-effort: open the lane worktree as a tab in the qgh herdr workspace
  local issue="$1" wt="$2" ws pane
  command -v herdr >/dev/null 2>&1 || return 0
  ws="$(herdr workspace list 2>/dev/null | python3 -c '
import sys, json
root = sys.argv[1]
data = json.load(sys.stdin)
for w in data.get("result", {}).get("workspaces", []):
    t = w.get("worktree") or {}
    if t.get("checkout_path") == root and not t.get("is_linked_worktree"):
        print(w["workspace_id"]); break
' "$ROOT" 2>/dev/null)" || return 0
  [ -n "$ws" ] || return 0
  # close stale tabs from previous runs of the same issue (dead tail panes)
  herdr tab list --workspace "$ws" 2>/dev/null | python3 -c '
import sys, json
label = sys.argv[1]
for t in json.load(sys.stdin).get("result", {}).get("tabs", []):
    if t.get("label") == label:
        print(t["tab_id"])
' "issue-$issue" 2>/dev/null | while read -r stale; do
    herdr tab close "$stale" >/dev/null 2>&1 || true
  done
  pane="$(herdr tab create --workspace "$ws" --label "issue-$issue" --no-focus 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["root_pane"]["pane_id"])' 2>/dev/null)" || return 0
  [ -n "$pane" ] || return 0
  herdr pane run "$pane" "cd $wt && tail -f $LOGDIR/issue-$issue.log" 2>/dev/null || true
  log "herdr tab opened for #$issue (workspace $ws, pane $pane)"
}

# --- kill switch -------------------------------------------------------------
# Activation = a line STARTING with the phrase; prose that merely mentions the
# phrase (e.g. the kill-switch instructions themselves) must not trigger it.
# Fail closed: if the state issue cannot be read, do not run.
state_body="$(gh issue view "$STATE_ISSUE" -R "$REPO" --json body -q .body)" \
  || { log "cannot read #$STATE_ISSUE — fail closed, exit"; exit 0; }
if printf '%s\n' "$state_body" | grep -q '^Loop status: paused'; then
  log "kill switch active (#$STATE_ISSUE: Loop status: paused) — exit"; exit 0
fi

# --- clear stale claims (crashed workers) -------------------------------------
for c in "$ROOT/.worktrees"/.claim-*; do
  [ -d "$c" ] || continue
  pid="$(cat "$c/pid" 2>/dev/null || true)"
  if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
    log "clearing stale claim $(basename "$c")"
    rm -rf "$c"
  fi
done

active_lanes() { find "$ROOT/.worktrees" -maxdepth 1 -type d -name 'issue-*' | wc -l | tr -d ' '; }

if [ "$(active_lanes)" -ge "$MAX_LANES" ]; then
  log "lane capacity reached ($(active_lanes)/$MAX_LANES) — exit"; exit 0
fi

git -C "$ROOT" fetch origin main --quiet

# --- fill free slots -----------------------------------------------------------
launched=0
for n in $(gh issue list -R "$REPO" --label ready-for-agent --state open \
             --json number,labels \
             --jq '[.[] | select([.labels[].name] | index("needs-info") | not) | .number] | sort | .[]'); do
  [ "$(active_lanes)" -ge "$MAX_LANES" ] && break
  [ -d "$ROOT/.worktrees/issue-$n" ] && continue
  git -C "$ROOT" show-ref --verify --quiet "refs/heads/agent/issue-$n" && continue
  # atomic claim guards against a concurrently running dispatcher
  mkdir "$ROOT/.worktrees/.claim-$n" 2>/dev/null || continue

  if ! git -C "$ROOT" worktree add "$ROOT/.worktrees/issue-$n" -b "agent/issue-$n" origin/main --quiet; then
    log "worktree add failed for #$n — skipping"
    rm -rf "$ROOT/.worktrees/.claim-$n"
    continue
  fi

  "$0" --worker "$n" "$ROOT/.worktrees/issue-$n" > "$LOGDIR/issue-$n.log" 2>&1 &
  echo $! > "$ROOT/.worktrees/.claim-$n/pid"
  log "lane launched: #$n (pid $!, log $LOGDIR/issue-$n.log)"
  open_worktree_in_herdr "$n" "$ROOT/.worktrees/issue-$n"
  launched=$((launched+1))
done

if [ "$launched" -eq 0 ]; then
  log "no lanes launched (queue empty or all claimed) — no-op exit"
else
  log "dispatched $launched lane(s); workers run detached"
fi

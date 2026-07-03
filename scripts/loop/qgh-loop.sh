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
STATE_ISSUE=18
RUNLOG_ISSUE=19
LOGDIR="$HOME/Library/Logs/qgh-loop"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

runlog() { # append one run entry to #19 (append-only run history)
  gh issue comment "$RUNLOG_ISSUE" -R "$REPO" --body "$1" >/dev/null || true
}

# =========================== worker mode ====================================
if [ "${1:-}" = "--worker" ]; then
  ISSUE="$2"; WT="$3"
  BR="agent/issue-$ISSUE"
  CLAIM="$ROOT/.worktrees/.claim-$ISSUE"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$CLAIM" "$TMP" 2>/dev/null || true' EXIT

  fail() { # label needs-info, record, clean up lane
    local reason="$1" disposition
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
    runlog "**Implementation Lane 실패** ($TS)
- issue: #$ISSUE
- stage: $reason
- $disposition
- action: \`needs-info\` 라벨 추가. 사람이 확인 후 \`ready-for-agent\` 재부여 시 재시도."
    exit 1
  }

  gh issue view "$ISSUE" -R "$REPO" --json title,body,comments \
    --jq '{title: .title, body: .body, comments: [.comments[].body]}' > "$TMP/issue.json"

  # --- maker: codex exec implements ----------------------------------------
  log "maker session start (#$ISSUE)"
  codex exec \
    -C "$WT" \
    -s workspace-write \
    --add-dir "$ROOT/.git" \
    -c 'sandbox_workspace_write.network_access=true' \
    -o "$TMP/maker-last.txt" \
    "You are the maker in a maker/checker loop for the qgh project.
Read AGENTS.md and loop-constraints.md first and obey both.

Implement GitHub issue #$ISSUE. Full issue JSON (title/body/comments):
$(cat "$TMP/issue.json")

Rules:
- Touch only files required by this issue. Never touch denylist paths in loop-constraints.md.
- All acceptance criteria in the issue must be satisfied.
- Run: cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test — all must pass.
- Commit with Conventional Commits (English type/subject). Do NOT push. Do NOT open PRs.
- If the issue is ambiguous or requires denylist changes, stop and explain instead of guessing." \
    || fail "maker: codex exec error"

  # require at least one commit
  if [ "$(git -C "$WT" rev-list --count origin/main..HEAD)" -eq 0 ]; then
    fail "maker: no commits produced (see maker-last message in #$RUNLOG_ISSUE)"
  fi

  # --- independent gates (script does not trust the maker) ------------------
  log "verification gates (#$ISSUE)"
  ( cd "$WT" && cargo fmt --all --check )                      || fail "gate: cargo fmt"
  ( cd "$WT" && cargo clippy --all-targets -- -D warnings )    || fail "gate: cargo clippy"
  ( cd "$WT" && cargo test )                                   || fail "gate: cargo test"

  # --- checker: separate read-only codex session -----------------------------
  log "checker session start (#$ISSUE)"
  git -C "$WT" diff origin/main...HEAD > "$TMP/lane.diff"
  codex exec \
    -C "$WT" \
    -s read-only \
    -o "$TMP/verdict.txt" \
    "$(sed -n '/instructions = """/,/"""/p' "$ROOT/.codex/agents/verifier.toml" | sed '1d;$d')

Context: maker implemented GitHub issue #$ISSUE on branch $BR.
Issue JSON: $(cat "$TMP/issue.json")
Gates already passed independently: cargo fmt --check, clippy -D warnings, cargo test.
Diff to review:
$(cat "$TMP/lane.diff")

End your reply with exactly one final line: 'VERDICT: APPROVE' or 'VERDICT: REJECT' or 'VERDICT: ESCALATE_HUMAN'." \
    || fail "checker: codex exec error"

  if ! grep -q 'VERDICT: APPROVE' "$TMP/verdict.txt"; then
    verdict_tail="$(tail -c 1500 "$TMP/verdict.txt")"
    runlog "**Checker verdict (non-APPROVE)** ($TS) — issue #$ISSUE
\`\`\`
$verdict_tail
\`\`\`"
    fail "checker: verdict not APPROVE"
  fi

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
  runlog "**Implementation Lane 성공** ($TS)
- issue: #$ISSUE
- branch: \`$BR\`
- PR: $PR_URL (draft)
- 검증: fmt/clippy/test green + checker APPROVE"

  # lane slot 반환 — 브랜치는 origin에 있음
  git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  git -C "$ROOT" branch -D "$BR" 2>/dev/null || true
  log "done: #$ISSUE -> $PR_URL"
  exit 0
fi

# ========================== dispatcher mode ==================================
mkdir -p "$ROOT/.worktrees" "$LOGDIR"

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
  launched=$((launched+1))
done

if [ "$launched" -eq 0 ]; then
  log "no lanes launched (queue empty or all claimed) — no-op exit"
else
  log "dispatched $launched lane(s); workers run detached"
fi

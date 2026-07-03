#!/usr/bin/env bash
# qgh Implementation Lane (L2 assisted) — see LOOP.md "L2 Implementation Lane".
# Picks one ready-for-agent issue, implements it via codex exec in an isolated
# worktree, verifies with independent gates + checker session, opens a draft PR.
# Never merges. Attempt limiting: an issue labeled needs-info is skipped until
# a human re-adds ready-for-agent.
set -euo pipefail

REPO="juicyjusung/qgh"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MAX_LANES=3
STATE_ISSUE=18
RUNLOG_ISSUE=19
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

runlog() { # append one run entry to #19 (append-only run history)
  gh issue comment "$RUNLOG_ISSUE" -R "$REPO" --body "$1" >/dev/null || true
}

# --- single-instance lock ---------------------------------------------------
LOCK="$ROOT/.worktrees/.lane.lock"
mkdir -p "$ROOT/.worktrees"
if ! mkdir "$LOCK" 2>/dev/null; then
  log "another lane run active ($LOCK) — exit"; exit 0
fi
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

# --- kill switch ------------------------------------------------------------
if gh issue view "$STATE_ISSUE" -R "$REPO" --json body -q .body | grep -q 'Loop status: paused'; then
  log "kill switch active (#$STATE_ISSUE: Loop status: paused) — exit"; exit 0
fi

# --- lane capacity ----------------------------------------------------------
active=$(find "$ROOT/.worktrees" -maxdepth 1 -type d -name 'issue-*' | wc -l | tr -d ' ')
if [ "$active" -ge "$MAX_LANES" ]; then
  log "lane capacity reached ($active/$MAX_LANES) — exit"; exit 0
fi

# --- pick oldest ready-for-agent issue without an active lane ----------------
ISSUE=""
for n in $(gh issue list -R "$REPO" --label ready-for-agent --state open \
             --json number,labels \
             --jq '[.[] | select([.labels[].name] | index("needs-info") | not) | .number] | sort | .[]'); do
  [ -d "$ROOT/.worktrees/issue-$n" ] && continue
  git -C "$ROOT" show-ref --verify --quiet "refs/heads/agent/issue-$n" && continue
  ISSUE="$n"; break
done
if [ -z "$ISSUE" ]; then
  log "ready-for-agent queue empty — no-op exit"; exit 0
fi
log "picked issue #$ISSUE"

BR="agent/issue-$ISSUE"
WT="$ROOT/.worktrees/issue-$ISSUE"
TMP="$(mktemp -d)"

fail() { # label needs-info, record, clean up lane
  local reason="$1"
  log "FAIL #$ISSUE: $reason"
  gh issue edit "$ISSUE" -R "$REPO" --add-label needs-info >/dev/null || true
  runlog "**Implementation Lane 실패** ($TS)
- issue: #$ISSUE
- stage: $reason
- action: \`needs-info\` 라벨 추가, worktree 정리. 사람이 확인 후 \`ready-for-agent\` 재부여 시 재시도."
  git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  git -C "$ROOT" branch -D "$BR" 2>/dev/null || true
  rm -rf "$TMP"
  exit 1
}

# --- worktree ----------------------------------------------------------------
git -C "$ROOT" fetch origin main --quiet
git -C "$ROOT" worktree add "$WT" -b "$BR" origin/main --quiet
log "worktree $WT on $BR"

gh issue view "$ISSUE" -R "$REPO" --json title,body,comments \
  --jq '{title: .title, body: .body, comments: [.comments[].body]}' > "$TMP/issue.json"

# --- maker: codex exec implements --------------------------------------------
log "maker session start"
codex exec \
  -C "$WT" \
  -s workspace-write \
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

# --- independent gates (script does not trust the maker) ----------------------
log "verification gates"
( cd "$WT" && cargo fmt --all --check )                      || fail "gate: cargo fmt"
( cd "$WT" && cargo clippy --all-targets -- -D warnings )    || fail "gate: cargo clippy"
( cd "$WT" && cargo test )                                   || fail "gate: cargo test"

# --- checker: separate read-only codex session --------------------------------
log "checker session start"
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

# --- draft PR ------------------------------------------------------------------
log "opening draft PR"
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
rm -rf "$TMP"
log "done: #$ISSUE -> $PR_URL"

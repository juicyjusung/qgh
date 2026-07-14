#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 /absolute/or/relative/qgh PROFILE_ID [PROFILE_ID ...]" >&2
  exit 2
fi

qgh=$1
shift
profiles=("$@")

if [[ ${#profiles[@]} -lt 2 ]]; then
  echo "at least two disposable profiles are required to verify a profile-set update" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for the schedule manager gate" >&2
  exit 2
fi
expected_profiles_json=$(printf '%s\n' "${profiles[@]}" | jq -R . | jq -s .)

status_json=$("$qgh" schedule status --json)
if ! jq -e '
  .ok == true
  and .data.schedule_state == "not_installed"
  and .data.artifact_state == "missing"
' <<<"$status_json" >/dev/null; then
  echo "refusing to replace an existing or drifted qgh user schedule" >&2
  exit 2
fi

case "$(uname -s)" in
  Darwin)
    preflight_domain="gui/$(id -u)"
    if launchctl print "$preflight_domain/com.juicyjusung.qgh.schedule" >/dev/null 2>&1; then
      echo "refusing to replace an already loaded qgh LaunchAgent" >&2
      exit 2
    fi
    ;;
  Linux)
    if systemctl --user is-enabled --quiet qgh-schedule.timer \
      || systemctl --user is-active --quiet qgh-schedule.timer \
      || systemctl --user is-active --quiet qgh-schedule.service; then
      echo "refusing to replace an active qgh systemd user schedule" >&2
      exit 2
    fi
    ;;
esac

cleanup() {
  "$qgh" schedule stop --json >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_json=$("$qgh" schedule start "${profiles[0]}" --json)
jq -e '.ok and .data.action == "installed" and .data.installed' <<<"$start_json" >/dev/null

status_json=$("$qgh" schedule status --json)
jq -e '.ok and .data.schedule_state == "active" and .data.artifact_state == "ready"' <<<"$status_json" >/dev/null

updated_json=$("$qgh" schedule start "${profiles[@]}" --json)
jq -e --argjson expected_profiles "$expected_profiles_json" '
  .ok
  and .data.action == "updated"
  and .data.manager_checked
  and .data.profile_ids == $expected_profiles
' <<<"$updated_json" >/dev/null

unchanged_json=$("$qgh" schedule start "${profiles[@]}" --json)
jq -e '.ok and .data.action == "unchanged" and .data.manager_checked' <<<"$unchanged_json" >/dev/null

assert_at_most_one_coordinator() {
  local count
  count=$({ pgrep -u "$(id -u)" -f 'qgh schedule run' 2>/dev/null || true; } | wc -l | tr -d ' ')
  if [[ $count -gt 1 ]]; then
    echo "more than one qgh schedule coordinator is running" >&2
    return 1
  fi
}

wait_for_macos_successful_run() {
  local log=$1
  local before_bytes=$2
  local bytes
  for _ in $(seq 1 60); do
    assert_at_most_one_coordinator
    if [[ -f $log ]]; then
      bytes=$(wc -c <"$log" | tr -d ' ')
      if [[ $bytes -gt $before_bytes ]] && tail -c "+$((before_bytes + 1))" "$log" | jq -e '
        .ok
        and .data.operation == "run"
        and .data.pass_state == "completed"
      ' >/dev/null 2>&1; then
        return 0
      fi
    fi
    sleep 1
  done
  echo "timed out waiting for a successful launchd coordinator result" >&2
  return 1
}

case "$(uname -s)" in
  Darwin)
    uid=$(id -u)
    domain="gui/$uid"
    label="com.juicyjusung.qgh.schedule"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    launchctl print "$domain/$label" >/dev/null
    grep -q '<key>StartCalendarInterval</key>' "$plist"
    grep -q '<key>RunAtLoad</key>' "$plist"
    grep -q '<key>Umask</key>' "$plist"
    launchctl bootout "$domain" "$plist"
    stdout_log="${XDG_CACHE_HOME:-$HOME/.cache}/qgh/schedule/stdout.log"
    before_repair_bytes=0
    if [[ -f $stdout_log ]]; then
      before_repair_bytes=$(wc -c <"$stdout_log" | tr -d ' ')
    fi
    repaired_json=$("$qgh" schedule start "${profiles[@]}" --json)
    jq -e '.ok and .data.action == "reloaded"' <<<"$repaired_json" >/dev/null
    launchctl print "$domain/$label" >/dev/null
    wait_for_macos_successful_run "$stdout_log" "$before_repair_bytes"
    before_kick_bytes=$(wc -c <"$stdout_log" | tr -d ' ')
    launchctl kickstart -k "$domain/$label"
    wait_for_macos_successful_run "$stdout_log" "$before_kick_bytes"
    ;;
  Linux)
    systemctl --user is-enabled --quiet qgh-schedule.timer
    systemctl --user is-active --quiet qgh-schedule.timer
    systemctl --user cat qgh-schedule.timer | grep -q '^Persistent=true$'
    systemctl --user disable --now qgh-schedule.timer
    systemctl --user stop qgh-schedule.service
    repaired_json=$("$qgh" schedule start "${profiles[@]}" --json)
    jq -e '.ok and .data.action == "reloaded"' <<<"$repaired_json" >/dev/null
    systemctl --user is-enabled --quiet qgh-schedule.timer
    systemctl --user is-active --quiet qgh-schedule.timer
    for _ in $(seq 1 60); do
      if ! systemctl --user is-active --quiet qgh-schedule.service; then
        break
      fi
      assert_at_most_one_coordinator
      sleep 1
    done
    if systemctl --user is-active --quiet qgh-schedule.service; then
      echo "timed out waiting for the prior coordinator run to finish" >&2
      exit 1
    fi
    journal_cursor=$(journalctl --user -n 1 --show-cursor --no-pager -o cat \
      | sed -n 's/^-- cursor: //p' | tail -n 1)
    if [[ -z $journal_cursor ]]; then
      echo "could not capture the user journal cursor before activation" >&2
      exit 1
    fi
    systemctl --user start qgh-schedule.service
    [[ $(systemctl --user show qgh-schedule.service --property=Result --value) == "success" ]]
    run_output=$(journalctl --user -u qgh-schedule.service _COMM=qgh \
      --after-cursor="$journal_cursor" --no-pager -o cat)
    jq -e '
      .ok
      and .data.operation == "run"
      and .data.pass_state == "completed"
    ' <<<"$run_output" >/dev/null
    assert_at_most_one_coordinator
    ;;
  *)
    echo "unsupported platform for schedule manager gate" >&2
    exit 2
    ;;
esac

stop_json=$("$qgh" schedule stop --json)
jq -e '.ok and .data.action == "removed" and (.data.installed | not)' <<<"$stop_json" >/dev/null

case "$(uname -s)" in
  Darwin)
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      echo "LaunchAgent remained loaded after schedule stop" >&2
      exit 1
    fi
    ;;
  Linux)
    if systemctl --user is-enabled --quiet qgh-schedule.timer; then
      echo "systemd timer remained enabled after schedule stop" >&2
      exit 1
    fi
    if systemctl --user is-active --quiet qgh-schedule.service; then
      echo "systemd service remained active after schedule stop" >&2
      exit 1
    fi
    ;;
esac

status_json=$("$qgh" schedule status --json)
jq -e '.ok and .data.schedule_state == "not_installed" and .data.artifact_state == "missing"' <<<"$status_json" >/dev/null

second_stop_json=$("$qgh" schedule stop --json)
jq -e '.ok and .data.action == "unchanged"' <<<"$second_stop_json" >/dev/null

trap - EXIT
echo "schedule manager gate passed on $(uname -s)"

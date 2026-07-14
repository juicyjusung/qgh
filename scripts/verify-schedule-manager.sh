#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 /absolute/or/relative/qgh PROFILE_ID [PROFILE_ID ...]" >&2
  exit 2
fi

qgh=$1
shift
profiles=("$@")

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for the schedule manager gate" >&2
  exit 2
fi

status_json=$("$qgh" schedule status --json)
if [[ $(jq -r '.data.installed' <<<"$status_json") != "false" ]]; then
  echo "refusing to replace an existing qgh user schedule" >&2
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

start_json=$("$qgh" schedule start "${profiles[@]}" --json)
jq -e '.ok and .data.action == "installed" and .data.installed' <<<"$start_json" >/dev/null

status_json=$("$qgh" schedule status --json)
jq -e '.ok and .data.schedule_state == "active" and .data.artifact_state == "ready"' <<<"$status_json" >/dev/null

unchanged_json=$("$qgh" schedule start "${profiles[@]}" --json)
jq -e '.ok and .data.action == "unchanged" and .data.manager_checked' <<<"$unchanged_json" >/dev/null

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
    repaired_json=$("$qgh" schedule start "${profiles[@]}" --json)
    jq -e '.ok and .data.action == "reloaded"' <<<"$repaired_json" >/dev/null
    launchctl print "$domain/$label" >/dev/null
    launchctl kickstart -k "$domain/$label"
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
    systemctl --user start --no-block qgh-schedule.service
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

# Scheduled Sync

qgh schedules one bounded foreground coordinator pass. The OS manager owns
only wake-up and process lifecycle; `qgh schedule run` owns profile planning,
rate admission, fairness, and sync results.

## Foreground pass

```sh
qgh schedule run work personal
qgh schedule run work personal --json
```

Profile ids are positional, explicit, unique, and never inferred from
`QGH_PROFILE`, the current worktree, an organization, or all configured
profiles. A pass:

- plans every named profile from local status before remote work;
- skips fresh profiles and never-synced profiles without a GitHub request;
- serializes profiles sharing a GitHub host;
- permits one probe request when core rate-budget evidence is missing, partial,
  or stale; complete fresh core headers move that same run to a known budget,
  while missing/partial probe headers defer every follow-up request. A pass
  that starts unknown may continue the probed profile under the learned known
  gate, but does not start a second same-host profile;
- checks one shared host gate immediately before every scheduled GitHub HTTP
  send, including pagination, comments, parent lookups, and permission
  confirmations;
- with a known budget, starts at most
  `remaining - ceil(limit * 20%)` additional requests and starts no additional
  qgh request after its latest observation reaches that reserve;
- attempts each profile once and starts at most eight remote syncs;
- persists a private atomic host cursor for round-robin fairness;
- continues other profiles after a profile-local failure.

A never-synced profile must be bootstrapped explicitly:

```sh
qgh sync --all --profile work --json
```

The coordinator never runs bootstrap, backfill, reconciliation, model install,
or model rebuild. The gate preserves already committed pages/cursors and defers
the interrupted profile plus same-host profiles without marking a full sync
successful. This is a qgh-local admission guarantee, not an absolute GitHub
quota guarantee: another process or client can consume the same quota between
observations. If a request admitted from a known budget starts but yields no
usable response headers (for example, a transport failure or a final response
with missing/partial core headers), qgh retains a content-free, per-host
write-ahead budget-uncertainty guard. Its deadline uses the latest applicable
fresh core reset or active selected-profile backoff and is capped at 24 hours;
when neither deadline is usable, the guard uses the five-minute observation
TTL. Later schedule passes report `host_cooldown`, including an explicit subset
containing only another same-host profile, and make no request during that
guard. qgh does not rewrite the last header observation with a guessed
remaining value. The private atomic guard is stored separately from the
round-robin cursor and is removed after expiry.

## Install the user schedule

All scheduled profiles must use `token_source.type = "github_cli"`. qgh does
not copy an environment token into a manager artifact.

```sh
gh auth status --hostname github.com
qgh schedule start work personal
qgh schedule status
```

v1 uses a fixed one-hour interval with a bounded 15-minute offset. Repeating
the same start is idempotent; changing the profile list atomically updates the
registration. qgh records the invoked absolute executable path without
canonicalizing a package-manager symlink, so a stable Homebrew path can keep
following upgrades. `start` and `stop` share one private owner record and lease
under `~/.local/state/qgh/schedule-owner/uid-<uid>/`. This location deliberately
does not follow `XDG_DATA_HOME` or `XDG_CONFIG_HOME`: the LaunchAgent label or
systemd timer name is one fixed identity per OS user, so ownership must have the
same scope. The v2 owner record captures the OS identity, only the platform's
artifact-root XDG override, and exact managed artifact paths. Changing an XDG
override therefore moves the artifacts in one transaction, while `status` and
`stop` still inspect or remove
the previously recorded paths. Local paths remain absent from command output.

An overlapping mutation returns retryable `schedule.busy`. Repeating `start`
also checks the user manager and reloads an externally disabled job without
changing the saved profile set. A fixed manager target or artifact without a
provable owner fails closed with `schedule.ownership_ambiguous`. A legacy v1
registration is accepted only when its private artifact hash uniquely proves
either the current or default XDG location; the next successful update publishes
v2. Ambiguous legacy state is never changed automatically.

### macOS

qgh installs one user LaunchAgent at
`~/Library/LaunchAgents/com.juicyjusung.qgh.schedule.plist`. It uses direct
`ProgramArguments`, `StartCalendarInterval`, `RunAtLoad`, and a deterministic
minute offset. Apple documents `launchd` as the preferred timed-job mechanism
and calendar intervals as coalescing missed triggers after wake. The agent's
stdout/stderr logs are private files under the qgh cache schedule directory;
the LaunchAgent also sets `Umask=077` so recreated logs remain private.
`status` requires both log paths to be regular private files but never hashes
their mutable contents. `start` recreates a missing log or restores private
permissions; symlinks and other non-regular runtime paths fail closed.

### Linux

qgh installs `qgh-schedule.service` and `qgh-schedule.timer` under the XDG
systemd user unit directory. The service is `Type=oneshot`; the timer uses
`OnCalendar=hourly`, `Persistent=true`, and a fixed randomized delay up to 15
minutes. systemd documents `Persistent=true` as catching up a missed calendar
trigger and `RandomizedDelaySec` as spreading timer load. A running oneshot is
not overlapped by another activation.

The user systemd manager must be available. Whether it remains active without
an interactive login is a host policy (`loginctl enable-linger`); qgh does not
change that policy and does not install a system service or cron fallback.

## Inspect, stop, and recover

```sh
qgh schedule status --json
qgh schedule stop
```

`status` reads registration, artifact hash, and permissions only. It does not
contact GitHub or launchctl/systemctl. `stop` disables the user job and removes
qgh-managed artifacts; on Linux it disables the timer before stopping any
running coordinator service. Repeating stop is successful and unchanged after
confirming that the fixed manager target is absent. A manager failure during
start/update/stop restores both old and new artifact locations, the prior owner
record, and the prior active/inactive manager state best-effort, then returns a
structured `schedule.*` error.

If `status` reports `drifted`, inspect the user-manager diagnostics and rerun
`schedule start` with the intended explicit profile list. On Linux use
`systemctl --user status qgh-schedule.timer` and
`journalctl --user -u qgh-schedule.service`. On macOS inspect the private qgh
schedule logs and `launchctl print gui/$(id -u)/com.juicyjusung.qgh.schedule`.

## Platform release gate

CI runs the lifecycle adapter and foreground coordinator contract suites on
both Ubuntu and macOS. Before a release, a real host for each platform must
also exercise `start -> status -> update -> suspend/offline catch-up -> stop`,
confirm one coordinator process at a time, and verify that a second `stop` is
unchanged. The Linux host must use an active systemd user manager; the macOS
host must use the logged-in user's `gui/<uid>` launchd domain. This live gate
must use disposable profiles and must not replace a user's existing schedule.
The manual `schedule-manager-gate` workflow targets dedicated self-hosted
macOS and Linux users and runs `scripts/verify-schedule-manager.sh`. The script
refuses an existing qgh schedule, exercises actual manager install, active
state, a two-profile set update, external disable/reload recovery, immediate
successful activation, the single-coordinator invariant, active stop, artifact
removal, and idempotent uninstall. Activation evidence must contain a completed
coordinator JSON pass, not merely a successful manager command. The workflow
requires the operator to record
the physical sleep/offline-resume observation and host/run reference; it does
not simulate or infer that observation. The loaded artifacts enforce the
documented `StartCalendarInterval`/`RunAtLoad` or `Persistent=true` catch-up
contract.

Primary references:

- [Apple: Scheduling Timed Jobs](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/ScheduledJobs.html)
- [Apple: Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
- [systemd.timer(5)](https://man7.org/linux/man-pages/man5/systemd.timer.5.html)

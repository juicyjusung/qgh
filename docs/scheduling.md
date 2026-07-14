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
- permits one host attempt when rate-budget evidence is missing, partial, or stale;
- preserves 20% of a usable observed limit;
- attempts each profile once and starts at most eight remote syncs;
- persists a private atomic host cursor for round-robin fairness;
- continues other profiles after a profile-local failure.

A never-synced profile must be bootstrapped explicitly:

```sh
qgh sync --all --profile work --json
```

The coordinator never runs bootstrap, backfill, reconciliation, model install,
or model rebuild. Rate admission is best-effort because one profile sync may
consume multiple requests and another client can consume the same quota.

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
following upgrades. `start` and `stop` share one stable local lifecycle lease;
an overlapping mutation returns retryable `schedule.busy`. Repeating `start`
also checks the user manager and reloads an externally disabled job without
changing the saved profile set.

### macOS

qgh installs one user LaunchAgent at
`~/Library/LaunchAgents/com.juicyjusung.qgh.schedule.plist`. It uses direct
`ProgramArguments`, `StartCalendarInterval`, `RunAtLoad`, and a deterministic
minute offset. Apple documents `launchd` as the preferred timed-job mechanism
and calendar intervals as coalescing missed triggers after wake. The agent's
stdout/stderr logs are private files under the qgh cache schedule directory;
the LaunchAgent also sets `Umask=077` so recreated logs remain private.

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
running coordinator service. Repeating stop is successful and unchanged. A manager
failure during start/update/stop restores the prior local artifacts and
registration best-effort and returns a structured `schedule.*` error.

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
state, external disable/reload recovery, immediate activation, active stop,
artifact removal, and idempotent uninstall. The operator still records the
physical sleep/offline-resume observation; the loaded artifacts enforce the
documented `StartCalendarInterval`/`RunAtLoad` or `Persistent=true` catch-up
contract.

Primary references:

- [Apple: Scheduling Timed Jobs](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/ScheduledJobs.html)
- [Apple: Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
- [systemd.timer(5)](https://man7.org/linux/man-pages/man5/systemd.timer.5.html)

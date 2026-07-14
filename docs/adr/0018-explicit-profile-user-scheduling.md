# Explicit-Profile User Scheduling

Status: accepted 2026-07-14 for issues #99, #100, and #101.

## Context

Many local profiles need periodic freshness without turning qgh into a shared
daemon or hiding expensive maintenance. The OSes already provide user-scoped
job managers with wake/catch-up and no-overlap semantics.

## Decision

`qgh schedule run <PROFILE_ID>...` is the only scheduling execution engine. It
requires an explicit profile list and runs one bounded pass under the REST
sync policy in ADR 0010. It does not discover profiles or execute bootstrap,
backfill, reconciliation, or model work.

`qgh schedule start/status/stop` owns one per-user registration:

- macOS: one LaunchAgent with direct `ProgramArguments`, hourly
  `StartCalendarInterval`, `RunAtLoad`, and a deterministic 0–14 minute offset;
- Linux: one `Type=oneshot` systemd user service plus an hourly timer with
  `Persistent=true`, `RandomizedDelaySec=15m`, and `FixedRandomDelay=true`;
- no cron, system daemon, or other fallback is installed.

The manager artifact invokes the stable absolute path by which qgh was called;
it is not canonicalized through a package-manager symlink. Lifecycle state and
artifacts are strict, private, atomically published, content-free, and rolled
back on manager failure. The owner record uses
`~/.local/state/qgh/schedule-owner/uid-<uid>/` rather than an XDG-selected data
directory because the manager label/unit is itself one fixed identity per OS
user. Registration schema v2 records the OS owner, only the platform's
artifact-root XDG override, and exact managed artifact paths. Lifecycle
mutations across all XDG variants therefore own one stable advisory lease,
operate on the recorded prior paths, and publish new ownership last.
`start` checks and repairs manager activation, while `status` inspects local
artifacts only and does not ask the manager or GitHub for live state. Linux
stop/update disables the timer before stopping an active oneshot service.
Mutable macOS log contents are not hashed, but their paths must remain regular
private files; missing or non-private logs are repaired and symlinks fail
closed before qgh opens or changes them.

Legacy registration v1 migrates only when one private artifact bundle under
the current or default XDG location uniquely matches its recorded hash. An
active fixed manager target, orphaned artifact, or ambiguous legacy candidate
returns `schedule.ownership_ambiguous` without mutation. This fail-closed rule
prevents an XDG override from silently taking over or abandoning the same
manager identity.

Scheduled authentication supports `github_cli` profiles only. The artifact
records the minimal HOME/XDG/GH_CONFIG_DIR/PATH context required to find qgh,
gh, and the GitHub CLI credential store, but never a token. The PATH contains
only resolved absolute executable directories and fixed platform system
directories; relative PATH entries are ignored. An explicit `GH_CONFIG_DIR`
must also be absolute and normalized. Foreground `schedule run` continues to
honor an explicitly configured env token source.

## Consequences

The OS manager can coalesce missed runs, while qgh's host and profile leases
prevent overlap from becoming concurrent GitHub or SQLite work. Linux hosts
without an active user systemd manager must run the foreground command
manually or enable their own user-session policy. qgh deliberately does not
change lingering policy.

The local registration cannot prove that a user externally disabled the job;
`schedule status` reports qgh-owned artifact integrity, not remote or manager
liveness. User-manager diagnostics remain the authority for that distinction.

References:

- [Apple: Scheduling Timed Jobs](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/ScheduledJobs.html)
- [Apple: Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
- [systemd.timer](https://www.freedesktop.org/software/systemd/man/latest/systemd.timer.html)

# REST Sync Scheduler Contract

Status: accepted, amended 2026-07-14 by issues #97~#99.

qgh sync uses GitHub REST Issues and issue comments endpoints directly instead of a broad GitHub SDK abstraction. The scheduler keeps pagination, rate-limit headers, ETags, `pull_request` filtering, source identities, and reconciliation state explicit.

Issues are listed with `state=all`, `sort=updated`, `direction=asc`, `since`, and `per_page=100`. Pull requests returned by Issues endpoints are excluded by the `pull_request` key. Pagination follows `Link: rel="next"` until exhausted.

Sync uses effective concurrency 1, idempotent upserts, a 60-second updated-at overlap window, conditional requests where available, and bounded backoff. The existing `max_in_flight_requests` config remains strict at 1..16 for compatibility, but it is reported as a configured value and does not increase current effective concurrency.

Every profile writer operation (`sync` and `sync issue`) owns one stable advisory `sync.lock`. A second process returns retryable `sync.busy`; it never waits indefinitely or deletes the lock inode. Process exit and crash release the OS lease.

Every received GitHub response, including `304` and primary/secondary backoff responses, contributes a content-free best-effort Rate Budget Observation: host, sanitized resource name, limit, remaining, reset time, and observation time. Missing or malformed headers replace older optimistic state with a partial observation. `status` reads this local state without network access; only explicit `doctor` may query `/rate_limit`.

`qgh schedule run <PROFILE_ID>...` is a CLI-only foreground coordinator. It performs a complete local plan before remote work, groups profiles by explicit host, serializes each host, and persists a minimal round-robin cursor. A usable observation is at most five minutes old and has limit, remaining, and a future reset. The coordinator reserves 20% of the limit; unknown/partial/stale budget permits at most one host attempt for that pass. Each profile gets at most one attempt and the pass gets at most eight. Budget is reread after every attempt.

Never-synced profiles require an explicit `qgh sync --all --profile <id>` bootstrap. The coordinator never performs bootstrap, backfill, reconciliation, model work, org discovery, or implicit all-profile selection. Full reconciliation remains explicit via `qgh sync --reconcile full`; it is never hidden background work.

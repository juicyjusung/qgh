# REST Sync Scheduler Contract

qgh sync uses GitHub REST Issues and issue comments endpoints directly instead of a broad GitHub SDK abstraction. The scheduler keeps pagination, rate-limit headers, ETags, `pull_request` filtering, source identities, and reconciliation state explicit.

Issues are listed with `state=all`, `sort=updated`, `direction=asc`, `since`, and `per_page=100`. Pull requests returned by Issues endpoints are excluded by the `pull_request` key. Pagination follows `Link: rel="next"` until exhausted.

Sync uses low concurrency, idempotent upserts, a 60-second updated-at overlap window, conditional requests where available, and bounded backoff. Full reconciliation is explicit via `qgh sync --reconcile full`; it is never hidden background work.

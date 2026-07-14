# JSON Envelope v2

Status: accepted 2026-07-14.

## Context

The already-published, closed `sync` and `status` payloads gained scheduler and
rate-budget fields, while `schedule` added bounded per-host policy and
user-manager lifecycle payloads. Keeping `qgh.v1` would make old strict
consumers reject current output while both producers claimed the same version.

## Decision

CLI `--json` output and MCP structured content emit `schema_version: qgh.v2`.
The released envelope schema, CLI help, product contract, qgh skill reference,
and release evidence use the same version. qgh does not offer a compatibility
mode that labels new payloads as `qgh.v1`.

This is an envelope and command-payload contract change only. Independent
internal schemas such as `qgh.config.v1`, `qgh.db.v1`, and
`qgh.schedule-registration.v2` keep their own version lifecycles; the schedule
registration bump is an independent ownership migration, not an envelope alias.

## Consequences

Strict `qgh.v1` consumers must upgrade their envelope and command-payload
schemas. A consumer that does not recognize `qgh.v2` must fail closed instead
of guessing compatibility. The single version constant used by CLI envelopes
also feeds MCP output schemas so those two public surfaces cannot drift.

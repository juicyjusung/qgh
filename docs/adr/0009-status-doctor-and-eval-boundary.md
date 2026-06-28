# Status, Doctor, And Eval Boundary

`status` is a local-only snapshot of profile, store, index, sync, and reconciliation state. It must not perform network probes, model loads, or expensive validation.

`doctor` is a CLI-only explicit diagnostic command. It may check config/profile parsing, file permissions, SQLite/Tantivy consistency, GitHub auth and reachability, and rate-limit headers. It is not exposed as an MCP tool.

`eval` is an MVP release/test harness, not a user-facing CLI or MCP command. This keeps the agent-facing surface small while preserving quality gates for implementation.

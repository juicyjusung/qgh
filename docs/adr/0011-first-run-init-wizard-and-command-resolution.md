# First-Run Init Wizard and Command Resolution

Top-level `qgh init` is the first-run bootstrap command. It reads the current
git worktree `origin` remote, previews GitHub.com or GHES host defaults, default
profile id `work`, `github_cli` token source, XDG config/profile DB paths, and
the default-on `.qgh.toml` repo policy path before writing. Enter or `Y` applies
that preset, `n` enters customize prompts, and EOF cancels before writes. `qgh
init --yes` and `qgh init -y` apply the same preset without preview or prompts.
`qgh init repo` remains repo-policy-only for projects that want tracked policy
without personal profile mutation.

Operational CLI commands define the Command Resolution pipeline, and MCP tools
mirror that same pipeline as a thin adapter:
explicit input, then current worktree repo policy, then current worktree Git
`origin` remote. Help and version output are parser-only surfaces and do not run
resolution.

When current repo Effective Scope exists, `sync` defaults to that repo instead
of every repo in the profile. Profile-wide sync requires explicit `sync --all`
or another explicit profile-wide command form. This prevents a repo-local
command from silently broadening into unrelated private repos in the same
profile.

`query` and `search` default to the Effective Scope when available. Explicit
repo arguments may override that scope only inside the resolved profile
allowlist. `status` and `doctor` report the same resolved profile and repo scope;
`status` remains local-only while `doctor` may run explicit probes.

`get --profile-id` is the highest-priority round-trip path from query results.
Without `--profile-id`, `get` uses shared Command Resolution and refuses to
return a source outside the current Effective Scope.

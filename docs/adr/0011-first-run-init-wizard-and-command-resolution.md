# First-Run Init Wizard and Command Resolution

Top-level `qgh init` is the first-run bootstrap command. It reads the current
git worktree `origin` remote and previews GitHub.com or GHES host defaults, a
profile id suggested from the current config (`github` for a fresh GitHub.com
setup, otherwise a collision-safe host-derived id), `github_cli` token source,
XDG config/profile DB paths, and the default-on `.qgh.toml` repo policy path.
Enter or `Y` fixes the displayed profile id and applies that preset; `n` enters
customize prompts, and EOF cancels before writes.

Promptless `qgh init --yes` and `qgh init -y` fix explicit `--profile`, then
`QGH_PROFILE`, as the selected id. Without either, they defer profile selection
until after acquiring the profile-config mutation lease and reading its latest
snapshot. Automatic selection reuses exactly one repo-and-host match, otherwise
exactly one same-host profile, otherwise creates a collision-safe host-derived
id. Multiple candidates at either matching tier fail with
`config.ambiguous_profile` and require `--profile`; qgh never chooses the
first map entry. Reusing an existing profile preserves its configured token
source and endpoints unless an endpoint was explicitly overridden. Interactive
preview/customization displays the stored token source and does not offer a
token-source prompt for an existing profile; an explicitly conflicting token
source fails before mutation. Host identity is normalized to lowercase at Git
remote and init-input boundaries.
Origin endpoints are reused only when the normalized origin host matches the
selected host; otherwise qgh derives endpoints from the selected host.
Interactive customization uses an existing selected profile's endpoints as
defaults and treats only a supplied flag or changed prompt value as an
override. The selected id is the single source for config mutation and success
output. Explicit environment selection reports `meta.profile_source: env`; all
other init selection remains `cli` to preserve the released closed `qgh.v2`
provenance enum. `meta.repo_source` independently reports `cli` or `git_remote`
according to the actual repo input. The resulting policy path belongs in init
`data`; `meta.repo_policy_path` remains `null` because no policy supplied the
input scope.

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

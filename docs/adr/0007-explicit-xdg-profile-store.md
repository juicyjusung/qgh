# XDG Profile Store and Repo Policy Scope

qgh separates config, data, and cache through XDG paths: strict TOML config at `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`, profile data under `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`, and cache under `${XDG_CACHE_HOME:-~/.cache}/qgh`.

Profiles remain the security boundary: GitHub host, token source reference, repo allowlist, and profile store are defined only in the XDG profile config. MVP does not provide arbitrary DB path override; the profile data path is derived from XDG and profile id.

qgh may read a tracked repo policy from the current git worktree root to determine repo scope and safe default filters. The repo policy is not a credential source, cannot define a token source, and cannot widen access beyond a profile repo allowlist. Worktrees resolve their own root policy; qgh does not follow another checkout's policy file.

Current MVP commands still use explicit `--profile`; repo policy is applied only after the profile security boundary is fixed. This keeps common repo/worktree queries scoped to the current repository without letting cwd, token environment, or a project file select the private corpus.

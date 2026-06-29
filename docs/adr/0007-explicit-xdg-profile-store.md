# XDG Profile Store and Repo Policy Resolution

qgh separates config, data, and cache through XDG paths: strict TOML config at `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`, profile data under `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`, and cache under `${XDG_CACHE_HOME:-~/.cache}/qgh`.

Profiles remain the security boundary: GitHub host, token source reference, repo allowlist, and profile store are defined only in the XDG profile config. MVP does not provide arbitrary DB path override; the profile data path is derived from XDG and profile id.

qgh may read a tracked repo policy from the current git worktree root to determine repo scope and safe default filters. The repo policy is not a credential source, cannot define a token source, and cannot widen access beyond a profile repo allowlist. Worktrees resolve their own root policy; qgh does not follow another checkout's policy file.

Profile resolution uses explicit inputs first: CLI `--profile` overrides environment, and environment overrides automatic resolution. If no profile is explicit, qgh may auto-select a profile only when an effective repo scope exists and exactly one configured profile allowlists that repo scope. Zero matches fail with `config.no_matching_profile`; multiple matches fail with `config.ambiguous_profile` and require `--profile`.

This keeps common single-profile repo workflows concise while preserving explicit failure for ambiguous private-corpus access. A repo policy may help define repo scope, but it still cannot select credentials or widen the profile allowlist.

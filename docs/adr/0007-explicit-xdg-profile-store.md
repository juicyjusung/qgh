# Explicit XDG Profile Store

qgh separates config, data, and cache through XDG paths: strict TOML config at `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`, profile data under `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`, and cache under `${XDG_CACHE_HOME:-~/.cache}/qgh`.

Every CLI and MCP command requires an explicit `--profile`. qgh does not infer corpus from cwd, repo remotes, token environment, or a hidden default profile. MVP does not provide arbitrary DB path override; the profile data path is derived from XDG and profile id. This prevents agents from accidentally searching the wrong private corpus.

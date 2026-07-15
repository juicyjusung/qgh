# XDG Profile Store and Repo Policy Resolution

qgh separates config, data, and cache through XDG paths: strict TOML config at `${XDG_CONFIG_HOME:-~/.config}/qgh/config.toml`, profile data under `${XDG_DATA_HOME:-~/.local/share}/qgh/profiles/<profile-id>`, and cache under `${XDG_CACHE_HOME:-~/.cache}/qgh`.

Profiles remain the security boundary: GitHub host, token source reference, repo allowlist, and profile store are defined only in the XDG profile config. MVP does not provide arbitrary DB path override; the profile data path is derived from XDG and profile id.

Read-only config loading retains compatibility with an existing symbolic link at
the final `config.toml` entry. Config mutation is stricter: `qgh init` resolves
the existing parent directory, then fails closed if the final config entry is a
symbolic link or is not a regular file. This permits platform-managed symbolic
links in parent XDG or HOME paths without allowing a config write to follow a
redirected final entry.

Profile-config mutation is a single-writer transaction scoped to
`config.toml`. qgh opens a stable, private `config.toml.lock` without following
a final lock symlink on supported Unix targets, acquires a bounded exclusive
lease, and only then reads and validates the current config. Contention that
lasts five seconds returns retryable `config.busy`; qgh never deletes the
stable lock file to recover a lease because the OS releases the lease when the
writer exits.

After validating the complete candidate config, qgh writes a same-directory
`0600` staging file, synchronizes it, atomically renames it over `config.toml`,
and synchronizes the canonical config-directory ancestry through the filesystem
root on macOS and Linux. The conservative ancestor barrier lets a later
successful init confirm directory entries left uncertain by an interrupted or
failed first init. A failure before the rename preserves the previous config
and removes qgh's staging file. A directory-sync failure after the rename
reports retryable `storage.failure` with
`publication_state = "visible_durability_unconfirmed"`: the complete new file
is visible, but crash durability was not confirmed. The XDG config directory
and stable lock remain private (`0700` and `0600` on Unix). An uncatchable
process termination may leave a private staging file; qgh never treats that
file as config or publishes it implicitly.

This transaction does not claim cross-file atomicity with a worktree
`.qgh.toml`. Top-level `qgh init` validates the planned repo-policy action
before mutating the profile config, then publishes each file independently; a
later repo-policy filesystem failure is reported for explicit recovery.

The derived Profile Store database entry must be a regular file. qgh does not
follow a symbolic link at the final `qgh.sqlite3` path for either reads or
writes; it fails closed instead. Parent XDG or HOME paths may resolve through
platform-managed symbolic links, so qgh canonicalizes the existing parent and
applies no-follow semantics to the database entry itself.

qgh may read a tracked repo policy from the current git worktree root to determine repo scope and safe default filters. The repo policy is not a credential source, cannot define a token source, and cannot widen access beyond a profile repo allowlist. Worktrees resolve their own root policy; qgh does not follow another checkout's policy file.

Profile resolution uses explicit inputs first: CLI `--profile` overrides environment, and environment overrides automatic resolution. If no profile is explicit, qgh may auto-select a profile only when an effective repo scope exists and exactly one configured profile allowlists that repo scope. Effective scope may come from repo policy or from the current worktree Git `origin` remote as described in ADR-0011. Zero matches fail with `config.no_matching_profile`; multiple matches fail with `config.ambiguous_profile` and require `--profile`.

This keeps common single-profile repo workflows concise while preserving explicit failure for ambiguous private-corpus access. A repo policy may help define repo scope, but it still cannot select credentials or widen the profile allowlist.

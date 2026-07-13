# qgh

qgh is a local-first, read-only CLI and MCP server for retrieving GitHub
Issues and issue comments. It synchronizes an explicit repository scope to
your machine so humans and agents can search without spending GitHub search
quota, then keeps the evidence workflow explicit: `query -> get -> cite`.

Search results are source candidates, not answers. Always open a result with
`get` and cite the authoritative body and `canonical_url`, never the search
snippet alone.

## Install

qgh publishes Homebrew binaries for macOS Apple Silicon and Linux x86_64 with
glibc 2.38 or newer:

```sh
brew install juicyjusung/tap/qgh
qgh --version
```

macOS Intel, Windows, and Linux ARM64 binaries are not currently published.
The formula installs one `qgh` executable; optional local model weights are
installed separately.

## Prerequisites

Run qgh from a repository whose GitHub or GitHub Enterprise origin you want to
search. Repository selection remains explicit; qgh does not discover an
organization-wide scope.

The default token source uses an authenticated GitHub CLI session:

```sh
gh auth login --hostname github.com
gh auth status --hostname github.com
```

For GitHub Enterprise, replace `github.com` with that host. To reference an
environment variable instead, initialize with
`qgh init --token-source env --token-env GITHUB_TOKEN`. qgh stores the token
source reference, never a literal token in project config.

## Quick Start for Humans

Review the detected host, repository, profile, and local paths before qgh
writes configuration:

```sh
qgh init
```

New release configs select the local Qwen embedding preset. Its weights are
not bundled or downloaded automatically. Install them when you want hybrid
search; skip this command to keep using the safe BM25 fallback:

```sh
qgh model install qwen3-embedding-0.6b
```

Existing profiles keep their configured embedding model and are never
silently migrated; follow [Local Qwen models](docs/local-qwen-models.md) when
you intentionally change one.

Then synchronize and search:

```sh
qgh sync
qgh query "why was this behavior changed?"
```

Human output ends with a state-aware `next:` instruction. Follow it instead
of guessing whether to sync, backfill, retry, install a model, or open a
source.

For non-interactive agent or automation setup, accept inferred defaults with:

```sh
qgh init -y
```

## Sync and Historical Coverage

Normal sync contacts the configured GitHub host, applies live issue/comment
changes, rebuilds BM25, and updates missing or changed embeddings when a local
embedding model is configured and installed:

```sh
qgh sync
qgh status
```

`coverage: partial` is not a failed sync. Current data is searchable, but open
coverage, historical coverage, or both are incomplete; open or older closed
issues may still be absent. qgh chooses the next phase in order:

```sh
qgh sync --all --profile PROFILE
qgh sync --backfill --all --profile PROFILE
```

The first command completes open coverage across the profile. After that, each
backfill command runs one bounded historical pass. Copy the exact `next:`
command from human output; agents should execute
`coverage.next_action.json_command`. Repeat only while qgh recommends another
pass. A repo-scoped sync cannot claim completion for a multi-repo profile.
`qgh embed --force` is an advanced repair or full recompute command, not part
of the normal sync workflow.

## Query, Get, Cite

1. Find source candidates:

   ```sh
   qgh query "image generation failed"
   ```

2. Copy the exact `get:` command printed for the selected result:

   ```sh
   qgh get '<source_id>' --profile-id '<profile_id>'
   ```

   Keeping the emitted `profile_id` makes the round trip stable across working
   directories and multi-profile setups. The CLI can open 1 to 20 source IDs
   in one `get` call.

3. Read the full source, verify its version and lifecycle metadata, and cite
   its `canonical_url`. A snippet is only a navigation aid.

## JSON for Agents and Scripts

Human output is designed for reading and may evolve. Add `--json` for the
versioned, machine-stable `qgh.v1` envelope:

```sh
qgh query "search terms" --json
qgh get '<source_id>' --profile-id '<profile_id>' --json
qgh status --json
```

Agents should copy both fields from `data.results[].get_args`. When qgh emits
a recommended JSON action, execute its `json_command` so `--json` is
preserved. CLI, config, and MCP schemas are strict: unknown fields and invalid
enum values fail instead of silently falling back. See the
[CLI JSON contract](docs/cli-json-contract.md) and released schemas under
[`docs/schemas/`](docs/schemas/).

## MCP

Start the stdio server directly:

```sh
qgh mcp
# Or pin a profile when repository context is unavailable:
qgh --profile work mcp
```

Or configure an MCP client with the profile that owns the local snapshot:

```json
{
  "mcpServers": {
    "qgh": {
      "command": "qgh",
      "args": ["--profile", "work", "mcp"]
    }
  }
}
```

MCP v1 exposes only the read-only `query`, `get`, and `status` tools. It does
not expose `init`, `sync`, model management, `doctor`, lifecycle verification,
or GitHub write tools. An operator prepares and refreshes the local snapshot
with the CLI. MCP `get` opens one source per call; CLI `get` also supports
bounded batches.

## Retrieval Modes

| Mode | Local model | Behavior |
| --- | --- | --- |
| BM25 | None | Complete model-free `query -> get -> cite` workflow |
| Hybrid | Qwen3-Embedding-0.6B | Keeps the protected BM25 head and adds semantic candidates after explicit model installation and sync |
| Optional reranking | Qwen3-Reranker-0.6B | Reorders at most the first 10 retrieved candidates when `--rerank` is requested; it cannot add a missing source |

Install and configure only the capability you need. Reranking is experimental,
separately configured, and off by default. Model, device, fallback, and
configuration details are in [Local Qwen models](docs/local-qwen-models.md).

## Privacy and Network Boundaries

- `query`, default `get`, `status`, and all MCP tools use only the local
  snapshot.
- `sync` and `doctor` contact the configured GitHub host. `doctor` also loads
  the configured local model runtime for an explicit health probe.
- `model install` contacts Hugging Face for pinned public weights but sends no
  repository content, metadata, chunks, embeddings, or queries.
- `get --verify-lifecycle` explicitly contacts GitHub and may purge confirmed
  unavailable qgh-managed local data.
- Embedding and reranking are local; qgh has no hosted retrieval provider and
  performs no GitHub write-back.

The local database, indexes, snippets, embeddings, logs, and cache can reflect
private repository content and must be treated as sensitive derivative data.
See [Privacy and local data](docs/privacy.md).

The current product scope is GitHub Issues and issue comments from explicit
repositories. It does not index code, pull requests, Discussions, Projects, or
Wiki content.

## Diagnose Problems

Start with the least invasive check:

```sh
qgh status       # local-only; no GitHub request or model load
qgh doctor       # explicit GitHub connectivity and model-runtime probe
```

| Symptom | What to do |
| --- | --- |
| GitHub authentication is unavailable | Run `gh auth status --hostname <host>` or verify the configured environment token source. |
| `coverage: partial` | Search is available but not exhaustive; follow the printed `next:` action for open sync or historical backfill. |
| Embedding model is missing | Install the configured preset explicitly; BM25 remains available meanwhile. |
| Snapshot is stale | Run the exact `qgh sync` action printed by query or status. |
| First hybrid sync is slow | Let the foreground progress and ETA finish; later syncs reuse unchanged vectors. |

Errors are structured and documented in [Error codes](docs/error-codes.md).
Use `qgh help` or `qgh <command> --help` for the current command surface.

## Upgrade and Release Integrity

```sh
brew update
brew upgrade juicyjusung/tap/qgh
qgh --version
```

Release artifacts are built by `cargo-dist` from `vX.Y.Z` tags, published to
[GitHub Releases](https://github.com/juicyjusung/qgh/releases), and delivered
through [`juicyjusung/homebrew-tap`](https://github.com/juicyjusung/homebrew-tap).
Maintainer verification and artifact-attestation steps are documented in the
[release checklist](docs/release-checklist.md).

## Contributing

Use [GitHub Issues](https://github.com/juicyjusung/qgh/issues) for bugs and
proposals. Keep changes within qgh's local-first, read-only product boundaries
and add focused regression coverage. Before opening a pull request, follow the
verification commands and release-contract checks in the
[release checklist](docs/release-checklist.md).

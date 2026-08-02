# Graft

Native repository context graph for coding agents. Graft indexes source into one
SQLite store outside the repository, refreshes changed files incrementally, and
answers structural and ranked retrieval queries without Node, npm, `npx`, a
daemon, telemetry, or model API calls.

## Install from `wallentx/Graft`, branch `dev`

Termux:

```sh
pkg install git rust clang
cargo install --git https://github.com/wallentx/Graft.git --branch dev --locked graft
```

Other platforms with Rust installed:

```sh
cargo install --git https://github.com/wallentx/Graft.git --branch dev --locked graft
```

`dev` is mutable. Replace `--branch dev` with `--rev <full-commit-sha>` for a
reproducible source install. Cargo builds locally for the current target, so the
installed executable uses Termux's Android/Bionic ABI when run in Termux.

To install this checkout instead:

```sh
cargo install --path rust --locked --force
```

The binary normally lands in `~/.cargo/bin`. Add that directory to `PATH` if
Cargo reports a successful install but `graft` is not found.

## Start

```sh
cd /path/to/repository
graft build
graft map
graft ask "where is request validation handled?" --source
graft callers validate_request --depth 2
graft grep 'TODO|FIXME'
graft check
```

Graft supports TypeScript, TSX, JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`),
Python, Go, and Rust. It honors gitignore rules and skips TypeScript declaration
files. Every read command checks live source hashes first and incrementally
refreshes stale rows. Use `--no-refresh` or `GRAFT_NO_REFRESH=1` for a read-only
snapshot.

The shared store defaults to `$XDG_DATA_HOME/graft/graft.db`, then
`~/.local/share/graft/graft.db`. Indexed repositories are never modified.

## Commands

| Command | Purpose |
|---|---|
| `build [path]` | Incrementally index one repository or every child in a multi-repo folder |
| `ask <query> [path]` | Ranked lexical/structural retrieval with optional source excerpts |
| `grep <regex>` | Exhaustive source search grouped by enclosing symbol |
| `callers <symbol>` | Incoming or outgoing call/import traversal |
| `skeleton <file>` | Definitions, signatures, and source spans in one file |
| `map [path]` | Directory clusters, hubs, and hotspots |
| `check [path]` | Non-mutating CI freshness check |
| `status [path]` | Store counts, schema, age, extractor, and source drift |
| `mcp [path]` | Bounded newline-delimited MCP JSON-RPC server |
| `init --host <id>` | User-level MCP registration using this executable's absolute path |
| `viz [path]` | Loopback HTML viewer, or `--output map.html` |
| `export <destination>` | Deterministic Markdown cards or `--json` export |
| `cache prune|doctor|recover|reset` | Store maintenance and recoverable repair |
| `version` | Build, executable, schema, extractor, and store identity |
| `upgrade` | Safe package-manager/source update guidance; never self-modifies |
| `completions <shell>` | Generate Bash, Zsh, Fish, Elvish, or PowerShell completions |

Run `graft <command> --help` for flags. Read commands support stable JSON where
applicable. Path scopes are segment-aware with `--in`; workspace results include
`scope` labels. Ranked workspace results interleave child repositories so a large
child cannot monopolize top-N.

Exit codes:

| Code | Meaning |
|---:|---|
| `0` | Command completed; an empty search is valid output |
| `1` | Stale/failed check or internal error |
| `2` | Unindexed repository, missing/ambiguous requested node, or bad CLI input |

## Agent/MCP setup

Preview user-level writes first:

```sh
graft init --host codex --dry-run
graft init --host claude --host cursor --dry-run --json
```

Apply selected registrations by omitting `--dry-run`. Supported IDs are
`claude`, `codex`, `cursor`, `gemini`, `antigravity`, `opencode`, and `copilot`.
Registration merges a `graft` MCP entry into the host's user configuration and
uses the installed executable's absolute path. No machine-specific path is
written into the indexed repository. No background edit hooks are installed.

The MCP server exposes `find`, `grep`, `callers`, `skeleton`, `map`, `status`,
and `freshness`. It refuses stale or unindexed data instead of silently answering
from an incomplete graph. Requests and responses are capped at 1 MiB.

## Workspace behavior

A non-repository directory with at least two immediate Git repository children
is a workspace. `build`, `ask`, `grep`, `callers`, `skeleton`, `map`, `check`,
and MCP federate those child graphs. Each worktree has its own repository row;
`git_common_dir` is recorded for diagnosis but linked worktrees do not share
source rows because their checked-out contents can differ.

## Viewer and exports

`graft viz` now uses a self-contained Rust-rendered page. It binds only to
loopback unless `--allow-remote` is explicit.

`graft export out/` writes deterministic Markdown cards plus `INDEX.md`.
`graft export graph.json --json` writes one JSON document. Existing destinations
require `--force`.

## Deliberate rewrite decisions

- No `--deep` network/LLM enrichment in the native CLI. Search uses deterministic
  symbol metadata, bounded definition bodies, IDF weighting, coupling boosts,
  strength gating, and test-path de-ranking.
- Call graph edges are `contains`, `imports`, and resolved `calls`. Unknown member
  receiver types stay unresolved instead of falling back to same-name methods.
  Inheritance/reference edges are not used for `callers` and are intentionally
  outside the native blast-radius contract.
- No self-updater. `graft upgrade` never downloads or executes code.
- No repository-local agent hooks or host files. Freshness is checked on query;
  host registration belongs under the user's host configuration.
- Non-color deterministic output is the default. Unicode arrows appear only in
  human display; JSON is ASCII-safe data.

## Binary releases

Release archives contain the stripped binary, README, and license. Each archive
has a SHA-256 sidecar:

```sh
./scripts/package-release.sh
sha256sum -c release/graft-*.tar.gz.sha256
```

Do not execute downloaded installer scripts. Download an archive and checksum,
verify the checksum, inspect the extracted files, then copy `graft` into a
directory on `PATH`.

## Development

```sh
git clone --branch dev https://github.com/wallentx/Graft.git
cd Graft
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release --locked
```

## Security

See [SECURITY.md](SECURITY.md). Graft has no telemetry. Source indexing and all
native queries stay local. `init` refuses invalid existing JSON instead of
overwriting it; `cache recover` preserves the old database beside a clean store.

## License

MIT. See [LICENSE](LICENSE).

# Graft

Graft builds a local structural map of a source repository for people and coding
agents. It indexes definitions, imports, and resolvable calls into one SQLite
database outside the repository, then exposes that graph through a CLI and MCP.

It is a native Rust executable. There is no Node.js runtime, npm, `npx`, daemon,
telemetry, model API, or repository-local agent configuration.

## What it does

- Indexes TypeScript, TSX, JavaScript, Python, Go, and Rust.
- Finds relevant symbols for natural-language questions.
- Groups regex or literal matches under their enclosing symbol.
- Traverses incoming and outgoing call/import relationships.
- Produces file skeletons, repository maps, JSON, Markdown, and an HTML viewer.
- Serves the same tools to Codex, Claude Code, Cursor, Gemini CLI, Antigravity,
  OpenCode, and GitHub Copilot CLI through MCP.
- Incrementally reparses changed files and automatically refreshes normal CLI
  queries when source changes.

Graft does not modify an indexed repository. Its shared database defaults to
`$XDG_DATA_HOME/graft/graft.db`, falling back to
`~/.local/share/graft/graft.db`.

## Install

The current native work is installed from the explicit
[`wallentx/Graft`](https://github.com/wallentx/Graft) repository and `dev`
branch.

### Termux

Install native build prerequisites, clone the repository so it can be inspected,
and run the local installer:

```sh
pkg update
pkg install git rust clang
git clone --branch dev --single-branch https://github.com/wallentx/Graft.git
cd Graft
./install.sh
```

This builds a native Android/Bionic executable and installs it to
`~/.local/bin/graft`. It does not require `proot`, Node.js, npm, or `npx`.

If `~/.local/bin` is not already on `PATH`, add it to your shell configuration:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Linux and other Rust targets

Install Git, Rust, Cargo, and a C compiler with the operating system package
manager, then use the same inspect-first flow:

```sh
git clone --branch dev --single-branch https://github.com/wallentx/Graft.git
cd Graft
./install.sh
```

Choose another user-owned installation root when needed:

```sh
./install.sh --prefix "$HOME/.local/graft"
```

The installer never uses `sudo`. It builds only the checkout containing the
script, verifies that the installed binary starts, and then prints the exact
`PATH` entry and next steps.

### Direct Cargo install

If inspecting a clone first is not required:

```sh
cargo install \
  --git https://github.com/wallentx/Graft.git \
  --branch dev \
  --locked \
  --root "$HOME/.local" \
  graft
```

The `dev` branch is mutable. For a reproducible source install, replace
`--branch dev` with `--rev <full-commit-sha>`.

### Update or uninstall

The `upgrade` command explains the update policy but never downloads or executes
anything. Update an inspect-first installation from its checkout:

```sh
cd Graft
git pull --ff-only origin dev
./install.sh
```

Remove an installation made under the default root:

```sh
cargo uninstall --root "$HOME/.local" graft
```

## Configure providers

Run `init` with no options to open the interactive checkbox picker:

```sh
graft init
```

```text
Select providers (space toggles, enter confirms)
> [ ] Claude Code           ~/.claude.json
  [x] Codex                 ~/.codex/config.toml             detected
  [ ] Cursor                ~/.cursor/mcp.json
```

Detected providers start checked. Move with the arrow keys, toggle with space,
and apply the selection with enter. Graft merges an MCP entry named `graft` into
each selected user configuration. Existing unrelated entries are preserved,
invalid JSON is refused, and writes are private and atomic.

For scripts or headless machines, repeat `--provider` instead of opening the
picker:

```sh
graft init --provider codex --provider claude
graft init --provider cursor --dry-run
graft init --provider opencode --dry-run --json
```

Supported provider IDs and destinations:

| Provider ID | Provider | User configuration |
|---|---|---|
| `claude` | Claude Code | `~/.claude.json` |
| `codex` | Codex | `~/.codex/config.toml` |
| `cursor` | Cursor | `~/.cursor/mcp.json` |
| `gemini` | Gemini CLI | `~/.gemini/settings.json` |
| `antigravity` | Antigravity | `~/.gemini/config/mcp_config.json` |
| `opencode` | OpenCode | `~/.config/opencode/opencode.json` |
| `copilot` | GitHub Copilot CLI | `~/.copilot/mcp-config.json` |

The registered command is the installed executable's absolute path. No
machine-specific path, hook, prompt, or provider file is written into a source
repository.

## Quick start

```sh
cd /path/to/repository
graft build
graft map
graft ask "where is request validation handled?" --source
graft callers validate_request --depth 2
graft grep 'TODO|FIXME' --path .
graft check
```

`build` walks the Git worktree using gitignore-compatible rules and skips
TypeScript declaration files. Subsequent builds reuse unchanged extraction
payloads. Large cold builds use at most four parser workers; use `--jobs 1` or
`GRAFT_JOBS=1` on a memory-constrained device.

Normal CLI query commands refresh a stale index before answering. `check` and
`status` are observational: they report drift without repairing it. MCP refuses
stale or unindexed data rather than silently returning an incomplete graph. Use
global `--no-refresh` or `GRAFT_NO_REFRESH=1` when an intentional stored snapshot
is required.

## Commands

| Command | Purpose |
|---|---|
| `build [path]` | Incrementally index a repository or multi-repository workspace |
| `ask <query> [path]` | Ranked lexical and structural symbol retrieval |
| `grep <pattern> --path <path>` | Regex or literal source search grouped by symbol |
| `callers <symbol>` | Traverse incoming or outgoing calls and imports |
| `skeleton <file>` | List definitions, signatures, and spans in one file |
| `map [path]` | Show directory clusters, hubs, and hotspots |
| `viz [path]` | Serve a loopback viewer or write self-contained HTML |
| `export <destination>` | Write deterministic Markdown cards or JSON |
| `init` | Select and configure MCP providers |
| `mcp [path]` | Serve newline-delimited MCP JSON-RPC over stdin/stdout |
| `check [path]` | Fail when an index is missing, stale, or incompatible |
| `status [path]` | Report graph counts, age, schema, extractor, and source drift |
| `cache prune` | Remove records for repository paths that no longer exist |
| `cache doctor` | Run SQLite's full integrity check |
| `cache recover` | Preserve a damaged store and create a clean replacement |
| `cache reset [path]` | Remove one repository graph from the shared store |
| `version` | Show binary, build, schema, extractor, and store identity |
| `upgrade` | Show the safe update path without modifying the executable |
| `completions <shell>` | Generate shell completion code |

Run `graft <command> --help` for the complete flags. Read commands provide JSON
where applicable. `ask`, `grep`, and `callers` accept segment-aware `--in`
scopes.

Exit status `0` means the command completed, including an empty search. Status
`1` means a failed freshness/integrity check or another runtime error. Status `2`
means bad input, an unindexed repository, or a missing/ambiguous requested node.

## MCP tools

The MCP server exposes:

- `find`
- `grep`
- `callers`
- `skeleton`
- `map`
- `status`
- `freshness`

Requests and responses are capped at 1 MiB. Retrieval is local and
deterministic; Graft does not send source code or queries to a model service.

## Multi-repository workspaces

A non-repository directory containing at least two immediate Git repository
children is treated as a workspace. Build and query commands federate those
children and label results by repository. Ranked results are interleaved so one
large child cannot consume the entire result limit.

Linked Git worktrees keep separate graph rows because their checked-out source
can differ. Their shared Git directory is recorded only for diagnostics.

## Viewer and exports

`graft viz` starts a self-contained HTML viewer on loopback. Binding a non-loopback
address requires explicit `--allow-remote`:

```sh
graft viz
graft viz --output map.html
```

Exports are deterministic and require an explicit destination. Existing output
is not replaced without `--force`:

```sh
graft export docs/graft-map
graft export graph.json --json
```

## Graph boundaries

Graft records `contains`, `imports`, and calls it can resolve without guessing.
Unknown receiver types stay unresolved instead of creating false same-name
edges. Inheritance and arbitrary reference edges are not part of the native
`callers` contract.

Natural-language `ask` is deterministic local retrieval over symbol names,
signatures, bounded definition bodies, graph coupling, and test-path de-ranking.
There is no embedding service or `--deep` model enrichment.

## Security and privacy

- Indexed source and queries stay on the machine.
- The index lives outside the repository.
- Provider setup touches only explicitly selected user configuration files.
- Invalid existing JSON is never overwritten.
- There is no self-updater or remote installer execution.
- Release archives contain a stripped binary, README, license, and SHA-256
  sidecar; verify the checksum before copying a release binary onto `PATH`.

See [SECURITY.md](SECURITY.md) for reporting and policy details.

## Development

```sh
git clone --branch dev https://github.com/wallentx/Graft.git
cd Graft
cargo fmt --all --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

The deterministic performance harness records wall time, peak RSS, database
size, device/toolchain metadata, SQLite plans, and optional syscall profiles:

```sh
./scripts/benchmark.sh
```

See [bench/RESULTS.md](bench/RESULTS.md) for device-scoped measurements. Those
results are not universal performance claims.

## License

MIT. See [LICENSE](LICENSE).

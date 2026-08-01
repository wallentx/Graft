# TODO

Working notes for the fork. Numbers are measured against this repo unless stated
otherwise; the TypeScript build is the reference implementation being ported.

Status as of the last commit on `wallentx/rust`: 20 tests green, schema v3,
`build` / `grep` / `callers` / `skeleton` / `map` / `status` implemented for
TypeScript, Python and Go.

---

## Rust port — remaining commands

### `ask` — the ranked retrieval scorer
**Not started. The hard one.**

Everything else in the port is mechanical; this is where the reference
implementation's actual value lives. `src/ask/ask.ts` carries idf weighting, a
strength gate, pytest `test_*.py` / `conftest.py` de-ranking, and junk-token
gating that the TypeScript suite pins at both ~30-node and ~200-node scale.

Port it last and validate differentially: compare **ranked ID lists**, not
scores. Identical ordering on the same query is the bar; identical floats are
not achievable and not the point.

### MCP server
**Not started.** The gate is already wired — `not_indexed()` in `rust/src/main.rs`
is the single message every read path routes through, so the "this directory has
not been indexed by graft" behaviour just needs a JSON-RPC stdio loop in front of
it. Tools to expose mirror the CLI: find_code, find_all, trace_calls, file_api,
repo_map.

### `export`
**Not started.** Requested explicitly: regenerate markdown cards + `INDEX.md`
from the store, on demand, to a path the caller names. This is what replaces the
old in-repo `graft/` directory for anyone who wants the graph as readable text.
`--format md|json`, `--out DIR`.

---

## Rust port — fidelity gaps

### Call edges: 1040 vs TypeScript's 1138
Member calls now resolve **only** through a bound receiver type; an unknown
receiver produces no edge. That rule removed ~200 false edges (it was attributing
every `arr.push(...)` in the repo to a local function named `push`, which made
`push` the top hub at 82 in-edges) at the cost of some true ones the reference
implementation's more complete binding pass keeps.

The trade is right — hubs being correct matters more than the count matching —
but the difference is real and worth revisiting if `callers` ever feels thin.

### Python: field assigned from an annotated parameter
```python
def __init__(self, store: Store):
    self.store = store          # not resolved
```
`self.store = Store()` works; the above needs the parameter's type propagated
into the field binding. See `PY_BINDINGS` in `rust/src/extract.rs`.

### Indexed files: 124 vs 126
The two `.mjs` files under `scripts/`. Add `js`/`mjs`/`cjs` to `Lang::of_path`
in `rust/src/repo.rs` — the JavaScript grammar is not currently a dependency, so
this needs `tree-sitter-javascript` added (or the TypeScript grammar reused,
which parses plain JS).

### Incremental builds
`files.hash` is populated (FNV-1a, stable across Rust releases — deliberately not
`DefaultHasher`) but nothing reads it. Every build replaces the whole repo. Doing
this properly means only re-parsing changed files and re-resolving edges that
touched them; a partially-correct incremental build is worse than a slower
complete one, so it stayed out until the whole-repo path was trusted.

Full build of this repo: ~6s for 124 files.

### Workspace federation
The TypeScript build federates queries across a parent with ≥2 git children.
Not ported. In the SQLite model this becomes a query across multiple `repo_id`s
rather than a walk over child directories — likely simpler than the original.

---

## Host configuration

Deciding where the graft MCP server and instruction files get registered is
**independent of the Rust port** and still open. The port writes nothing into a
repo by design, so the remaining question is only where user-level registration
lands.

Verified user-level paths (all confirmed on this machine or in vendor docs):

| host | user-level target |
|---|---|
| Claude Code | `~/.claude/` — `CLAUDE.md`, `settings.json`, `skills/` |
| Codex | `~/.codex/config.toml` *(already global)* |
| Cursor | `~/.cursor/mcp.json` |
| Gemini CLI | `~/.gemini/settings.json` |
| Antigravity | `~/.gemini/config/mcp_config.json` *(fixed on `wallentx/fixes`)* |
| opencode | `~/.config/opencode/` |
| Copilot | `~/.copilot/copilot-instructions.md` |

**Open design question:** global instructions must stay quiet in repos with no graph.
The MCP not-indexed gate largely solves this — the server reports the state, so
the instruction text can be honest without being conditional.

---

## TypeScript implementation / infra

### `npm run build` fails on Termux
Exit 127: `node_modules/.bin/tsc` starts `#!/usr/bin/env node` and `/usr/bin/env`
does not exist. This means `npm install -g 'git+https://…#dev'` — the install
command the README prescribes — runs `prepare` → `npm run build` → fails.

Workaround in use: `node node_modules/typescript/bin/tsc -p tsconfig.json`.

Worth confirming whether an interactive Termux shell has `termux-exec` loaded
(`echo $LD_PRELOAD`). If it does, this is sandbox-only; if not, the documented
install path is broken on the platform this fork exists to support. The CI smoke
job runs on ubuntu and will not catch it either way.

### Flaky perf test
`test/graphrank.test.ts` — `PageRank: broad seeds on a large mostly-dangling
graph complete fast` asserts `ms < 3000`. Runs in ~1.9s isolated, ~6.3s under
full-suite contention on this device. It guards against a regression whose
failure mode is *minutes*, so the threshold is what is fragile. Either raise it
substantially or assert on operation count instead of wall clock.

### CI does not cover Termux
The `install-smoke` job added on `wallentx/fixes` validates the git-install path
on ubuntu, which is where `/usr/bin/env` exists. It cannot catch the shebang
class of failure that has bitten this fork three times.

# TODO

Native Rust rewrite status after local cutover work on `wallentx/rust`.

## Completed locally

- [x] Root Cargo workspace with the native crate in `rust/` and one root lockfile.
- [x] Version aligned at `0.8.2`; release profile uses thin LTO, one codegen unit,
  and symbol stripping.
- [x] SQLite schema v4 plus non-destructive v3 -> v4 migration.
- [x] Incremental per-file extraction cache keyed by content hash and extractor
  identity; changed/added/deleted files re-resolve repository-wide edges.
- [x] WAL readers, five-second busy timeout, transaction rollback coverage,
  integrity check, missing-repository pruning, per-repository reset, and
  recoverable corrupt-store replacement that preserves the old database.
- [x] Automatic live-source freshness before read commands, `--no-refresh`,
  `GRAFT_NO_REFRESH`, and non-mutating CI `check`.
- [x] TypeScript, TSX, JavaScript/JSX/MJS/CJS, Python, Go, and Rust extraction.
- [x] Python annotated-parameter -> `self.field` propagation.
- [x] Go and Rust receiver/module resolution.
- [x] Bounded symbol and file bodies in ranked retrieval.
- [x] `build`, ranked/structural `ask`, regex/fixed/case-aware `grep`, qualified
  `callers`, basename-aware `skeleton`, bounded `map`, `check`, and drift-aware
  `status`.
- [x] Stable JSON output for every native read command.
- [x] Immediate-child multi-repository workspace detection and federation for
  build/query/check/MCP, with labels and fair interleaving for ranked results.
- [x] Bounded newline-delimited MCP JSON-RPC with initialize, ping, tool schemas,
  structured errors, stale/unindexed refusal, and 1 MiB request/response limits.
- [x] MCP tools for find, grep, callers, skeleton, map, status, and freshness.
- [x] User-level `init --dry-run` and registration for Claude, Codex, Cursor,
  Gemini, Antigravity, opencode, and Copilot using the installed binary's
  absolute path. Existing JSON is merged; invalid JSON is never overwritten;
  writes are private and atomic.
- [x] Self-contained loopback-only-by-default Rust HTML viewer and explicit HTML
  export.
- [x] Deterministic Markdown-card/`INDEX.md` and JSON exports to explicit paths.
- [x] Detailed `version`, non-self-modifying `upgrade`, Bash/Zsh/Fish/Elvish/
  PowerShell completion generation, SIGPIPE handling, and deterministic
  non-color output.
- [x] Safe source install docs using the explicit
  `https://github.com/wallentx/Graft.git` repository and `dev` branch. No npm or
  `npx` installation path.
- [x] npm CLI mapping removed; TypeScript package renamed/private and retained
  only as the public-API/differential reference.
- [x] Tracked Node-era `.claude` hooks, shims, statusline, skill, and host config
  removed; local host directories ignored.
- [x] Stripped archive + SHA-256 packaging script and tag/manual release workflow.
- [x] Ubuntu isolated-prefix release smoke covering every command help path,
  indexing/querying/export/viewer/MCP/init dry-run/SIGPIPE.
- [x] Termux Bionic-container CI plus a manually dispatched real
  Android/AArch64 self-hosted-runner gate.
- [x] TypeScript-to-Rust semantic differential fixture for ranked retrieval and
  call traversal.
- [x] Native Termux release built and validated locally: AArch64 Android ELF,
  `/system/bin/linker64`, Android API 24, stripped; archive checksum verified.
- [x] Native local suite: 48 Rust tests, rustfmt, clippy `-D warnings`, actionlint,
  shellcheck, release smoke, MCP smoke, and differential smoke.

## Deliberate scope decisions

- No native `--deep` or model/network enrichment. Native retrieval is local and
  deterministic; concept-node fusion belongs to the retained TypeScript library,
  not the installed CLI.
- No extension-selection flag. One repository identity always indexes the full
  supported-language set; gitignore rules are the opt-out mechanism. Declaration
  files are skipped. Generated/vendor files follow ignore rules rather than
  filename guesses.
- Native dependency traversal uses `calls`, `imports`, and `contains`. Reference,
  inheritance, and implementation edges are not part of the native `callers`
  contract. Unknown receiver types remain unresolved instead of creating false
  same-name hubs.
- Linked Git worktrees keep separate source rows because their checked-out files
  can differ. `git_common_dir` is recorded for diagnosis only.
- No repository-local agent instructions or background edit hooks. Query-time
  freshness replaces hooks and keeps global instructions silent outside indexed
  repositories.
- No self-updater or downloaded installer execution. Updates stay under Cargo,
  Git, or a release manager.
- No ASCII-mode flag. JSON contains data-only strings; human output uses a small
  fixed Unicode vocabulary and never emits color.
- Keep `rust/` as the package directory under a root Cargo workspace. Do not move
  the crate sources again for cosmetic layout reasons.
- Keep the private TypeScript source/tests/viewer temporarily as executable
  differential and public-JavaScript-API history. They do not install a CLI.

## Performance backlog

Performance was not the primary cutover target. The native implementation
already avoids several obvious costs: unchanged files reuse cached extraction,
SQLite uses WAL and FTS, indexed bodies are bounded, and release builds use thin
LTO with stripped symbols. No benchmark suite exists yet, so do not claim the
Rust CLI is faster than the TypeScript CLI without measurements.

- [ ] Add reproducible benchmarks for cold builds, warm no-change builds,
  one-file rebuilds, common queries, MCP calls, and large multi-repository
  workspaces. Record wall time, peak memory, and database size on both Termux and
  a conventional Linux host.
- [ ] Profile representative workloads before changing hot paths; retain
  before/after profiles with benchmark results.
- [ ] Evaluate bounded parallel file parsing while preserving deterministic
  database contents, output ordering, and useful behavior on memory-constrained
  phones.
- [ ] Replace repository-wide edge re-resolution after a changed file with a
  proven affected-edge update algorithm.
- [ ] Benchmark larger SQLite write batches, longer-lived prepared statements,
  query plans, and additional indexes. Keep transaction rollback and concurrent
  reader guarantees intact.
- [ ] Reduce extractor and ranking allocations, string cloning, and repeated
  tokenization where profiling shows meaningful cost.
- [ ] Evaluate paginated or chunked handling for large exports and MCP tool
  results while preserving the 1 MiB JSON-RPC frame limit.
- [ ] Evaluate a Git-assisted freshness fast path with a content-hash fallback
  for untracked, ignored, non-Git, and timestamp-ambiguous files.
- [ ] Publish performance claims only with repeatable benchmark commands,
  fixture sizes, hardware/runtime details, and measured before/after results.

## Remaining release operations

These require pushed GitHub state or external infrastructure and are separate
from the optional local performance backlog.

- [ ] Review the worktree, commit on the existing `wallentx/rust` branch, and push
  only when explicitly requested.
- [ ] Run GitHub CI and fix any hosted-runner or `termux/termux-docker` drift.
- [ ] Register/enable a `[self-hosted, termux, ARM64]` runner and run the real
  Android workflow at least once.
- [ ] After pushing the implementation to `dev`, verify the documented command:
  `cargo install --git https://github.com/wallentx/Graft.git --branch dev --locked graft`.
- [ ] Tag the first native release, verify uploaded archive/checksum artifacts,
  and publish an Android/AArch64 artifact produced by the native Termux runner.
- [ ] After one native release cycle, decide whether any consumer still needs the
  private TypeScript API. If not, delete `src/`, `test/`, `viewer/`, npm metadata,
  `.env.example`, `.ignore`, and Node-only scripts in one dedicated cleanup.

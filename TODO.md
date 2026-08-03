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
- [x] Interactive checkbox provider selection plus repeatable `--provider`,
  `init --dry-run`, and registration for Claude Code, Codex, Cursor, Gemini CLI,
  Antigravity, OpenCode, and Copilot CLI using the installed binary's absolute
  path. Existing JSON is merged; invalid JSON is never overwritten; writes are
  private and atomic.
- [x] Self-contained loopback-only-by-default Rust HTML viewer and explicit HTML
  export.
- [x] Deterministic Markdown-card/`INDEX.md` and JSON exports to explicit paths.
- [x] Detailed `version`, non-self-modifying `upgrade`, Bash/Zsh/Fish/Elvish/
  PowerShell completion generation, SIGPIPE handling, and deterministic
  non-color output.
- [x] Safe source install docs and an inspect-first local installer using the
  explicit `https://github.com/wallentx/Graft.git` repository and `dev` branch.
  No npm, `npx`, `sudo`, or remote-script execution path.
- [x] npm CLI mapping and the complete legacy TypeScript/npm package removed.
- [x] Tracked Node-era `.claude` hooks, shims, statusline, skill, and provider
  config removed; local provider directories ignored.
- [x] Stripped archive + SHA-256 packaging script and tag/manual release workflow.
- [x] Ubuntu isolated-prefix release smoke covering every command help path,
  indexing/querying/export/viewer/MCP/init dry-run/SIGPIPE.
- [x] Termux Bionic-container CI plus a manually dispatched real
  Android/AArch64 self-hosted-runner gate.
- [x] Pre-cutover TypeScript-to-Rust differential validation completed for
  ranked retrieval and call traversal; the obsolete harness was then removed.
- [x] Native Termux release built and validated locally: AArch64 Android ELF,
  `/system/bin/linker64`, Android API 24, stripped; archive checksum verified.
- [x] Native local suite: 52 Rust tests, rustfmt, clippy `-D warnings`, actionlint,
  shellcheck, release smoke, and MCP smoke. Pre-cutover differential smoke also
  passed before removal of the legacy implementation.
- [x] Removed obsolete TypeScript sources/tests/viewer, npm metadata, Node-only
  scripts, differential fixtures, unreferenced media, and generated Node/graph/
  build caches after the native cutover audit.

## Deliberate scope decisions

- No native `--deep` or model/network enrichment. Native retrieval is local and
  deterministic; legacy concept-node fusion was deliberately retired.
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
- Legacy TypeScript implementation remains available through Git history, not
  the working tree or installed product.

## Performance backlog

Performance was not the primary cutover target. The native implementation
already avoids several obvious costs: unchanged files reuse cached extraction,
SQLite uses WAL and FTS, indexed bodies are bounded, and release builds use thin
LTO with stripped symbols. The benchmark suite and device-scoped results live in
`scripts/benchmark.sh` and `bench/RESULTS.md`; do not generalize those results to
other repositories or systems without rerunning the harness.

- [x] Added reproducible benchmarks for cold builds, warm no-change builds,
  one-file rebuilds, common queries, MCP calls, and large multi-repository
  workspaces. Runs record wall time, peak memory, database size, toolchain/device
  metadata, SQLite query plans, and per-workload medians.
- [x] Profiled representative Termux workloads before optimization and retained
  compact before/after measurements and syscall-profile summaries under
  `bench/results/`.
- [x] Removed Git subprocesses from normal repository/worktree discovery, reused
  per-language Tree-sitter parser/query state, qualified in-degree subqueries so
  existing composite indexes are used, reused the edge insert statement, and
  skipped unchanged file metadata writes.
- [x] Added bounded parallel file parsing for change sets above 64 files, with a
  four-worker automatic ceiling, `--jobs`/`GRAFT_JOBS` overrides, per-worker
  parser state, ordered database insertion, and serial/parallel graph-equivalence
  coverage. On the measured Termux device, four workers reduced the 4,000-file
  cold-build median from 2,555 ms to 2,422 ms (5.2%) while increasing peak RSS
  from 30,424 KiB to 31,080 KiB (2.2%); smaller change sets remain serial.
- [ ] Replace repository-wide edge re-resolution after a changed file with a
  proven affected-edge update algorithm.
- [x] Benchmarked prepared-statement reuse and existing SQLite query plans while
  preserving transaction rollback and concurrent reader guarantees. Further
  batching or indexes require a new profile demonstrating need.
- [ ] Reduce extractor and ranking allocations, string cloning, and repeated
  tokenization where profiling shows meaningful cost.
- [ ] Evaluate paginated or chunked handling for large exports and MCP tool
  results while preserving the 1 MiB JSON-RPC frame limit.
- [ ] Evaluate a Git-assisted freshness fast path with a content-hash fallback
  for untracked, ignored, non-Git, and timestamp-ambiguous files.
- [x] Published only scoped claims with repeatable benchmark commands, fixture
  sizes, hardware/runtime details, medians, and measured before/after results.
- [ ] Run and retain the same benchmark matrix on a conventional Linux system.

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

# Performance results

## 2026-08-02 Termux/AArch64

Same-device comparison on Android/AArch64 using `scripts/benchmark.sh` with its
default 400-file repository, three 100-file workspace children, three build
repetitions, and five query repetitions. Values are medians. The optimized
binary was built from the working-tree performance patch on top of
`5f01e4c9e929f7ee0f0354d80d979571852b3793`; the before binary was the installed
pre-patch build from the same native rewrite.

Environment:

- Linux `6.1.162-android14-11-g752d9c17787d-ab15574904`, AArch64 Android.
- Rust `1.97.1`; Cargo `1.97.1`; Graft `0.8.2`.
- Release profile: thin LTO, one codegen unit, stripped symbols.

| Workload | Before ms | After ms | Reduction | Speedup |
|---|---:|---:|---:|---:|
| Cold build | 16,860 | 490 | 97.1% | 34.4x |
| Warm build | 397 | 193 | 51.4% | 2.1x |
| One-file build | 446 | 166 | 62.8% | 2.7x |
| Ask | 593 | 176 | 70.3% | 3.4x |
| Grep | 488 | 103 | 78.9% | 4.7x |
| Callers | 168 | 83 | 50.6% | 2.0x |
| Status | 159 | 78 | 50.9% | 2.0x |
| MCP find | 590 | 172 | 70.8% | 3.4x |
| Workspace cold build | 4,520 | 263 | 94.2% | 17.2x |
| Workspace warm build | 321 | 117 | 63.6% | 2.7x |
| Workspace ask | 282 | 93 | 67.0% | 3.0x |

Cold-build peak RSS fell from 16,584 KiB to 14,228 KiB. Database sizes stayed
unchanged for equivalent fixture contents. Query RSS stayed effectively flat.

`strace -f -c` exposed Git subprocess startup as the command latency floor:
before optimization, `wait4` consumed 0.462 seconds of warm-build syscall time
and 0.310 seconds of ask syscall time. Native `.git` and linked-worktree marker
discovery removed those waits. Reusing Tree-sitter parsers and compiled queries
removed per-file grammar/query setup. Repository-qualified in-degree subqueries
then enabled the existing composite edge index for ask, grep, callers, and map.

These results support claims only for this fixture and device. Run the harness
on target repositories and conventional Linux before generalizing.

Compact measurements and syscall-profile summaries are retained under
`bench/results/`. Full run directories default to `$TMPDIR`; pass `--output` to
retain them elsewhere.

### Bounded parallel extraction

A separate three-run comparison used a 4,000-file fixture after the preceding
optimizations. Forcing `GRAFT_JOBS=1` produced a 2,555 ms cold-build median and
30,424 KiB peak RSS. `GRAFT_JOBS=4` produced 2,422 ms and 31,080 KiB: 5.2% less
wall time for 2.2% more peak memory. Warm and one-file builds remain serial by
design because the worker pool is enabled only when more than 64 files require
parsing. The serial and parallel paths also have a graph-equivalence regression
test covering ordered symbols and edges.

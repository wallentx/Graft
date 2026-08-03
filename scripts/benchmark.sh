#!/usr/bin/env sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp_base=${TMPDIR:-/tmp}
files=${GRAFT_BENCH_FILES:-400}
query_runs=${GRAFT_BENCH_QUERY_RUNS:-5}
build_runs=${GRAFT_BENCH_BUILD_RUNS:-3}
workspace_repos=${GRAFT_BENCH_WORKSPACE_REPOS:-3}
workspace_files=${GRAFT_BENCH_WORKSPACE_FILES:-100}
profile=${GRAFT_BENCH_PROFILE:-1}
graft_bin=${GRAFT_BENCH_BIN:-"$repo_dir/target/release/graft"}
output=

usage() {
    printf '%s\n' \
        "usage: $0 [--binary PATH] [--output DIR] [--files N] [--query-runs N]" \
        "          [--build-runs N] [--workspace-repos N] [--workspace-files N]" \
        "          [--no-profile]"
}

positive_integer() {
    case $2 in
        ''|*[!0-9]*|0) printf '%s must be a positive integer\n' "$1" >&2; exit 2 ;;
    esac
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --binary) graft_bin=$2; shift 2 ;;
        --output) output=$2; shift 2 ;;
        --files) files=$2; shift 2 ;;
        --query-runs) query_runs=$2; shift 2 ;;
        --build-runs) build_runs=$2; shift 2 ;;
        --workspace-repos) workspace_repos=$2; shift 2 ;;
        --workspace-files) workspace_files=$2; shift 2 ;;
        --no-profile) profile=0; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

positive_integer --files "$files"
positive_integer --query-runs "$query_runs"
positive_integer --build-runs "$build_runs"
positive_integer --workspace-repos "$workspace_repos"
positive_integer --workspace-files "$workspace_files"

if [ -z "$output" ]; then
    output="$tmp_base/graft-benchmark-$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
scratch=$(mktemp -d "$tmp_base/graft-benchmark-fixture.XXXXXX")
measure_bin="$scratch/rusage"
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$output"

cc=${CC:-cc}
"$cc" -O2 -Wall -Wextra -Werror "$repo_dir/bench/rusage.c" -o "$measure_bin"

if [ ! -x "$graft_bin" ]; then
    cargo build --manifest-path "$repo_dir/Cargo.toml" --release --locked --package graft
fi
graft_bin=$(CDPATH='' cd -- "$(dirname -- "$graft_bin")" && pwd)/$(basename -- "$graft_bin")

generate_repo() {
    destination=$1
    count=$2
    mkdir -p "$destination/src"
    git -C "$destination" init -q
    i=0
    while [ "$i" -lt "$count" ]; do
        current=$(printf '%05d' "$i")
        path="$destination/src/module_$current.ts"
        if [ "$i" -gt 0 ]; then
            previous=$(printf '%05d' "$((i - 1))")
            printf 'import { process_payment_%s_0 } from "./module_%s";\n' "$previous" "$previous" > "$path"
            printf 'export function process_payment_%s_0(input: number) { return process_payment_%s_0(input) + 1; }\n' "$current" "$previous" >> "$path"
        else
            printf 'export function process_payment_%s_0(input: number) { return input + 1; }\n' "$current" > "$path"
        fi
        printf 'export function validate_gateway_%s(input: number) { return process_payment_%s_0(input) > 0; }\n' "$current" "$current" >> "$path"
        printf 'export class PaymentService%s { process(input: number) { return validate_gateway_%s(input); } }\n' "$current" "$current" >> "$path"
        i=$((i + 1))
    done
}

single="$scratch/single"
workspace="$scratch/workspace"
generate_repo "$single" "$files"
mkdir -p "$workspace"
i=0
while [ "$i" -lt "$workspace_repos" ]; do
    child=$(printf 'repo-%02d' "$i")
    generate_repo "$workspace/$child" "$workspace_files"
    i=$((i + 1))
done

single_store="$scratch/single.db"
workspace_store="$scratch/workspace.db"
results="$output/results.tsv"
printf 'workload\titeration\twall_ms\tpeak_rss_kb\tdb_bytes\texit_code\n' > "$results"

measure() {
    workload=$1
    iteration=$2
    store=$3
    shift 3
    metrics="$scratch/metrics-$workload-$iteration.tsv"
    stdout="$output/$workload-$iteration.stdout"
    stderr="$output/$workload-$iteration.stderr"
    set +e
    "$measure_bin" "$metrics" "$@" > "$stdout" 2> "$stderr"
    wrapper_status=$?
    set -e
    if [ ! -s "$metrics" ]; then
        printf '%s failed before metrics were recorded\n' "$workload" >&2
        sed -n '1,80p' "$stderr" >&2
        exit "$wrapper_status"
    fi
    tab=$(printf '\t')
    IFS="$tab" read -r wall_ms peak_rss exit_code < "$metrics"
    db_bytes=0
    if [ -f "$store" ]; then
        db_bytes=$(wc -c < "$store" | tr -d ' ')
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$workload" "$iteration" "$wall_ms" "$peak_rss" "$db_bytes" "$exit_code" >> "$results"
    if [ "$exit_code" -ne 0 ]; then
        printf '%s exited %s\n' "$workload" "$exit_code" >&2
        sed -n '1,80p' "$stderr" >&2
        exit "$exit_code"
    fi
}

i=1
while [ "$i" -le "$build_runs" ]; do
    cold_store=$single_store
    if [ "$i" -gt 1 ]; then
        cold_store="$scratch/single-cold-$i.db"
    fi
    measure cold-build "$i" "$cold_store" "$graft_bin" --store "$cold_store" build "$single"
    i=$((i + 1))
done
i=1
while [ "$i" -le "$build_runs" ]; do
    measure warm-build "$i" "$single_store" "$graft_bin" --store "$single_store" build "$single"
    i=$((i + 1))
done
middle=$(printf '%05d' "$((files / 2))")
i=1
while [ "$i" -le "$build_runs" ]; do
    printf '\nexport function changed_hot_path_%s_%s() { return process_payment_%s_0(1); }\n' \
        "$middle" "$i" "$middle" >> "$single/src/module_$middle.ts"
    measure one-file-build "$i" "$single_store" "$graft_bin" --store "$single_store" build "$single"
    i=$((i + 1))
done

i=1
while [ "$i" -le "$query_runs" ]; do
    measure ask "$i" "$single_store" "$graft_bin" --store "$single_store" ask "process payment" "$single" --limit 20 --json
    measure grep "$i" "$single_store" "$graft_bin" --store "$single_store" grep process_payment --fixed --path "$single" --json
    measure callers "$i" "$single_store" "$graft_bin" --store "$single_store" callers process_payment_00000_0 --path "$single" --depth 2 --json
    measure status "$i" "$single_store" "$graft_bin" --store "$single_store" status "$single" --json
    i=$((i + 1))
done

printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"benchmark","version":"1"}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find","arguments":{"query":"process payment","limit":20}}}' \
    > "$scratch/mcp.requests"
i=1
while [ "$i" -le "$query_runs" ]; do
    measure mcp-find "$i" "$single_store" "$graft_bin" --store "$single_store" mcp "$single" < "$scratch/mcp.requests"
    i=$((i + 1))
done

i=1
while [ "$i" -le "$build_runs" ]; do
    cold_store=$workspace_store
    if [ "$i" -gt 1 ]; then
        cold_store="$scratch/workspace-cold-$i.db"
    fi
    measure workspace-cold-build "$i" "$cold_store" "$graft_bin" --store "$cold_store" build "$workspace"
    i=$((i + 1))
done
i=1
while [ "$i" -le "$build_runs" ]; do
    measure workspace-warm-build "$i" "$workspace_store" "$graft_bin" --store "$workspace_store" build "$workspace"
    i=$((i + 1))
done
measure workspace-ask 1 "$workspace_store" "$graft_bin" --store "$workspace_store" ask "process payment" "$workspace" --limit 20 --json

{
    printf 'key\tvalue\n'
    printf 'timestamp_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'uname\t%s\n' "$(uname -a | tr '\t' ' ')"
    printf 'rustc\t%s\n' "$(rustc --version)"
    printf 'cargo\t%s\n' "$(cargo --version)"
    printf 'graft\t%s\n' "$("$graft_bin" --version)"
    printf 'git_sha\t%s\n' "$(git -C "$repo_dir" rev-parse --verify HEAD)"
    printf 'git_dirty\t%s\n' "$(test -n "$(git -C "$repo_dir" status --porcelain)" && printf true || printf false)"
    printf 'single_files\t%s\n' "$files"
    printf 'workspace_repos\t%s\n' "$workspace_repos"
    printf 'workspace_files_per_repo\t%s\n' "$workspace_files"
    printf 'query_runs\t%s\n' "$query_runs"
    printf 'build_runs\t%s\n' "$build_runs"
    printf 'graft_jobs\t%s\n' "${GRAFT_JOBS:-auto}"
} > "$output/metadata.tsv"

if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$single_store" > "$output/sqlite.txt" <<'EOF'
.headers on
.mode column
select 'files' as object, count(*) as rows from files
union all select 'symbols', count(*) from symbols
union all select 'edges', count(*) from edges;
pragma page_count;
pragma page_size;
explain query plan select id from symbols where repo_id=1 and name='process_payment_00000_0';
explain query plan select src_symbol_id from edges where repo_id=1 and dst_symbol_id=1;
EOF
fi

if [ "$profile" -ne 0 ] && command -v strace >/dev/null 2>&1; then
    if ! strace -f -c -o "$output/profile-warm-build.txt" \
        "$graft_bin" --store "$single_store" build "$single" >/dev/null 2> "$output/profile-warm-build.stderr"; then
        printf '%s\n' 'strace warm-build profile unavailable' >> "$output/profile-warm-build.stderr"
    fi
    if ! strace -f -c -o "$output/profile-ask.txt" \
        "$graft_bin" --store "$single_store" ask "process payment" "$single" --limit 20 --json >/dev/null 2> "$output/profile-ask.stderr"; then
        printf '%s\n' 'strace ask profile unavailable' >> "$output/profile-ask.stderr"
    fi
fi

median_column() {
    workload=$1
    column=$2
    values="$scratch/median-$workload-$column"
    awk -F '\t' -v workload="$workload" -v column="$column" \
        'NR > 1 && $1 == workload { print $column }' "$results" | sort -n > "$values"
    count=$(wc -l < "$values" | tr -d ' ')
    awk -v count="$count" '
        NR == int((count + 1) / 2) { lower = $1 }
        NR == int((count + 2) / 2) { upper = $1 }
        END {
            if (count % 2 == 1) print upper
            else printf "%.1f\n", (lower + upper) / 2
        }
    ' "$values"
}

summary="$output/summary.tsv"
printf 'workload\tsamples\tmedian_wall_ms\tmedian_peak_rss_kb\tdb_bytes\n' > "$summary"
awk -F '\t' 'NR > 1 { print $1 }' "$results" | sort -u | while IFS= read -r workload; do
    samples=$(awk -F '\t' -v workload="$workload" 'NR > 1 && $1 == workload { count++ } END { print count + 0 }' "$results")
    wall=$(median_column "$workload" 3)
    rss=$(median_column "$workload" 4)
    db_bytes=$(awk -F '\t' -v workload="$workload" 'NR > 1 && $1 == workload { value = $5 } END { print value + 0 }' "$results")
    printf '%s\t%s\t%s\t%s\t%s\n' "$workload" "$samples" "$wall" "$rss" "$db_bytes" >> "$summary"
done

printf 'benchmark results: %s\n' "$output"
printf 'measurements: %s\n' "$results"
printf 'summary: %s\n' "$summary"

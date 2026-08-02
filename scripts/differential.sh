#!/data/data/com.termux/files/usr/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
rust_bin=${1:-"$repo_dir/target/debug/graft"}
tmp_base=${TMPDIR:-/tmp}
fixture=$(mktemp -d "$tmp_base/graft-differential.XXXXXX")
trap 'rm -rf "$fixture"' EXIT HUP INT TERM

cp -R "$repo_dir/fixtures/differential/." "$fixture/"
git -C "$fixture" init -q

node "$repo_dir/dist/cli.js" build "$fixture" >/dev/null
"$rust_bin" --store "$fixture/rust.db" build "$fixture" >/dev/null

node "$repo_dir/dist/cli.js" ask "process payment" "$fixture" --json > "$fixture/ts-ask.json"
"$rust_bin" --store "$fixture/rust.db" ask "process payment" "$fixture" --json > "$fixture/rs-ask.json"
jq -e '.hits[0].title | startswith("processPayment")' "$fixture/ts-ask.json" >/dev/null
jq -e '.hits[0].name == "processPayment"' "$fixture/rs-ask.json" >/dev/null

(cd "$fixture" && node "$repo_dir/dist/cli.js" callers processPayment --json) > "$fixture/ts-callers.json"
"$rust_bin" --store "$fixture/rust.db" callers processPayment --path "$fixture" --json > "$fixture/rs-callers.json"
jq -e '.matches[0].hits | any(.name == "main" and .relation == "calls")' "$fixture/ts-callers.json" >/dev/null
jq -e '.results[0].reached | any(.name == "main" and .edge == "calls")' "$fixture/rs-callers.json" >/dev/null

"$rust_bin" --store "$fixture/rust.db" grep 'processPayment' --fixed --path "$fixture" --json > "$fixture/rs-grep.json"
jq -e '.[] | .total_hits >= 3' "$fixture/rs-grep.json" >/dev/null
"$rust_bin" --store "$fixture/rust.db" check "$fixture" --json >/dev/null

printf '%s\n' "differential semantics: ok"

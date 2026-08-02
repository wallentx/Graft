#!/data/data/com.termux/files/usr/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${1:-"$repo_dir/release"}
target=$(rustc -vV | sed -n 's/^host: //p')
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/rust/Cargo.toml" | head -n 1)
git_sha=$(git -C "$repo_dir" rev-parse --verify HEAD)

mkdir -p "$output_dir"
GRAFT_GIT_SHA=$git_sha cargo build --manifest-path "$repo_dir/Cargo.toml" --release --locked --package graft

stage=${TMPDIR:-/tmp}/graft-release-$$
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/graft-$version-$target"
cp "$repo_dir/target/release/graft" "$stage/graft-$version-$target/graft"
cp "$repo_dir/README.md" "$repo_dir/LICENSE" "$stage/graft-$version-$target/"

archive="$output_dir/graft-$version-$target.tar.gz"
tar -C "$stage" -czf "$archive" "graft-$version-$target"
sha256sum "$archive" > "$archive.sha256"
printf '%s\n' "$archive"
printf '%s\n' "$archive.sha256"

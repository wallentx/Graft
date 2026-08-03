#!/usr/bin/env sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
install_root=${GRAFT_INSTALL_ROOT:-"${HOME:?HOME is not set}/.local"}

usage() {
    printf '%s\n' \
        'Install Graft from this inspected checkout.' \
        '' \
        "usage: $0 [--prefix DIR]" \
        '' \
        'options:' \
        "  --prefix DIR  installation root (default: \$HOME/.local)" \
        '  -h, --help    show this help' \
        '' \
        'The script does not fetch Graft or modify provider configuration.' \
        'Cargo may fetch the dependency versions pinned in Cargo.lock.'
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --prefix)
            if [ "$#" -lt 2 ]; then
                printf '%s\n' 'error: --prefix requires a directory' >&2
                exit 2
            fi
            install_root=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ ! -f "$repo_dir/Cargo.lock" ] || [ ! -f "$repo_dir/rust/Cargo.toml" ]; then
    printf 'error: %s is not a complete Graft checkout\n' "$repo_dir" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    printf '%s\n' 'error: Rust and Cargo are required.' >&2
    if command -v pkg >/dev/null 2>&1; then
        printf '%s\n' 'On Termux: pkg install git rust clang' >&2
    else
        printf '%s\n' 'Install Rust with your operating system package manager, then rerun this script.' >&2
    fi
    exit 1
fi

mkdir -p "$install_root"
install_root=$(CDPATH='' cd -- "$install_root" && pwd)
git_sha=source-build
if command -v git >/dev/null 2>&1; then
    detected_sha=$(git -C "$repo_dir" rev-parse --verify HEAD 2>/dev/null || true)
    if [ -n "$detected_sha" ]; then
        git_sha=$detected_sha
    fi
fi

printf 'Installing Graft from %s\n' "$repo_dir"
printf 'Installation root: %s\n' "$install_root"
GRAFT_GIT_SHA=$git_sha cargo install \
    --path "$repo_dir/rust" \
    --locked \
    --force \
    --root "$install_root"

binary="$install_root/bin/graft"
if [ ! -x "$binary" ]; then
    printf 'error: Cargo completed but %s is not executable\n' "$binary" >&2
    exit 1
fi
"$binary" --version

case :${PATH:-}: in
    *:"$install_root/bin":*) ;;
    *)
        printf '\nAdd Graft to PATH:\n'
        printf '  export PATH="%s/bin:%s"\n' "$install_root" "\$PATH"
        ;;
esac

printf '%s\n' '' 'Next:' '  graft init' '  cd /path/to/repository && graft build'

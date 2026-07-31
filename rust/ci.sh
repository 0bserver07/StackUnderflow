#!/usr/bin/env bash
# The gates every wave of the Rust port must pass, in order.
#
#   1. cargo fmt --check   — formatting is the law inside rust/. The Python
#                            tree's format exemption does not apply here; a
#                            fleet of agents writing Rust in parallel needs one
#                            canonical layout or every diff is noise.
#   2. cargo clippy        — --all-targets so tests and benches are linted too,
#                            -D warnings so a lint is a failure, not a message
#                            nobody reads.
#   3. cargo test          — the whole workspace.
#
# Usage:  rust/ci.sh          (runs all three)
# Exit:   first failing gate's status.
set -euo pipefail

cd "$(dirname "$0")"

# The toolchain is user-local (docs/specs/rust-port.md §5: rustup, no sudo).
# Rustup must WIN over a distro cargo (found the hard way: /usr/bin/cargo 1.75
# shadows the pinned 1.97 and dies on edition2024). Prepend unconditionally.
if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

gate() {
    printf '\n=== %s ===\n' "$1"
    shift
    "$@"
}

# Gate 0 exists because it caught us: a manifest edit lived only in the working
# tree while its Cargo.lock entry was committed, so five commits in a row failed
# to compile from a clean checkout while every in-tree run stayed green. The
# working tree's opinion of itself is not evidence.
echo "=== gate 0/4  clean-checkout build (git archive HEAD) ==="
_cc_tmp="$(mktemp -d)"
( cd "$(git rev-parse --show-toplevel)" && git archive HEAD rust/ contracts/ ) | tar -x -C "$_cc_tmp"
( cd "$_cc_tmp/rust" && cargo check --workspace --quiet ) \
    || { echo "GATE 0 FAILED: HEAD does not build from a clean checkout"; rm -rf "$_cc_tmp"; exit 1; }
rm -rf "$_cc_tmp"
echo "    clean checkout builds"

gate "gate 1/4  cargo fmt --check" cargo fmt --check
gate "gate 2/4  cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings
gate "gate 3/4  cargo test --workspace" cargo test --workspace

printf '\nall three gates green\n'

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
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

gate() {
    printf '\n=== %s ===\n' "$1"
    shift
    "$@"
}

gate "gate 1/3  cargo fmt --check" cargo fmt --check
gate "gate 2/3  cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings
gate "gate 3/3  cargo test --workspace" cargo test --workspace

printf '\nall three gates green\n'

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
#   4. CLI byte-parity     — every verb × {fresh store, populated-FTS store} ×
#                            {text, --json}: stdout, stderr and exit code
#                            diffed byte for byte against the Python CLI. This
#                            is the P0 gate (rust/DIRECTIVE-PARITY-P0.md):
#                            "drop-in or it doesn't ship", and the
#                            populated-FTS state is the maintainer's machine,
#                            not an edge case. Skipped only where the Python
#                            venv or the harness states are absent — loudly.
#
# Usage:  rust/ci.sh                (runs all five)
#         rust/ci.sh --skip-parity  (gates 0-3 only; boxes without the venv)
# Exit:   first failing gate's status.
set -euo pipefail

cd "$(dirname "$0")"

SKIP_PARITY=0
for arg in "$@"; do
    case "$arg" in
        --skip-parity) SKIP_PARITY=1 ;;
        *) echo "ci.sh: unknown argument '$arg'" >&2; exit 2 ;;
    esac
done

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
echo "=== gate 0/5  clean-checkout build (git archive HEAD) ==="
_cc_tmp="$(mktemp -d)"
( cd "$(git rev-parse --show-toplevel)" && git archive HEAD rust/ contracts/ ) | tar -x -C "$_cc_tmp"
( cd "$_cc_tmp/rust" && cargo check --workspace --quiet ) \
    || { echo "GATE 0 FAILED: HEAD does not build from a clean checkout"; rm -rf "$_cc_tmp"; exit 1; }
rm -rf "$_cc_tmp"
echo "    clean checkout builds"

gate "gate 1/5  cargo fmt --check" cargo fmt --check
gate "gate 2/5  cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings
gate "gate 3/5  cargo test --workspace" cargo test --workspace

# Gate 4 — the P0 gate. It is last because it is the slowest and because a
# workspace that does not compile cannot be diffed; it is not optional because
# every gate above it can be green while the shipped binary answers a question
# differently from the tool it replaces. That is exactly what happened: 39 of
# 188 cases diverged on the populated-FTS store while gates 0-3 were green.
printf '\n=== gate 4/5  CLI byte-parity vs the Python CLI ===\n'
if [ "$SKIP_PARITY" = 1 ]; then
    echo '  !!  GATE 4 SKIPPED by --skip-parity.'
    echo '  !!  Drop-in parity is UNVERIFIED for this run. The only sanctioned'
    echo '  !!  reason is a box without the Python venv; a green ci.sh here'
    echo '  !!  does NOT mean the binary is a drop-in replacement.'
else
    _parity_out="$(mktemp)"
    set +e
    ./parity-cli.sh 2>&1 | tee "$_parity_out"
    _parity_rc="${PIPESTATUS[0]}"
    set -e
    case "$_parity_rc" in
        0) : ;;
        2)
            echo
            echo '  !!  GATE 4 COULD NOT RUN (setup): see the message above.'
            echo '  !!  Build the states once with: rust/parity-cli.sh --build-state'
            echo '  !!  Re-run with --skip-parity only if this box genuinely has'
            echo '  !!  no Python venv — parity is UNVERIFIED either way.'
            rm -f "$_parity_out"
            exit 2
            ;;
        *)
            echo
            echo "GATE 4 FAILED: the CLI is not a drop-in replacement (see the tally above)."
            rm -f "$_parity_out"
            exit 1
            ;;
    esac
    rm -f "$_parity_out"
fi

printf '\nall gates green\n'

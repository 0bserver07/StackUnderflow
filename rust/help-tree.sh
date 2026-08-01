#!/usr/bin/env bash
# The `--help`-tree differ — wave 8's RS-8-087 measurement.
#
# Dumps `--help` for every node of the Python command tree AND for the same node
# of the Rust binary, with COLUMNS=80 pinned on both sides, and reports the three
# contract facts the wave-8 items name: same summary, same options, same
# subcommand list. Writes rust/parity/HELP-TREE.md.
#
# Byte parity is NOT the gate here and the report says why in full: Click's and
# clap's templates differ structurally (D-1, measured in the report). Nodes the
# port has not reached are listed by name — never skipped silently.
#
#   rust/help-tree.sh              # measure, write the report
#
# Exit: 0 when every ported node agrees on all three facts, 1 otherwise,
# 2 on a setup failure. NOT wired into ci.sh: it is a measurement whose verdict
# is a maintainer ruling (DIV-240), not a regression gate.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
export STAX_PARITY_PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
export STAX_PARITY_RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
OUT="${1:-$HERE/parity/HELP-TREE.md}"

if [ ! -x "$PY_INTERP" ]; then
    echo "help-tree: SETUP FAILURE — no Python interpreter at $PY_INTERP" >&2
    exit 2
fi
if [ ! -x "$STAX_PARITY_PY_BIN" ]; then
    echo "help-tree: SETUP FAILURE — no Python CLI at $STAX_PARITY_PY_BIN" >&2
    exit 2
fi
if [ ! -x "$STAX_PARITY_RS_BIN" ]; then
    echo "help-tree: building the release binary (the report compares shipped bytes)"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
    "$PY_INTERP" "$HERE/parity/tools/help_tree.py" "$OUT"

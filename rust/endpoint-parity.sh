#!/usr/bin/env bash
# Gate 6 — the HTTP byte-diff harness.
#
# Gate 4 proved the CLI is a drop-in replacement by diffing stdout byte for
# byte. This does the same job for the dashboard: boot BOTH servers against one
# shared `STACKUNDERFLOW_HOME`, walk `parity/endpoint-cases.txt` in order, and
# diff status + content-type + BODY BYTES.
#
#   python  uvicorn parity/pyserver.py:app on :8097 (the reference)
#   rust    target/release/stax-server     on :8096 (the port)
#
# :8095 is NEVER bound — that is the maintainer's live server
# (docs/specs/rust-port.md §5), and both the driver and the differ refuse it.
#
# ── Why one shared home, and which one ───────────────────────────────────────
#
# One home, two binaries, is the real deployment; it is also the only way the
# two sides can be reading identical bytes. The default is the state gate 4
# already builds — `.parity-state/fresh` — which is a `Connection.backup()`
# snapshot of the live store, never the live store itself. Point
# STAX_ENDPOINT_HOME elsewhere for a different corpus.
#
# The Python server boots through `parity/pyserver.py`, which disarms the three
# writers its lifespan would otherwise run (ingest, price-book backfill, cold
# cache rmtree). That file documents each one. Nothing that shapes a RESPONSE is
# patched.
#
# Both servers are pointed at the SAME package directory — the Python
# checkout's `stackunderflow/` — so the React bundle under test and the
# `data/models.toml` rate card are literally the same files. A difference can
# then only come from the server.
#
# ── The tree-skew hazard, stated because it is live ──────────────────────────
#
# `$PY_ROOT` defaults to `../StackUnderflow`, a DIFFERENT checkout on a
# different branch, because that is where the venv lives. So the reference this
# gate diffs against is that tree's Python, not the `rust` branch's copy of it.
# When a campaign fix lands Python-FIRST in this worktree (which is the ordered
# procedure for a contract change), the two trees disagree until the other
# checkout catches up, and the gate reports a divergence that is really a skew.
# Gate 4 is in exactly that state today over the DIV-021 resume tiebreaker.
#
# Two ways out, both one line:
#   STAX_PARITY_PY_ROOT=<this worktree>   diff against the branch's own Python
#   STAX_ENDPOINT_PKG_DIR=<...>           pin the package dir independently
#
# Check it before believing a divergence:
#   diff -u {../StackUnderflow,.}/stackunderflow/routes/<module>.py
#
# Usage
#   rust/endpoint-parity.sh                 # the whole matrix
#   rust/endpoint-parity.sh --only P-list   # id substring filter
#   rust/endpoint-parity.sh --keep-running  # leave both servers up to poke at
#
# Exit: 0 every case identical (known-open rows reported, not fatal), 1 on a
# divergence, 2 on a setup failure. ci.sh's gate 6 calls it with no arguments.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"

PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_ENDPOINT_PY_BIN:-$PY_ROOT/.venv/bin/python}"
# The package dir both servers read the React bundle and models.toml from.
PKG_DIR="${STAX_ENDPOINT_PKG_DIR:-$PY_ROOT/stackunderflow}"
HOME_DIR="${STAX_ENDPOINT_HOME:-$HERE/.parity-state/fresh}"
CASES="${STAX_ENDPOINT_CASES:-$HERE/parity/endpoint-cases.txt}"
DIFFS="${STAX_ENDPOINT_DIFFS:-$HERE/.parity-state/endpoint-diffs}"
LOGS="${STAX_ENDPOINT_LOGS:-$HERE/.parity-state/endpoint-logs}"
# Never hardcode a binary name: the CLI binary was renamed mid-campaign and the
# server's may be too. Override, don't edit.
RS_BIN="${STAX_ENDPOINT_PARITY_RS_BIN:-$HERE/target/release/stax-server}"
DIFFER="${STAX_ENDPOINT_DIFFER:-$HERE/target/release/stax-endpoint-parity}"

PY_PORT="${STAX_ENDPOINT_PY_PORT:-8097}"
RS_PORT="${STAX_ENDPOINT_RS_PORT:-8096}"

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep-running) KEEP=1; shift ;;
        -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
        *) echo "endpoint-parity: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [ "$PY_PORT" = 8095 ] || [ "$RS_PORT" = 8095 ]; then
    echo "endpoint-parity: :8095 is the maintainer's live server; refusing." >&2
    exit 2
fi

if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi

# ── setup checks, each with the fix in the message ───────────────────────────

if [ ! -x "$PY_BIN" ]; then
    echo "endpoint-parity: SETUP FAILURE — no Python interpreter at $PY_BIN" >&2
    echo "                 (set STAX_ENDPOINT_PY_BIN, or run ci.sh --skip-parity)" >&2
    exit 2
fi
if [ ! -f "$PKG_DIR/static/react/index.html" ]; then
    echo "endpoint-parity: SETUP FAILURE — no React bundle at $PKG_DIR/static/react/" >&2
    echo "                 The checked-in build IS the wave-5 oracle; do not rebuild it." >&2
    exit 2
fi
if [ ! -f "$HOME_DIR/store.db" ]; then
    echo "endpoint-parity: SETUP FAILURE — no store at $HOME_DIR/store.db" >&2
    echo "                 Build the state once with: rust/parity-cli.sh --build-state" >&2
    exit 2
fi
if ! "$PY_BIN" -c 'import uvicorn' 2>/dev/null; then
    echo "endpoint-parity: SETUP FAILURE — uvicorn is not installed in $PY_BIN" >&2
    exit 2
fi

echo "=== building the port (release) ==="
( cd "$HERE" && cargo build --release -p stax-server -p stax-parity ) || {
    echo "endpoint-parity: SETUP FAILURE — cargo build failed" >&2
    exit 2
}
[ -x "$RS_BIN" ]  || { echo "endpoint-parity: no binary at $RS_BIN" >&2;  exit 2; }
[ -x "$DIFFER" ]  || { echo "endpoint-parity: no differ at $DIFFER" >&2;  exit 2; }

# A busy port is a SETUP FAILURE, never a silent redirect. Found the hard way:
# the first version backgrounded each server inside a subshell, so `$!` was the
# subshell's pid and `kill` left uvicorn alive. The next run then bound nothing,
# talked to the PREVIOUS run's server — which still had a project selected from
# its last case — and reported a divergence that did not exist. A differ that
# can quietly address the wrong process is worse than no differ.
#
# The probe CONNECTS rather than binds. A bind test is wrong here: the port a
# previous run just released sits in TIME_WAIT, a bare bind refuses it, and both
# servers set SO_REUSEADDR and would have started perfectly well — so the bind
# test reports "busy" for a port nothing is listening on. "Can I reach a server
# there?" is the question actually being asked.
port_busy() {
    "$PY_BIN" - "$1" <<'PY'
import socket, sys
try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=1):
        sys.exit(0)   # something is listening
except OSError:
    sys.exit(1)       # nothing there
PY
}
for _port in "$PY_PORT" "$RS_PORT"; do
    if port_busy "$_port"; then
        echo "endpoint-parity: SETUP FAILURE — :$_port is already in use." >&2
        echo "                 A stale harness server would be diffed instead of a" >&2
        echo "                 fresh one. Find it with: lsof -i :$_port" >&2
        exit 2
    fi
done

mkdir -p "$LOGS" "$DIFFS"
PY_LOG="$LOGS/python.log"
RS_LOG="$LOGS/rust.log"
: > "$PY_LOG"
: > "$RS_LOG"

PY_PID=""
RS_PID=""
cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo
        echo "  servers left running: python :$PY_PORT (pid $PY_PID), rust :$RS_PORT (pid $RS_PID)"
        return
    fi
    [ -n "$PY_PID" ] && kill "$PY_PID" 2>/dev/null
    [ -n "$RS_PID" ] && kill "$RS_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT INT TERM

# ── boot ─────────────────────────────────────────────────────────────────────
#
# Python first, always. It is the side that runs `schema.apply` on startup, so
# any migration the shared store still needs happens before the Rust reader
# looks at it — the same ordering rule gate 4 uses.

#
# `exec` inside each subshell is load-bearing: without it `$!` names the
# subshell, `kill` reaps that, and the server it spawned outlives the run. See
# the port check above for what that cost.

echo "=== booting python (uvicorn) on :$PY_PORT ==="
(
    cd "$PY_ROOT" || exit 1
    export STACKUNDERFLOW_HOME="$HOME_DIR"
    export PYTHONPATH="$HERE/parity${PYTHONPATH:+:$PYTHONPATH}"
    exec "$PY_BIN" -m uvicorn pyserver:app \
        --host 127.0.0.1 --port "$PY_PORT" \
        --log-level warning --no-access-log
) >>"$PY_LOG" 2>&1 &
PY_PID=$!

echo "=== booting rust (stax-server) on :$RS_PORT ==="
(
    export STACKUNDERFLOW_HOME="$HOME_DIR"
    exec "$RS_BIN" --host 127.0.0.1 --port "$RS_PORT" \
        --data-dir "$HOME_DIR" --package-dir "$PKG_DIR"
) >>"$RS_LOG" 2>&1 &
RS_PID=$!

# ── diff ─────────────────────────────────────────────────────────────────────

DIFFER_ARGS=(--cases "$CASES" --py-port "$PY_PORT" --rs-port "$RS_PORT" --diffs "$DIFFS")
[ -n "$ONLY" ] && DIFFER_ARGS+=(--only "$ONLY")

"$DIFFER" "${DIFFER_ARGS[@]}"
RC=$?

if [ "$RC" != 0 ]; then
    echo
    echo "  server logs: $PY_LOG"
    echo "               $RS_LOG"
    if [ "$RC" = 2 ]; then
        echo "  (exit 2 = harness failure: a server did not answer. The logs above say why.)"
    fi
fi
exit "$RC"

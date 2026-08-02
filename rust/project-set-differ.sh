#!/usr/bin/env bash
# `POST /api/project` — the two-home procedure `rust/PROJECT-SET-DIFFER.md`
# specifies. RS-5-095 / DIV-341.
#
# The matrix cannot hold this endpoint's SUCCESS leg. Its mutation is
# process-global server state (`deps.current_project_path` /
# `deps.current_log_path`), not a file, so wave 8's `@home[:SEED]` mechanism —
# give each side its own tree, diff the trees — has nothing to compare. Worse,
# a `!` row would still FIRE the request (DIV-059 / DIV-078), re-pointing the
# reference's current project mid-run and changing what every later row means
# on one side only. That is finding 13 of `ARCHITECT-STATE.md` verbatim, and it
# cost this campaign a false divergence once already.
#
# So: two servers, TWO homes, six requests each, and the two transcripts are
# diffed. The homes are independent copies of the same fresh state, which is the
# one structural difference from `endpoint-parity.sh`.
#
# Ports :8100 / :8101 — :8095 is NEVER bound (it is the maintainer's live
# server) and :8096/:8097 belong to the matrix.
#
# Usage
#   rust/project-set-differ.sh
#   rust/project-set-differ.sh --keep-running
#
# Exit: 0 the transcripts are byte-identical, 1 they are not, 2 a setup failure.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"

PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_ENDPOINT_PY_BIN:-$PY_ROOT/.venv/bin/python}"
PKG_DIR="${STAX_ENDPOINT_PKG_DIR:-$PY_ROOT/stackunderflow}"
SEED="${STAX_PROJECT_SET_SEED:-$HERE/.parity-state/fresh}"
OUT="${STAX_PROJECT_SET_OUT:-$HERE/.parity-state/project-set}"
RS_BIN="${STAX_ENDPOINT_PARITY_RS_BIN:-$HERE/target/release/stax-server}"

PY_PORT="${STAX_PROJECT_SET_PY_PORT:-8100}"
RS_PORT="${STAX_PROJECT_SET_RS_PORT:-8101}"

# The two filesystem arguments the procedure's table calls for. Both must be
# REAL directories — that is precisely why neither can be a matrix row: step 4
# needs a directory that exists and has no claude logs, which no throwaway home
# can supply.
WITH_LOGS="${STAX_PROJECT_SET_WITH_LOGS:-$PY_ROOT}"
NO_LOGS="${STAX_PROJECT_SET_NO_LOGS:-$REPO_ROOT}"

KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --keep-running) KEEP=1; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "project-set-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [ "$PY_PORT" = 8095 ] || [ "$RS_PORT" = 8095 ]; then
    echo "project-set-differ: :8095 is the maintainer's live server; refusing." >&2
    exit 2
fi

[ -x "$PY_BIN" ] || { echo "project-set-differ: SETUP FAILURE — no interpreter at $PY_BIN" >&2; exit 2; }
[ -x "$RS_BIN" ] || { echo "project-set-differ: SETUP FAILURE — no binary at $RS_BIN (cargo build --release -p stax-server)" >&2; exit 2; }
[ -f "$SEED/store.db" ] || { echo "project-set-differ: SETUP FAILURE — no seed store at $SEED/store.db" >&2; exit 2; }
[ -d "$WITH_LOGS" ] || { echo "project-set-differ: SETUP FAILURE — $WITH_LOGS is not a directory" >&2; exit 2; }
[ -d "$NO_LOGS" ]   || { echo "project-set-differ: SETUP FAILURE — $NO_LOGS is not a directory" >&2; exit 2; }

# Same determinism pins gate 6 exports, same reasons (DIV-195).
export PYTHONHASHSEED=0
export LC_ALL=C LANG=C TZ=UTC PYTHONIOENCODING=utf-8

# ── 0. two independent copies of the fresh state ─────────────────────────────
PY_HOME="$HERE/.parity-state/proj-py"
RS_HOME="$HERE/.parity-state/proj-rs"
rm -rf "$PY_HOME" "$RS_HOME"
cp -a "$SEED" "$PY_HOME"
cp -a "$SEED" "$RS_HOME"
mkdir -p "$OUT"

port_busy() {
    "$PY_BIN" - "$1" <<'PY'
import socket, sys
try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=1):
        sys.exit(0)
except OSError:
    sys.exit(1)
PY
}
for _port in "$PY_PORT" "$RS_PORT"; do
    if port_busy "$_port"; then
        echo "project-set-differ: SETUP FAILURE — :$_port is already in use." >&2
        exit 2
    fi
done

PY_PID=""
RS_PID=""
cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo "  servers left running: python :$PY_PORT (pid $PY_PID), rust :$RS_PORT (pid $RS_PID)"
        return
    fi
    [ -n "$PY_PID" ] && kill "$PY_PID" 2>/dev/null
    [ -n "$RS_PID" ] && kill "$RS_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT INT TERM

# `exec` inside the subshell — without it `$!` names the subshell and the server
# it spawned outlives the run (endpoint-parity.sh's header records what that
# cost the first time).
echo "=== booting python (uvicorn) on :$PY_PORT — home $PY_HOME ==="
(
    cd "$PY_ROOT" || exit 1
    export STACKUNDERFLOW_HOME="$PY_HOME"
    export PYTHONPATH="$HERE/parity${PYTHONPATH:+:$PYTHONPATH}"
    exec "$PY_BIN" -m uvicorn pyserver:app \
        --host 127.0.0.1 --port "$PY_PORT" --log-level warning --no-access-log
) >"$OUT/python-server.log" 2>&1 &
PY_PID=$!

echo "=== booting rust (stax-server) on :$RS_PORT — home $RS_HOME ==="
(
    export STACKUNDERFLOW_HOME="$RS_HOME"
    export STACKUNDERFLOW_DISABLE_WATCHER=1
    export STACKUNDERFLOW_DISABLE_LOCK=1
    exec "$RS_BIN" --host 127.0.0.1 --port "$RS_PORT" \
        --data-dir "$RS_HOME" --package-dir "$PKG_DIR"
) >"$OUT/rust-server.log" 2>&1 &
RS_PID=$!

# ── the six steps, over a raw socket ─────────────────────────────────────────
#
# Not an HTTP client library, for `parity/src/http.rs`'s reason: reqwest and
# friends decompress, follow redirects, retry and normalise headers, each of
# which can turn a real divergence into a green tick. A hand-run procedure earns
# no exemption from that.
run_side() {
    "$PY_BIN" - "$1" "$2" "$3" <<'PY'
import json, socket, sys, time

port, with_logs, no_logs = int(sys.argv[1]), sys.argv[2], sys.argv[3]


def request(method, path, body=None):
    payload = b"" if body is None else json.dumps(body).encode()
    head = f"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
    if body is not None:
        head += f"Content-Type: application/json\r\nContent-Length: {len(payload)}\r\n"
    raw = head.encode() + b"\r\n" + payload
    with socket.create_connection(("127.0.0.1", port), timeout=30) as sock:
        sock.sendall(raw)
        buf = b""
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
    head, _, body_bytes = buf.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = lines[0].split(" ", 2)[1]
    ctype = ""
    for line in lines[1:]:
        if line.lower().startswith("content-type:"):
            ctype = line.split(":", 1)[1].strip()
    return status, ctype, body_bytes


# Wait for the server to answer at all before the first measured request.
for _ in range(600):
    try:
        request("GET", "/api/health")
        break
    except OSError:
        time.sleep(0.05)

STEPS = [
    ("1  before-state          ", "GET", "/api/project", None),
    ("2  set, real project     ", "POST", "/api/project", {"project_path": with_logs}),
    ("3  the mutation          ", "GET", "/api/project", None),
    ("4  set, dir with no logs ", "POST", "/api/project", {"project_path": no_logs}),
    ("5  404 left it intact    ", "GET", "/api/project", None),
    ("6  idempotence           ", "POST", "/api/project", {"project_path": with_logs}),
]
for label, method, path, body in STEPS:
    status, ctype, payload = request(method, path, body)
    print(f"{label} {method} {path}")
    print(f"    status       {status}")
    print(f"    content-type {ctype}")
    print(f"    body         {payload.decode('utf-8')}")
PY
}

run_side "$PY_PORT" "$WITH_LOGS" "$NO_LOGS" >"$OUT/python.txt" 2>&1
PY_RC=$?
run_side "$RS_PORT" "$WITH_LOGS" "$NO_LOGS" >"$OUT/rust.txt" 2>&1
RS_RC=$?

if [ "$PY_RC" != 0 ] || [ "$RS_RC" != 0 ]; then
    echo "project-set-differ: SETUP FAILURE — a side did not complete (py=$PY_RC rs=$RS_RC)" >&2
    echo "  transcripts: $OUT/python.txt · $OUT/rust.txt" >&2
    exit 2
fi

echo
echo "=== POST /api/project — six steps, two homes ==="
cat "$OUT/python.txt"
echo
if diff -u "$OUT/python.txt" "$OUT/rust.txt" >"$OUT/transcript.diff"; then
    echo "PROJECT-SET differ: 6/6 steps IDENTICAL — $OUT/{python,rust}.txt"
    exit 0
fi
echo "PROJECT-SET differ: DIVERGENT"
cat "$OUT/transcript.diff"
exit 1

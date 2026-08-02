#!/usr/bin/env bash
# `memory embed` — the three legs the closed-port rows cannot reach.
#
# `parity/cases.txt` carries `T2-embed-*`: the no-endpoint leg, exit 1, stderr
# byte-identical. Everything past that guard needs something answering
# `GET /api/tags`, because `cli.py` probes BEFORE it looks for the index.
#
# This procedure supplies that something and nothing more:
# `parity/ollama_stub.py`, bound to 127.0.0.1, answering exactly two paths with
# a vector that is a pure function of the prompt. Both implementations therefore
# receive IDENTICAL numbers, and `embeddings.db` can be compared blob for blob —
# which is the whole point, because a real daemon's vectors depend on a model
# file and would make the two stores differ for a reason that is not the port's.
#
# ── the legs ────────────────────────────────────────────────────────────────
#
# 1. reachable + NO index      → "No search index at …", stderr, exit 1
# 2. reachable + index         → the real write: banner, progress lines, the
#                                "Done — N message(s) embedded." footer, and
#                                two `embeddings.db` files compared row by row
# 3. re-run                    → "0 embedded — …", nothing written (idempotent)
# 4. reachable + embed fails   → the same "0 embedded" hint by the OTHER route
#                                (a daemon with no model), and still no write
# 5. --batch smaller than the  → more than one loop iteration, so the "  … N
#    candidate count             embedded" progress lines actually appear
#
# ── ports ───────────────────────────────────────────────────────────────────
#
# `:8095` is the maintainer's live server and is never bound. `:8096`/`:8097`
# are the shared endpoint harness's and `:8098`/`:8099` belong to the refresh
# and etl-backfill procedures. This uses **`:8101`**, which nothing else does.
#
#   rust/memory-embed-differ.sh [--keep]
#
# Exit 0 when every leg is green, 1 on any divergence, 2 on a setup failure.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
SCRATCH="$HERE/.parity-state/memory-embed"
PORT="${STAX_EMBED_STUB_PORT:-8101}"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

for tool in "$PY_BIN" "$PY_INTERP" "$RS_BIN"; do
    [ -x "$tool" ] || { echo "memory-embed-differ: SETUP FAILURE — no $tool" >&2; exit 2; }
done
[ "$PORT" = 8095 ] && { echo "memory-embed-differ: refusing :8095" >&2; exit 2; }

rc=0
STUB_PID=""
note() { printf '  %s\n' "$*"; }
cleanup() { [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

start_stub() {   # start_stub [--fail-embed]
    cleanup; STUB_PID=""
    ( exec "$PY_INTERP" "$HERE/parity/ollama_stub.py" "$PORT" "$@" ) \
        >"$SCRATCH/stub.log" 2>&1 &
    STUB_PID=$!
    for _ in $(seq 1 40); do
        code="$(curl -s -m 2 -o /dev/null -w '%{http_code}' \
            "http://127.0.0.1:$PORT/api/tags" 2>/dev/null)"
        [ "$code" = 200 ] && return 0
        sleep 0.25
    done
    echo "memory-embed-differ: the stub never answered on :$PORT" >&2
    return 1
}

# `STACKUNDERFLOW_OLLAMA_URL` is the CLOUD slot in `_resolve_endpoints`, tried
# first and then falling back to `http://localhost:11434`. Pointing it at the
# stub is therefore enough to make `active_endpoint()` return the stub on a box
# that has no local daemon — and on a box that HAS one, the stub still wins,
# which is what keeps this procedure reproducible either way.
export STACKUNDERFLOW_OLLAMA_URL="http://127.0.0.1:$PORT"
# The embed model name goes into `embeddings.model` and into the "0 embedded"
# hint, so it is pinned rather than left to the machine's environment.
export STACKUNDERFLOW_EMBED_MODEL="stub-embed"
unset STACKUNDERFLOW_OLLAMA_API_KEY OLLAMA_API_KEY OLLAMA_URL 2>/dev/null || true

seed_index() {   # seed_index <home> <rows>
    "$PY_INTERP" - "$1/search_index.db" "$2" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
conn.execute("CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, content TEXT)")
conn.execute("DELETE FROM messages")
rows = int(sys.argv[2])
conn.executemany(
    "INSERT INTO messages (id, content) VALUES (?, ?)",
    # One blank and one whitespace-only row on purpose: they are candidates a
    # naive port would embed and the reference skips (`content[:2000].strip()`).
    [(i, f"message body number {i}") for i in range(1, rows + 1)]
    + [(rows + 1, ""), (rows + 2, "   \t\n ")],
)
conn.commit()
conn.close()
PY
}

prepare() {   # prepare <tier> [index-rows]
    local tier="$1" rows="${2:-}" side
    for side in py rs; do
        rm -rf "$SCRATCH/$tier-$side"; mkdir -p "$SCRATCH/$tier-$side"
        [ -n "$rows" ] && { seed_index "$SCRATCH/$tier-$side" "$rows" || return 1; }
    done
    if [ -n "$rows" ]; then
        local a b
        a="$(md5sum "$SCRATCH/$tier-py/search_index.db" | cut -d' ' -f1)"
        b="$(md5sum "$SCRATCH/$tier-rs/search_index.db" | cut -d' ' -f1)"
        # The two seeds are written by the SAME interpreter, so they are equal
        # by construction; asserted anyway, because an unequal start is how a
        # differ reports its own setup as a defect.
        [ "$a" = "$b" ] || { echo "  SEED MISMATCH $a $b" >&2; return 1; }
        note "seed $tier: $rows real rows + 2 blank, both copies $a"
    fi
}

run_leg() {   # run_leg <tier> <label> [argv…]
    local tier="$1" label="$2"; shift 2
    local out="$SCRATCH/$label"
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-py" \
        "$PY_BIN" memory embed "$@" >"$out-py.out" 2>"$out-py.err" </dev/null )
    local py_rc=$?
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-rs" \
        "$RS_BIN" memory embed "$@" >"$out-rs.out" 2>"$out-rs.err" </dev/null )
    local rs_rc=$?
    # The two homes are at DIFFERENT paths, and leg 1's message prints the
    # index path. Normalise the home prefix on both sides — scoped to that one
    # substitution, which is the only text the paths reach.
    local stream
    for stream in "$out-py.out" "$out-py.err"; do
        sed -i "s#$SCRATCH/$tier-py#<HOME>#g" "$stream"
    done
    for stream in "$out-rs.out" "$out-rs.err"; do
        sed -i "s#$SCRATCH/$tier-rs#<HOME>#g" "$stream"
    done
    local ok=1
    [ "$py_rc" = "$rs_rc" ] || { ok=0; note "EXIT $py_rc vs $rs_rc"; }
    cmp -s "$out-py.out" "$out-rs.out" || { ok=0; note "STDOUT DIVERGENT"
        diff -u "$out-py.out" "$out-rs.out" | sed 's/^/    /'; }
    cmp -s "$out-py.err" "$out-rs.err" || { ok=0; note "STDERR DIVERGENT"
        diff -u "$out-py.err" "$out-rs.err" | sed 's/^/    /'; }
    if [ "$ok" = 1 ]; then
        note "$label byte-identical (out $(wc -c <"$out-py.out")B, err $(wc -c <"$out-py.err")B), exit $py_rc"
    else
        note "$label FAILED"; rc=1
    fi
}

compare_vectors() {   # compare_vectors <tier> <label>
    local tier="$1" label="$2" side
    for side in py rs; do
        if [ -f "$SCRATCH/$tier-$side/embeddings.db" ]; then
            sqlite3 "$SCRATCH/$tier-$side/embeddings.db" \
              "SELECT message_id, model, dim, hex(vector) FROM embeddings
                 ORDER BY message_id, model;" >"$SCRATCH/$label-$side-vec.txt"
        else
            : >"$SCRATCH/$label-$side-vec.txt"
        fi
    done
    if diff -u "$SCRATCH/$label-py-vec.txt" "$SCRATCH/$label-rs-vec.txt" >/dev/null; then
        note "$label vectors IDENTICAL ($(wc -l <"$SCRATCH/$label-py-vec.txt") rows, blobs compared as hex)"
    else
        note "$label vectors DIVERGENT"
        diff -u "$SCRATCH/$label-py-vec.txt" "$SCRATCH/$label-rs-vec.txt" | head -20 | sed 's/^/    /'
        rc=1
    fi
    # And the index must be untouched: this verb reads it and writes only the
    # vector store. A port that wrote back into `search_index.db` would agree on
    # stdout and be wrong on disk.
    local a b
    a="$(md5sum "$SCRATCH/$tier-py/search_index.db" 2>/dev/null | cut -d' ' -f1)"
    b="$(md5sum "$SCRATCH/$tier-rs/search_index.db" 2>/dev/null | cut -d' ' -f1)"
    [ "$a" = "$b" ] && note "$label search_index.db unchanged on both sides ($a)" \
                    || { note "$label search_index.db DIVERGENT ($a vs $b)"; rc=1; }
}

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"
start_stub || exit 2
note "stub on 127.0.0.1:$PORT (loopback only, deterministic vectors)"

echo "=== leg 1: reachable daemon, NO search index ==="
prepare noindex || exit 2
run_leg noindex noindex

echo "=== leg 2: the real write ==="
prepare write 12 || exit 2
run_leg write write-first
compare_vectors write write-first
for side in py rs; do
    n="$(sqlite3 "$SCRATCH/write-$side/embeddings.db" "SELECT COUNT(*) FROM embeddings" 2>/dev/null || echo 0)"
    note "write-first vectors on $side: $n"
    [ "$n" = 12 ] || { note "expected 12 — the two blank rows must be SKIPPED"; rc=1; }
done

echo "=== leg 3: re-run — everything already vectorised ==="
run_leg write write-second
compare_vectors write write-second

echo "=== leg 4: --batch 5 over 12 candidates (three loop iterations) ==="
prepare batch 12 || exit 2
run_leg batch batch5 --batch 5
compare_vectors batch batch5

echo "=== leg 5: reachable daemon whose embed call FAILS ==="
start_stub --fail-embed || exit 2
prepare nomodel 4 || exit 2
run_leg nomodel nomodel
compare_vectors nomodel nomodel

echo
if [ "$rc" = 0 ]; then echo "memory-embed-differ: GREEN"; else echo "memory-embed-differ: DIVERGENT"; fi
[ "$KEEP" = 1 ] && echo "scratch kept at $SCRATCH"
exit "$rc"

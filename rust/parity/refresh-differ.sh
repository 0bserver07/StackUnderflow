#!/usr/bin/env bash
# The isolated `POST /api/refresh` differ. See `rust/REFRESH-DIFFER.md` for why
# this endpoint cannot live in `parity/endpoint-cases.txt`.
#
# Two servers, two SEPARATE copies of the state, one PINNED ingest source, and
# the REAL `stackunderflow.server:app` — not `parity/pyserver.py`, which
# replaces `run_ingest` with `return {}` and would make every comparison here
# vacuous (that is finding 2 of this run).
#
# Runs unattended and writes its verdict to .parity-state/refresh/REPORT.txt.
# :8095 is never bound; this uses :8098/:8099 so it can run beside gate 6.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="$HERE/.parity-state/refresh"
REPORT="$SCRATCH/REPORT.txt"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$HERE/../../StackUnderflow" && pwd)}"
PKG_DIR="$PY_ROOT/stackunderflow"
PY_BIN="$PY_ROOT/.venv/bin/python"
RS_BIN="$HERE/target/release/stax-server"
PY_PORT=8099
RS_PORT=8098

say() { echo "$@" | tee -a "$REPORT"; }

pkill -f "uvicorn stackunderflow.server:app" 2>/dev/null
pkill -f "stax-server --host 127.0.0.1 --port $RS_PORT" 2>/dev/null
sleep 2

mkdir -p "$SCRATCH"
: > "$REPORT"
say "=== POST /api/refresh — isolated differ, $(date -Is) ==="
say

# ── 0. pin the ingest source ────────────────────────────────────────────────
SLUG="$(cat "$SCRATCH/slug" 2>/dev/null || true)"
if [ -z "$SLUG" ] || [ ! -d "$SCRATCH/home/.claude/projects/$SLUG" ]; then
    say "FATAL: no pinned source at $SCRATCH/home — see REFRESH-DIFFER.md step 0"
    exit 2
fi
say "pinned source : $SLUG"
say "               $(ls "$SCRATCH/home/.claude/projects/$SLUG" | wc -l) file(s), \
$(du -sh "$SCRATCH/home/.claude/projects/$SLUG" | cut -f1)"

# ── 1. two independent state copies ─────────────────────────────────────────
# The seed is a SCHEMA-ONLY store built by the reference's own `store/schema.py`
# (RS-0-025 is unported, so Python is the only thing that can create one), not a
# copy of `.parity-state/fresh`. Two reasons, both learned by trying the copy
# first:
#
#   * 3.9 GB × 2 makes one Python `/api/refresh` exceed a 900 s probe timeout —
#     the pre/post `SELECT COUNT(*)` per file runs over a partitioned VIEW of
#     383,580 rows — so the run never reached its own assertions;
#   * every message in the pinned source is ALREADY in that store, so the cold
#     pass ingested zero rows and the strongest assertion in the procedure
#     ("the two writers wrote the same thing") was vacuously true.
#
# An empty store makes the cold pass a real ingest with real INSERTs, which is
# the thing actually under test.
SEED="$SCRATCH/seed.db"
if [ ! -f "$SEED" ]; then
    "$PY_BIN" - "$SEED" <<'SEEDPY'
import pathlib, sys
from stackunderflow.store import db, schema
p = pathlib.Path(sys.argv[1])
conn = db.connect(p); schema.apply(conn); conn.close()
SEEDPY
fi
for side in py rs; do
    rm -rf "${SCRATCH:?}/$side"; mkdir -p "$SCRATCH/$side"
    sqlite3 "$SEED" ".backup '$SCRATCH/$side/store.db'"
done
PY_MD5=$(md5sum "$SCRATCH/py/store.db" | cut -d' ' -f1)
RS_MD5=$(md5sum "$SCRATCH/rs/store.db" | cut -d' ' -f1)
say "start states  : $PY_MD5 / $RS_MD5  $([ "$PY_MD5" = "$RS_MD5" ] && echo IDENTICAL || echo MISMATCH)"
[ "$PY_MD5" = "$RS_MD5" ] || exit 2

# ── 2. boot ─────────────────────────────────────────────────────────────────
( cd "$PY_ROOT" && HOME="$SCRATCH/home" STACKUNDERFLOW_HOME="$SCRATCH/py" \
    STACKUNDERFLOW_DISABLE_WATCHER=1 STACKUNDERFLOW_DISABLE_LOCK=1 \
    exec "$PY_BIN" -m uvicorn stackunderflow.server:app --host 127.0.0.1 \
       --port "$PY_PORT" --log-level warning --no-access-log ) >"$SCRATCH/py.log" 2>&1 &
PY_PID=$!
( HOME="$SCRATCH/home" STACKUNDERFLOW_HOME="$SCRATCH/rs" \
  exec "$RS_BIN" --host 127.0.0.1 --port "$RS_PORT" \
       --data-dir "$SCRATCH/rs" --package-dir "$PKG_DIR" ) >"$SCRATCH/rs.log" 2>&1 &
RS_PID=$!
trap 'kill "$PY_PID" "$RS_PID" 2>/dev/null; wait 2>/dev/null' EXIT INT TERM

for _ in $(seq 1 60); do
    a=$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PY_PORT/api/health")
    b=$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$RS_PORT/api/health")
    [ "$a" = 200 ] && [ "$b" = 200 ] && break
    sleep 2
done
say "servers up    : python :$PY_PORT ($a)  rust :$RS_PORT ($b)"
[ "$a" = 200 ] && [ "$b" = 200 ] || exit 2

probe() {  # probe <label> <port> <body>
    local code
    code=$(curl -s -m 900 -o "$SCRATCH/$1.body" -w '%{http_code}' \
           -X POST "http://127.0.0.1:$2/api/refresh" \
           -H 'content-type: application/json' --data "$3")
    printf '%s' "$code"
}

counts() {  # counts <side>
    local out=""
    for t in projects sessions messages usage_events ingest_log; do
        out="$out$t=$(sqlite3 "$SCRATCH/$1/store.db" "SELECT COUNT(*) FROM $t" 2>/dev/null) "
    done
    printf '%s' "$out"
}

compare_bodies() {  # compare_bodies <case>
    "$PY_BIN" - "$SCRATCH/py-$1.body" "$SCRATCH/rs-$1.body" <<'PY'
import json, sys
a = json.load(open(sys.argv[1])); b = json.load(open(sys.argv[2]))
ta, tb = a.pop("refresh_time_ms", None), b.pop("refresh_time_ms", None)
ok = a == b and isinstance(ta, int) and ta >= 0 and isinstance(tb, int) and tb >= 0
print(("IDENTICAL modulo refresh_time_ms" if ok else "DIVERGENT"),
      f"(py {ta} ms / rs {tb} ms)")
if not ok:
    print("   py:", json.dumps(a, sort_keys=True))
    print("   rs:", json.dumps(b, sort_keys=True))
sys.exit(0 if ok else 1)
PY
}

# ── 3a. quiesce ─────────────────────────────────────────────────────────────
# The real lifespan starts a background ingest. Drive both sides to a fixed
# point first, so the COLD probe below measures the injected file and nothing
# else.
say
say "--- quiesce (drive both to a fixed point) ---"
for round in 1 2; do
    pc=$(probe "py-q$round" "$PY_PORT" '{}'); rc=$(probe "rs-q$round" "$RS_PORT" '{}')
    say "  round $round: python $pc  rust $rc"
done
say "  python : $(counts py)"
say "  rust   : $(counts rs)"
Q_PY=$(counts py); Q_RS=$(counts rs)
say "  quiesced states $([ "$Q_PY" = "$Q_RS" ] && echo MATCH || echo DIVERGENT)"

# ── 3b. inject a genuinely new session, then COLD ───────────────────────────
say
say "--- inject one new session into the pinned source ---"
"$PY_BIN" - "$SCRATCH/home/.claude/projects/$SLUG" <<'PY'
import json, pathlib, sys, uuid
d = pathlib.Path(sys.argv[1])
src = sorted(p for p in d.glob("*.jsonl") if not p.stem.startswith("parity-"))[0]
new_id = str(uuid.UUID(int=0xC0FFEE00C0FFEE00C0FFEE00C0FFEE00))
out, n = [], 0
for line in src.read_text(errors="replace").splitlines():
    if not line.strip():
        continue
    try:
        obj = json.loads(line)
    except ValueError:
        continue
    obj["sessionId"] = new_id
    if isinstance(obj.get("uuid"), str):
        obj["uuid"] = str(uuid.uuid5(uuid.NAMESPACE_URL, "parity/" + obj["uuid"]))
    if isinstance(obj.get("parentUuid"), str):
        obj["parentUuid"] = str(uuid.uuid5(uuid.NAMESPACE_URL, "parity/" + obj["parentUuid"]))
    out.append(json.dumps(obj)); n += 1
(d / f"{new_id}.jsonl").write_text("\n".join(out) + "\n")
print(f"  wrote {new_id}.jsonl — {n} records rebased off {src.name}")
PY
say "$(tail -1 "$SCRATCH/../refresh/REPORT.txt" >/dev/null; true)"

say
say "--- (b) COLD: the pass that must actually ingest ---"
PC=$(probe py-cold "$PY_PORT" '{}'); RC=$(probe rs-cold "$RS_PORT" '{}')
say "  status: python $PC  rust $RC"
say "  body  : $(compare_bodies cold)"; COLD_RC=$?

say
say "--- (c) WARM: idempotence ---"
PC=$(probe py-warm "$PY_PORT" '{}'); RC=$(probe rs-warm "$RS_PORT" '{}')
say "  status: python $PC  rust $RC"
say "  body  : $(compare_bodies warm)"; WARM_RC=$?

say
say "--- (d) PER-PROJECT branch ---"
for p in "$PY_PORT" "$RS_PORT"; do
    curl -s -m 60 -o /dev/null -X POST "http://127.0.0.1:$p/api/project-by-dir" \
         -H 'content-type: application/json' --data "{\"dir_name\": \"$SLUG\"}"
done
PC=$(probe py-proj "$PY_PORT" '{}'); RC=$(probe rs-proj "$RS_PORT" '{}')
say "  status: python $PC  rust $RC"
say "  body  : $(compare_bodies proj)"; PROJ_RC=$?

# ── 3c. validation probes ───────────────────────────────────────────────────
say
say "--- (a) VALIDATION (never reaches the ingest pass) ---"
V_RC=0
for name in empty:'' list:'[]' num:'5' junk:'nope' str:'"x"'; do
    label="${name%%:*}"; body="${name#*:}"
    pc=$(probe "py-422-$label" "$PY_PORT" "$body"); rc=$(probe "rs-422-$label" "$RS_PORT" "$body")
    if diff -q "$SCRATCH/py-422-$label.body" "$SCRATCH/rs-422-$label.body" >/dev/null 2>&1 \
       && [ "$pc" = "$rc" ]; then
        say "  $label: $pc/$rc IDENTICAL"
    else
        V_RC=1
        say "  $label: $pc/$rc DIVERGENT"
        say "     py: $(cat "$SCRATCH/py-422-$label.body")"
        say "     rs: $(cat "$SCRATCH/rs-422-$label.body")"
    fi
done

# ── 4. the real assertion: the stores ───────────────────────────────────────
say
say "--- store row counts after the whole run ---"
STORE_RC=0
for t in projects sessions messages usage_events ingest_log; do
    a=$(sqlite3 "$SCRATCH/py/store.db" "SELECT COUNT(*) FROM $t")
    b=$(sqlite3 "$SCRATCH/rs/store.db" "SELECT COUNT(*) FROM $t")
    if [ "$a" = "$b" ]; then s=OK; else s=DIVERGENT; STORE_RC=1; fi
    say "$(printf '  %-14s python=%-10s rust=%-10s %s' "$t" "$a" "$b" "$s")"
done

say
say "=== VERDICT ==="
say "  cold body        : $([ $COLD_RC = 0 ] && echo PASS || echo FAIL)"
say "  warm body        : $([ $WARM_RC = 0 ] && echo PASS || echo FAIL)"
say "  per-project body : $([ $PROJ_RC = 0 ] && echo PASS || echo FAIL)"
say "  validation 422   : $([ $V_RC = 0 ] && echo PASS || echo FAIL)"
say "  store row counts : $([ $STORE_RC = 0 ] && echo PASS || echo FAIL)"
RC=$(( COLD_RC | WARM_RC | PROJ_RC | V_RC | STORE_RC ))
say "  overall          : $([ $RC = 0 ] && echo GREEN || echo RED)"
exit "$RC"

#!/usr/bin/env bash
# The migration-runner differ — wave 7's gate for `store/schema.py`.
#
# The claim being tested is narrow and total: **a store migrated by the Rust
# runner and a store migrated by the Python runner are the same store.** Not
# "both reach v30" — the same `sqlite_master`, in the same creation order, with
# the same generated DDL text, and (where a fixture put rows in) the same rows.
#
# Why that bar and not `PRAGMA user_version`:
#
#   * v008 GENERATES its schema — partition tables, a UNION-ALL view, and an
#     INSTEAD OF trigger whose whole body is built by string concatenation. Two
#     runners can agree on every version number and still write different
#     trigger text, and the difference only surfaces months later in a backup
#     diff or a `.dump`.
#   * `sqlite_master` ORDER is creation order. Sorting the comparison would hide
#     a runner that reaches the right objects by a different route.
#
# Both stores are dumped by ONE reader (`parity/schema_states.py dump`), never by
# the engine that wrote them — a store described by its own writer is a
# tautology. That reader is CPython's `sqlite3`, which is also the Python
# implementation's engine; the asymmetry is deliberate and is the conservative
# direction (the Rust store is read by the *other* side's engine, so a
# Rust-only encoding quirk cannot hide).
#
# ── the state matrix ─────────────────────────────────────────────────────────
#
#   empty            an empty file → v30, both sides. The DDL half.
#   mid:N            Python takes a store to vN, then EACH side finishes it.
#                    N walks every version boundary. This is the half that
#                    exercises "resume", which is the only thing a real user's
#                    store ever does.
#   partial:N        vN-1 plus N's ALTER, with `user_version` left behind — the
#                    crashed-mid-migration state `_ADD_COLUMN_GUARDS` exists
#                    for, and the one branch a from-empty run can never reach.
#   data:FIXTURE:N   rows seeded at vN, then finished by each side. v008's
#                    routing and v005's redistribute are data, not DDL, so the
#                    dump includes rows.
#
# ── DIV-302: the wall clock is in the schema ─────────────────────────────────
#
# v008 names its bootstrap partition `messages_$(date -u +%Y%m)` when the store
# has no messages. Two stores created either side of a month boundary therefore
# have legitimately different schemas. The month is recorded before and after
# every run and a rollover ABORTS the differ, rather than being reported as a
# divergence the harness caused itself.
#
# Usage
#   rust/schema-differ.sh              # the whole matrix
#   rust/schema-differ.sh --only mid   # id substring filter
#   rust/schema-differ.sh --keep       # leave the scratch stores for inspection
#
# Exit: 0 when every state is byte-identical, 1 on any divergence, 2 on setup.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
STATES="$HERE/parity/schema_states.py"
WORK="${STAX_SCHEMA_WORK:-$HERE/.parity-state/schema}"
RS_BIN="${STAX_SCHEMA_RS_BIN:-$HERE/target/release/stax-schema-apply}"

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,52p' "$0"; exit 0 ;;
        *) echo "schema-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[ -x "$PY_INTERP" ] || PY_INTERP="python3"
if ! "$PY_INTERP" -c 'import sqlite3' 2>/dev/null; then
    echo "schema-differ: SETUP FAILURE — no usable python at $PY_INTERP" >&2
    exit 2
fi

if [ ! -x "$RS_BIN" ]; then
    echo "schema-differ: building the release binary"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-core --bin stax-schema-apply --quiet ) || exit 2
fi

# Determinism, and the reference must be THIS worktree's Python (gate 0's
# split-brain lesson — the venv's editable install names another worktree).
export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

MONTH_BEFORE="$(date -u +%Y%m)"

rm -rf "$WORK"
mkdir -p "$WORK/diffs"

pass=0; fail=0
failed_ids=()

py() { "$PY_INTERP" "$STATES" "$@"; }

# Remove a store and the WAL/SHM sidecars WAL mode leaves next to it.
drop_store() { rm -f "$1" "$1-wal" "$1-shm"; }

# One state: build the substrate ONCE, copy it, let each side finish its own
# copy, dump both with the one reader, diff.
#
#   $1 id   $2 seed-fixture ("" for none)   $3 stop version ("" for empty)
#   $4 "partial" version ("" for none)      $5 "--data" or ""
run_state() {
    local id="$1" fixture="$2" stop="$3" partial="$4" data="$5"
    if [ -n "$ONLY" ]; then
        case "$id" in *"$ONLY"*) ;; *) return 0 ;; esac
    fi

    local base="$WORK/$id"
    local sub="$base.base.db" py_db="$base.py.db" rs_db="$base.rs.db"
    drop_store "$sub"; drop_store "$py_db"; drop_store "$rs_db"

    if [ -n "$stop" ]; then
        py apply "$sub" --to "$stop" >/dev/null || { setup_fail "$id" "apply --to $stop"; return 1; }
    fi
    if [ -n "$fixture" ] && [ "$fixture" != "empty" ]; then
        py seed "$sub" "$fixture" || { setup_fail "$id" "seed $fixture"; return 1; }
    fi
    if [ -n "$partial" ]; then
        py partial "$sub" "$partial" || { setup_fail "$id" "partial $partial"; return 1; }
    fi

    # The substrate is checkpointed into the main file before it is copied —
    # otherwise each side would inherit a different WAL tail and "the same
    # starting state" would be a lie.
    "$PY_INTERP" - "$sub" <<'PY' || true
import sqlite3, sys
if __import__("os").path.exists(sys.argv[1]):
    c = sqlite3.connect(sys.argv[1])
    c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    c.close()
PY
    [ -f "$sub" ] && cp "$sub" "$py_db" && cp "$sub" "$rs_db"

    local py_out rs_out py_rc rs_rc
    py_out="$(py apply "$py_db" 2>&1)"; py_rc=$?
    rs_out="$("$RS_BIN" "$rs_db" 2>&1)"; rs_rc=$?

    local ok=1
    if [ "$py_rc" != "$rs_rc" ]; then ok=0; fi

    local py_dump rs_dump
    py_dump="$WORK/diffs/$id.py.txt"
    rs_dump="$WORK/diffs/$id.rs.txt"
    py dump "$py_db" $data >"$py_dump" 2>&1
    py dump "$rs_db" $data >"$rs_dump" 2>&1
    cmp -s "$py_dump" "$rs_dump" || ok=0

    if [ "$ok" = 1 ]; then
        pass=$((pass + 1))
        printf '  ok    %-34s %s\n' "$id" "$(head -1 "$py_dump")"
        [ "$KEEP" = 1 ] || { drop_store "$sub"; drop_store "$py_db"; drop_store "$rs_db"; rm -f "$py_dump" "$rs_dump"; }
        return 0
    fi

    fail=$((fail + 1)); failed_ids+=("$id")
    {
        printf '=== %s ===\n' "$id"
        printf 'python: rc=%s %s\n' "$py_rc" "$py_out"
        printf 'rust:   rc=%s %s\n\n' "$rs_rc" "$rs_out"
        diff -u "$py_dump" "$rs_dump" | head -160
    } >"$WORK/diffs/$id.diff"
    printf '  FAIL  %-34s see %s\n' "$id" "$WORK/diffs/$id.diff"
    return 1
}

setup_fail() {
    fail=$((fail + 1)); failed_ids+=("$1")
    printf '  SETUP %-34s (%s)\n' "$1" "$2"
}

echo "schema-differ: python=$PY_INTERP"
echo "               rust=$RS_BIN"
echo "               work=$WORK"
echo "               utc month=$MONTH_BEFORE (DIV-302 guard armed)"

printf '\n=== from empty ===\n'
run_state "empty" "" "" "" ""

printf '\n=== mid-version (python builds vN, each side finishes) ===\n'
for n in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 16 17 18 19 20 21 22 23 24 25 26 27 28 29; do
    run_state "mid-v$(printf '%02d' "$n")" "" "$n" "" ""
done

printf '\n=== partial application (DDL in, user_version behind) ===\n'
for n in $(py list-partials); do
    run_state "partial-v$(printf '%02d' "$n")" "" "$((n - 1))" "$n" ""
done

printf '\n=== data migrations (rows seeded, rows compared) ===\n'
run_state "data-v008-mixed"  "messages-mixed" 7 "" "--data"
run_state "data-v008-from-v1" "messages-mixed" 1 "" "--data"
run_state "data-v005-cursor"  "cursor-legacy"  4 "" "--data"
run_state "data-v005-from-v1" "cursor-legacy"  1 "" "--data"

MONTH_AFTER="$(date -u +%Y%m)"
if [ "$MONTH_BEFORE" != "$MONTH_AFTER" ]; then
    echo
    echo "schema-differ: ABORT — the UTC month rolled over mid-run"
    echo "               ($MONTH_BEFORE -> $MONTH_AFTER). v008 names its bootstrap"
    echo "               partition from the wall clock (DIV-302), so this run's"
    echo "               result is the harness's artefact, not a measurement."
    exit 2
fi

total=$((pass + fail))
printf '\n=== schema tally ===\n'
printf 'states: %s   identical: %s   DIVERGENT: %s\n' "$total" "$pass" "$fail"
if [ "$fail" -gt 0 ]; then
    printf '\ndivergent:\n'
    printf '  %s\n' "${failed_ids[@]}"
    printf '\ndiffs: %s\n' "$WORK/diffs"
    exit 1
fi
printf 'every state byte-identical, sqlite_master order included.\n'
exit 0

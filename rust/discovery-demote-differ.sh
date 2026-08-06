#!/usr/bin/env bash
# `discovery demote-uncited` without `--dry-run` — the isolated writer proof.
#
# The dry legs are rows in `parity/cases.txt` (`T2-disc-dem-dry-*`). This one
# cannot be, for three reasons, and the third is the interesting one:
#
# 1. It flips `demoted = 1` on ~100 rows of the shared parity state, so it would
#    change the answer of every `T2-disc-*` row after it and of the ranking term
#    the discovery verbs read. DIV-059, again.
# 2. Both sides rewrite `store.db`, so `diff -r` on a `@home` pair reports
#    "Binary files differ" on page 1's `SQLITE_VERSION_NUMBER` (DIV-257) before
#    any behaviour is compared.
# 3. **It is not idempotent in the way a `@home` row needs.** The second run
#    finds nothing, because the first run demoted everything — so a row would
#    prove the SECOND run's empty output and never the first run's write. The
#    seed has to be private and fresh for the write to be observable at all.
#
# ── the corpus is the deliverable ────────────────────────────────────────────
#
# `demote_candidates` has four predicates and a threshold pair, and wave 6's law
# is that every constant needs a row that crosses it. The seed below carries one
# row per predicate, on each side of it:
#
#   | row                  | loaded | cited | demoted | first_loaded | expected |
#   |----------------------|-------:|------:|--------:|--------------|----------|
#   | hot-uncited-old      |     30 |     0 |       0 | 2020         | CANDIDATE|
#   | hotter-uncited-old   |    120 |     0 |       0 | 2020         | CANDIDATE (first — DESC) |
#   | at-the-load-floor    |     20 |     0 |       0 | 2020         | CANDIDATE (`>=`) |
#   | one-below-the-floor  |     19 |     0 |       0 | 2020         | out      |
#   | cited-once           |    500 |     1 |       0 | 2020         | out      |
#   | already-demoted      |    500 |     0 |       1 | 2020         | out      |
#   | null-first-loaded    |    500 |     0 |       0 | NULL         | out      |
#   | loaded-today         |    500 |     0 |       0 | now          | out (age)|
#
# Two candidates share `loaded_count = 30`/`20` under two different commands so
# the `ORDER BY loaded_count DESC, command, session_id` tie-break is crossed as
# well, and the `--min-loads` boundary is exercised from both sides in leg 3.
#
# ── usage ───────────────────────────────────────────────────────────────────
#
#   rust/discovery-demote-differ.sh [--keep]
#
# Exit 0 when every leg is green, 1 on any divergence, 2 on a setup failure.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
SEED_SRC="${STAX_DEMOTE_SEED:-$HERE/.parity-state/refresh/py/store.db}"
SCRATCH="$HERE/.parity-state/discovery-demote"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

for tool in "$PY_BIN" "$PY_INTERP" "$RS_BIN"; do
    [ -x "$tool" ] || { echo "discovery-demote-differ: SETUP FAILURE — no $tool" >&2; exit 2; }
done
[ -f "$SEED_SRC" ] || { echo "discovery-demote-differ: SETUP FAILURE — no seed at $SEED_SRC" >&2; exit 2; }

rc=0
note() { printf '  %s\n' "$*"; }

build_seed() {
    rm -f "$SCRATCH/seed.db"
    sqlite3 "$SEED_SRC" ".backup '$SCRATCH/seed.db'" || return 1
    # `DELETE` first: the seed store is a real one and may already carry rows,
    # and a corpus that is partly inherited is a corpus nobody can reason about.
    sqlite3 "$SCRATCH/seed.db" <<'SQL' || return 1
DELETE FROM discovery_telemetry;
INSERT INTO discovery_telemetry
      (command, session_id, loaded_count, cited_count,
       first_loaded_ts, last_loaded_ts, last_cited_ts, demoted)
VALUES
  ('search_past_decisions',      'hotter-uncited-old', 120, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-05T00:00:00+00:00', NULL, 0),
  ('search_past_decisions',      'hot-uncited-old',     30, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-04T00:00:00+00:00', NULL, 0),
  ('find_sessions_in_path',      'hot-uncited-old',     30, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-04T00:00:00+00:00', NULL, 0),
  ('find_sessions_touching_file','at-the-load-floor',   20, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-03T00:00:00+00:00', NULL, 0),
  ('find_sessions_in_path',      'one-below-the-floor', 19, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-03T00:00:00+00:00', NULL, 0),
  ('find_sessions_in_path',      'cited-once',         500, 1,
   '2020-01-01T00:00:00+00:00', '2026-01-02T00:00:00+00:00',
   '2026-01-02T01:00:00+00:00', 0),
  ('find_sessions_in_path',      'already-demoted',    500, 0,
   '2020-01-01T00:00:00+00:00', '2026-01-02T00:00:00+00:00', NULL, 1),
  ('find_sessions_in_path',      'null-first-loaded',  500, 0,
   NULL, '2026-01-02T00:00:00+00:00', NULL, 0),
  ('find_sessions_in_path',      'loaded-today',       500, 0,
   strftime('%Y-%m-%dT%H:%M:%S+00:00','now'),
   strftime('%Y-%m-%dT%H:%M:%S+00:00','now'), NULL, 0);
SQL
}

prepare() {   # prepare <tier>
    local tier="$1" side
    for side in py rs; do
        rm -rf "$SCRATCH/$tier-$side"; mkdir -p "$SCRATCH/$tier-$side"
        cp "$SCRATCH/seed.db" "$SCRATCH/$tier-$side/store.db" || return 1
    done
    local a b
    a="$(md5sum "$SCRATCH/$tier-py/store.db" | cut -d' ' -f1)"
    b="$(md5sum "$SCRATCH/$tier-rs/store.db" | cut -d' ' -f1)"
    [ "$a" = "$b" ] || { echo "  SEED MISMATCH $a $b" >&2; return 1; }
    note "seed $tier: both copies $a"
}

run_leg() {   # run_leg <tier> <label> [argv…]
    local tier="$1" label="$2"; shift 2
    local out="$SCRATCH/$label"
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-py" \
        "$PY_BIN" discovery demote-uncited "$@" >"$out-py.out" 2>"$out-py.err" </dev/null )
    local py_rc=$?
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-rs" \
        "$RS_BIN" discovery demote-uncited "$@" >"$out-rs.out" 2>"$out-rs.err" </dev/null )
    local rs_rc=$?
    local ok=1
    [ "$py_rc" = "$rs_rc" ] || { ok=0; note "EXIT $py_rc vs $rs_rc"; }
    cmp -s "$out-py.out" "$out-rs.out" || { ok=0; note "STDOUT DIVERGENT"
        diff -u "$out-py.out" "$out-rs.out" | sed 's/^/    /'; }
    cmp -s "$out-py.err" "$out-rs.err" || { ok=0; note "STDERR DIVERGENT"
        diff -u "$out-py.err" "$out-rs.err" | sed 's/^/    /'; }
    if [ "$ok" = 1 ]; then
        note "$label stdout byte-identical ($(wc -c <"$out-py.out") B), exit $py_rc"
    else
        note "$label FAILED"; rc=1
    fi
}

compare_table() {   # compare_table <tier> <label>
    local tier="$1" label="$2" side
    for side in py rs; do
        sqlite3 "$SCRATCH/$tier-$side/store.db" \
          "SELECT command, session_id, loaded_count, cited_count,
                  COALESCE(first_loaded_ts,'-'), COALESCE(last_loaded_ts,'-'),
                  COALESCE(last_cited_ts,'-'), demoted
             FROM discovery_telemetry ORDER BY command, session_id;" \
          >"$SCRATCH/$label-$side-rows.txt"
    done
    if diff -u "$SCRATCH/$label-py-rows.txt" "$SCRATCH/$label-rs-rows.txt" >/dev/null; then
        note "$label table IDENTICAL ($(wc -l <"$SCRATCH/$label-py-rows.txt") rows)"
    else
        note "$label table DIVERGENT"
        diff -u "$SCRATCH/$label-py-rows.txt" "$SCRATCH/$label-rs-rows.txt" | sed 's/^/    /'
        rc=1
    fi
    # And the whole store, since a verb that touched anything else would be a
    # defect no telemetry dump could see.
    if "$PY_INTERP" "$HERE/parity/etl_store_diff.py" \
            "$SCRATCH/$tier-py" "$SCRATCH/$tier-rs" >/dev/null 2>&1; then
        note "$label whole store IDENTICAL"
    else
        note "$label whole store DIVERGENT"
        "$PY_INTERP" "$HERE/parity/etl_store_diff.py" \
            "$SCRATCH/$tier-py" "$SCRATCH/$tier-rs" 2>&1 | sed 's/^/    /'
        rc=1
    fi
}

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"
build_seed || exit 2

echo "=== leg 1: the real write at the defaults ==="
prepare apply || exit 2
run_leg apply apply-first
compare_table apply apply-first
# The write must have HAPPENED. A port that printed "Demoted 4" and committed
# nothing would agree with the reference on stdout and be wrong on disk — and
# the `demoted` column is the only place that shows.
for side in py rs; do
    n="$(sqlite3 "$SCRATCH/apply-$side/store.db" \
        "SELECT COUNT(*) FROM discovery_telemetry WHERE demoted = 1")"
    note "apply-first demoted rows on $side: $n"
    [ "$n" -ge 5 ] || { note "expected the four candidates plus the pre-demoted row"; rc=1; }
done

echo "=== leg 2: re-run — idempotent, and the SECOND run finds nothing ==="
run_leg apply apply-second
compare_table apply apply-second

echo "=== leg 3: --min-loads on both sides of the boundary, from a fresh seed ==="
prepare floor || exit 2
run_leg floor floor-19 --min-loads 19
compare_table floor floor-19
prepare floor2 || exit 2
run_leg floor2 floor-1000 --min-loads 1000
compare_table floor2 floor-1000

echo "=== leg 4: --format json on a real write ==="
prepare json || exit 2
run_leg json json-apply --format json
compare_table json json-apply

echo
if [ "$rc" = 0 ]; then echo "discovery-demote-differ: GREEN"; else echo "discovery-demote-differ: DIVERGENT"; fi
[ "$KEEP" = 1 ] && echo "scratch kept at $SCRATCH"
exit "$rc"

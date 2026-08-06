#!/usr/bin/env bash
# `stackunderflow etl backfill` vs `stax etl backfill` — the isolated procedure.
#
# `rust/parity-cli.sh` cannot run this verb and a comment in the case file would
# not save it. This script is the proof that can. It is the CLI sibling of
# `rust/ETL-BACKFILL-DIFFER.md` (the HTTP endpoint's procedure) and it inherits
# that document's reasoning; what is written here is only what differs.
#
# ── why it cannot be a matrix row (four reasons) ─────────────────────────────
#
# 1. IT WRITES THE TABLES EVERY OTHER ROW READS. `--force` is `DELETE FROM
#    usage_events`, `DELETE FROM mart_watermark`, `rebuild_from_scratch` on all
#    eight marts, then a full normalize pass. On the shared parity states that
#    would change the answer of every `T3-*`, `T5-*` and `T2-etl-status` row
#    after it — the DIV-059 lesson (a `!` softens the verdict, not the request)
#    at 232,347 events.
# 2. IT PRINTS A WALL CLOCK. `f"  duration:                   {…:.3f}s"` is
#    `time.perf_counter()` either side of the run. Two processes never agree.
#    Compared for SHAPE here, never for equality — the same ruling
#    `refresh_time_ms` and `scanned_at` took.
# 3. BOTH SIDES REWRITE `store.db`. `diff -r` on a `@home` pair therefore
#    reports "Binary files differ" before any behaviour is compared: page 1's
#    `SQLITE_VERSION_NUMBER` at offset 96 is stamped by whoever wrote it
#    (3053001 for this host's CPython, 3053002 for rusqlite's bundled build).
#    That is DIV-257, and `parity/sqlite_header_diff.py` is the STRICTER answer
#    to it: same length, offsets 96..99 the only differing bytes, both values
#    recognised 3.53.x, `sqlite_master` identical, and every row of every table
#    identical.
# 4. A SECOND RUN IS NOT A SECOND SAMPLE. The verb is idempotent by design
#    (`uniq_events_msg`), so re-running it on a shared home proves nothing the
#    first run did not — the incremental leg needs a store that has NOT been
#    backfilled, which only a private seed can guarantee.
#
# ── what it does NOT need, unlike its HTTP sibling ───────────────────────────
#
# * No `parity/pyserver.py`. That file exists to disarm `backfill_price_book`,
#   which would populate `price_book` on the Python copy only and price the two
#   sides from different sources. The CLI never calls it: `cli.py` has no
#   `use_price_book_store` and no lifespan, so BOTH sides price from
#   `stackunderflow/data/models.toml` — the port through
#   `crate::status::engine_for_cli`, the reference through an unprimed
#   `infra.costs`. The seam (DIV-016 / RS-3-082) is closed on this path by the
#   reference's own construction, not by a patch.
# * No `409` leg and no inflated seed. The job slot is `backfill_jobs`, which
#   the CLI verb does not touch at all — there is no HTTP request to conflict
#   with, and `stackunderflow etl backfill` twice is just two runs.
# * No ports. Nothing is bound; `:8095` is not even in scope.
#
# ── usage ───────────────────────────────────────────────────────────────────
#
#   rust/etl-backfill-cli-differ.sh            # both legs
#   rust/etl-backfill-cli-differ.sh --keep     # leave the scratch tree behind
#
# Exit 0 when every leg is green, 1 on any divergence, 2 on a setup failure.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
SEED="${STAX_ETL_SEED:-$HERE/.parity-state/refresh/py/store.db}"
SCRATCH="$HERE/.parity-state/etl-backfill-cli"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

for tool in "$PY_BIN" "$PY_INTERP" "$RS_BIN"; do
    [ -x "$tool" ] || { echo "etl-backfill-cli-differ: SETUP FAILURE — no $tool" >&2; exit 2; }
done
[ -f "$SEED" ] || { echo "etl-backfill-cli-differ: SETUP FAILURE — no seed at $SEED" >&2; exit 2; }

rc=0
note() { printf '  %s\n' "$*"; }

# ── 0. one seed, two byte-identical copies ───────────────────────────────────
#
# A MISMATCH here aborts: two runs that did not start from the same bytes cannot
# be compared, and finding that out after the probes is how a differ reports an
# artefact as a defect.
prepare() {   # prepare <tier>
    local tier="$1" side
    for side in py rs; do
        rm -rf "$SCRATCH/$tier-$side"
        mkdir -p "$SCRATCH/$tier-$side"
        sqlite3 "$SEED" ".backup '$SCRATCH/$tier-$side/store.db'" || return 1
    done
    local a b
    a="$(md5sum "$SCRATCH/$tier-py/store.db" | cut -d' ' -f1)"
    b="$(md5sum "$SCRATCH/$tier-rs/store.db" | cut -d' ' -f1)"
    [ "$a" = "$b" ] || { echo "  SEED MISMATCH $a $b" >&2; return 1; }
    note "seed $tier: both copies $a"
}

# ── the stdout comparison, with the clock line masked ────────────────────────
#
# ONLY the `duration:` line is masked, and only its numeric field. Every other
# byte — the two comma-grouped counts, the eight `{name:<14s}  {count:>8,}`
# mart rows and their sort order, the leading blank line `click.echo("\n…")`
# emits — is compared exactly. A port that printed the duration in a different
# FORMAT does not match the mask, is not rewritten, and still fails.
mask() { sed -E 's/^(  duration: +)[0-9]+\.[0-9]{3}s$/\1<DURATION>s/' "$1"; }

run_leg() {   # run_leg <tier> <label> [extra argv…]
    local tier="$1" label="$2"; shift 2
    local out="$SCRATCH/$label"
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-py" \
        "$PY_BIN" etl backfill "$@" >"$out-py.out" 2>"$out-py.err" </dev/null )
    local py_rc=$?
    ( cd "$SCRATCH" && STACKUNDERFLOW_HOME="$SCRATCH/$tier-rs" \
        "$RS_BIN" etl backfill "$@" >"$out-rs.out" 2>"$out-rs.err" </dev/null )
    local rs_rc=$?

    local ok=1
    [ "$py_rc" = "$rs_rc" ] || { ok=0; note "EXIT $py_rc vs $rs_rc"; }
    mask "$out-py.out" >"$out-py.masked"
    mask "$out-rs.out" >"$out-rs.masked"
    if ! cmp -s "$out-py.masked" "$out-rs.masked"; then
        ok=0; note "STDOUT DIVERGENT"; diff -u "$out-py.masked" "$out-rs.masked" | sed 's/^/    /'
    fi
    cmp -s "$out-py.err" "$out-rs.err" || { ok=0; note "STDERR DIVERGENT"
        diff -u "$out-py.err" "$out-rs.err" | sed 's/^/    /'; }
    # The mask must have FIRED on both sides: a port that stopped printing the
    # line would otherwise pass by agreeing with a reference it never read.
    grep -qE '^  duration: +[0-9]+\.[0-9]{3}s$' "$out-py.out" || { ok=0
        note "the reference printed no duration line — the mask is stale"; }
    grep -qE '^  duration: +[0-9]+\.[0-9]{3}s$' "$out-rs.out" || { ok=0
        note "the port printed no duration line in the reference's format"; }

    [ "$ok" = 1 ] && note "$label stdout IDENTICAL (duration masked), exit $py_rc" \
                  || { note "$label FAILED"; rc=1; }
}

# ── the store is the real assertion ──────────────────────────────────────────
#
# A body can agree while the writes differ. `sqlite_header_diff.py` compares
# `sqlite_master` and every row of every table, and permits exactly one
# four-byte difference: DIV-257's version stamp.
compare_stores() {   # compare_stores <tier>
    local tier="$1"
    # `sqlite_header_diff.py` is the tool for a COPY and the wrong tool here:
    # it permits exactly one four-byte difference, and two independent WRITES
    # also disagree on `mart_watermark.last_refresh_ts`. It is still run, for
    # the record, because its byte offsets name where the disagreement is.
    local header
    header="$("$PY_INTERP" "$HERE/parity/sqlite_header_diff.py" \
        "$SCRATCH/$tier-py" "$SCRATCH/$tier-rs" 2>&1)"
    local header_rc=$?
    if [ "$header_rc" = 0 ]; then
        note "$tier store byte-identical modulo DIV-257's four header bytes"
    else
        note "$tier store differs on more than the header — expected (a WRITE, not a copy):"
        printf '%s\n' "$header" | sed 's/^/    /'
    fi
    # The real assertion: sqlite_master, then every row of every table, with
    # ONE column masked and the mask reported.
    if "$PY_INTERP" "$HERE/parity/etl_store_diff.py" \
            "$SCRATCH/$tier-py" "$SCRATCH/$tier-rs" 2>&1 | sed 's/^/    /'; then
        note "$tier store IDENTICAL (one masked column, named above)"
    else
        note "$tier store DIVERGENT"; rc=1
    fi
}

# The content assertion that survives the clock: everything except the stamp.
compare_content() {   # compare_content <tier>
    local tier="$1" side
    for side in py rs; do
        sqlite3 "$SCRATCH/$tier-$side/store.db" \
          "SELECT source_message_fk, provider, model, day, input_tokens, output_tokens,
                  cache_read_tokens, cache_create_tokens, ROUND(cost_usd, 10), cost_source
             FROM usage_events ORDER BY source_message_fk;
           SELECT day, project_id, provider, model, ROUND(cost_usd, 10), message_count,
                  session_count FROM daily_mart ORDER BY 1,2,3,4;
           SELECT session_id, ROUND(cost_usd, 10), message_count FROM session_mart ORDER BY 1;
           SELECT day, provider, ROUND(cost_usd, 10) FROM provider_day_mart ORDER BY 1,2;
           SELECT day, model, ROUND(cost_usd, 10) FROM model_day_mart ORDER BY 1,2;
           SELECT mart_name, last_event_id FROM mart_watermark ORDER BY 1;" \
          >"$SCRATCH/$tier-$side-dump.txt"
    done
    if diff -u "$SCRATCH/$tier-py-dump.txt" "$SCRATCH/$tier-rs-dump.txt" >/dev/null; then
        note "$tier content IDENTICAL ($(wc -l < "$SCRATCH/$tier-py-dump.txt") rows dumped)"
    else
        note "$tier content DIVERGENT"
        diff -u "$SCRATCH/$tier-py-dump.txt" "$SCRATCH/$tier-rs-dump.txt" | head -40 | sed 's/^/    /'
        rc=1
    fi
}

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"

echo "=== leg 1: incremental (the default) ==="
prepare inc || exit 2
run_leg inc inc-first
compare_stores inc
compare_content inc

echo "=== leg 2: the same store again — idempotent, every event a duplicate ==="
run_leg inc inc-second
compare_content inc

echo "=== leg 3: --force (wipe + rebuild_from_scratch + a fresh pass) ==="
prepare force || exit 2
run_leg force force-first --force
compare_stores force
compare_content force

echo "=== leg 4: the two legs must AGREE with each other ==="
# `--force` on a virgin store and an incremental pass over the same store must
# land on the same rows. If they do not, one of the two paths is wrong on BOTH
# implementations at once and no cross-implementation diff would have seen it.
if diff -u "$SCRATCH/inc-py-dump.txt" "$SCRATCH/force-py-dump.txt" >/dev/null; then
    note "incremental and --force agree, on the reference"
else
    note "incremental and --force DISAGREE on the reference — investigate"
    diff -u "$SCRATCH/inc-py-dump.txt" "$SCRATCH/force-py-dump.txt" | head -20 | sed 's/^/    /'
    rc=1
fi

echo
if [ "$rc" = 0 ]; then echo "etl-backfill-cli-differ: GREEN"; else echo "etl-backfill-cli-differ: DIVERGENT"; fi
[ "$KEEP" = 1 ] && echo "scratch kept at $SCRATCH"
exit "$rc"

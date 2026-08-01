#!/usr/bin/env bash
# The run-clock differ — closing the "today window is empty" vacuity gap.
#
# THE PROBLEM IT SOLVES, stated plainly. `rust/parity/cases.txt` gates
# `status`, `today`, `month` and `report -p today` against the shared
# `.parity-state` home, which is a snapshot taken on a fixed day. Those verbs
# window on the RUN clock, so from the day after the snapshot every one of them
# answers "No activity in this period." on BOTH sides and the case passes without
# ever exercising the branch it is named for. Tranche 1 filed that as a
# known-open on `status`; this script is the fix, and it covers tranche 3's three
# verbs at the same time.
#
# It is deliberately NOT wired into `parity-cli.sh` or `ci.sh`. Two reasons, both
# concrete: (1) the fixture has to be rebuilt at run time, and the harness's seed
# mechanism copies static directories — teaching it to run a generator is an edit
# to a file three tranches are flying over; (2) the fixture is time-sensitive and
# REFUSES to build near local midnight, so it can legitimately skip, and a gate
# that can skip itself is a gate that eventually skips silently. This is the
# `hooks-parity.sh` pattern: standalone, documented, run and reported by hand.
#
# Usage
#   rust/reports-clock-differ.sh          # build the fixture, diff, report
#   rust/reports-clock-differ.sh --keep   # leave the two homes for inspection
#
# Exit: 0 all identical · 1 any divergence · 2 setup failure · 3 skipped
#       (too close to local midnight — the reason is printed, never swallowed).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
WORK="${STAX_CLOCK_WORK:-$HERE/.parity-state/clock}"

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

[ -x "$PY_BIN" ] || { echo "clock-differ: SETUP FAILURE — no Python CLI at $PY_BIN" >&2; exit 2; }
[ -x "$PY_INTERP" ] || PY_INTERP=python3
if [ ! -x "$RS_BIN" ]; then
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

# The same determinism pins `parity-cli.sh` sets, and for the same reasons.
# TZ is NOT forced to UTC here: `parse_period` builds its window from
# `datetime.now(UTC)` while the fixture stamps naive local times, so the two must
# agree about what "local" means — and forcing TZ would make the fixture and the
# window disagree on any machine that is not already on UTC.
export LC_ALL=C LANG=C PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

rm -rf "$WORK"
mkdir -p "$WORK"

echo "clock-differ: building the run-clock fixture"
FIXTURE="$("$PY_INTERP" "$HERE/parity/build_clock_state.py" "$WORK/seed")"
case $? in
    0) ;;
    3) echo "clock-differ: SKIPPED (see reason above)"; exit 3 ;;
    *) echo "clock-differ: SETUP FAILURE — the fixture would not build" >&2; exit 2 ;;
esac
printf '%s\n' "$FIXTURE"

# THE ANTI-VACUITY ASSERTION. The whole point of this differ is that the window
# is NOT empty; a fixture that built but landed outside today would reproduce the
# exact bug being fixed, and it would do it while printing "identical".
today_events="$(printf '%s' "$FIXTURE" | sed -n 's/.*"today_events": \([0-9]*\).*/\1/p')"
if [ -z "$today_events" ] || [ "$today_events" -lt 3 ]; then
    echo "clock-differ: SETUP FAILURE — the fixture put only ${today_events:-0} event(s) in today's window;" >&2
    echo "              this differ exists to stop exactly that kind of green" >&2
    exit 2
fi
echo "clock-differ: today's window carries $today_events event(s) — non-vacuous"

pass=0; fail=0; failed=()

run_case() {
    local id="$1"; shift
    local py_home="$WORK/py-$id" rs_home="$WORK/rs-$id"
    rm -rf "$py_home" "$rs_home"
    cp -a "$WORK/seed" "$py_home"
    cp -a "$WORK/seed" "$rs_home"

    ( cd "$REPO_ROOT" && STACKUNDERFLOW_HOME="$py_home" timeout 120 \
        "$PY_BIN" "$@" >"$WORK/$id.py.out" 2>"$WORK/$id.py.err" </dev/null )
    local py_rc=$?
    ( cd "$REPO_ROOT" && STACKUNDERFLOW_HOME="$rs_home" timeout 120 \
        "$RS_BIN" "$@" >"$WORK/$id.rs.raw" 2>"$WORK/$id.rs.raw.err" </dev/null )
    local rs_rc=$?

    # The one normalisation `parity-cli.sh` performs, same scope, same reason.
    sed -e "/^Usage:/s/\bstax\b/stackunderflow/g" -e "/^Try '/s/\bstax\b/stackunderflow/g" \
        "$WORK/$id.rs.raw" >"$WORK/$id.rs.out"
    sed -e "/^Usage:/s/\bstax\b/stackunderflow/g" -e "/^Try '/s/\bstax\b/stackunderflow/g" \
        "$WORK/$id.rs.raw.err" >"$WORK/$id.rs.err"

    local ok=1
    cmp -s "$WORK/$id.py.out" "$WORK/$id.rs.out" || ok=0
    cmp -s "$WORK/$id.py.err" "$WORK/$id.rs.err" || ok=0
    [ "$py_rc" = "$rs_rc" ] || ok=0

    # A row that answers "no activity" on a fixture built to have activity is the
    # vacuity bug wearing a pass. Caught per-row, not just per-fixture.
    if grep -q "No activity in this period" "$WORK/$id.py.out" 2>/dev/null; then
        echo "  VACUOUS  $id — the reference found nothing in a window that has spend"
        ok=0
    fi

    if [ "$ok" = 1 ]; then
        pass=$((pass + 1))
        printf '  ok    %-22s rc=%s %sB\n' "$id" "$py_rc" "$(wc -c <"$WORK/$id.py.out")"
        return 0
    fi
    fail=$((fail + 1)); failed+=("$id")
    printf '  FAIL  %-22s py=rc%s rs=rc%s\n' "$id" "$py_rc" "$rs_rc"
    diff -u "$WORK/$id.py.out" "$WORK/$id.rs.out" | head -40
    diff -u "$WORK/$id.py.err" "$WORK/$id.rs.err" | head -20
    return 1
}

echo
echo "=== the verbs whose window is the run clock ==="
run_case status-text        status --no-auto-ingest
run_case status-json        status --format json --no-auto-ingest
run_case today-text         today --no-auto-ingest
run_case today-json         today --format json --no-auto-ingest
run_case month-text         month --no-auto-ingest
run_case month-json         month --format json --no-auto-ingest
run_case report-today       report -p today --no-auto-ingest
run_case report-today-json  report -p today --format json --no-auto-ingest
run_case report-month       report -p month --no-auto-ingest
run_case today-project      today --project -clock-alpha --no-auto-ingest
run_case today-exclude      today --exclude -clock-alpha --no-auto-ingest

echo
echo "=== clock-differ tally ==="
printf 'cases: %s   pass: %s   FAIL: %s\n' "$((pass + fail))" "$pass" "$fail"
if [ "$fail" -gt 0 ]; then
    printf 'failing: %s\n' "${failed[*]}"
    echo "homes kept at $WORK"
    exit 1
fi
[ "$KEEP" = 0 ] && rm -rf "$WORK"
echo "byte-identical on every run-clock case."
exit 0

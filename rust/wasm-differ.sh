#!/usr/bin/env bash
# wave 9 — the CLI-vs-wasm differ.
#
# One store, two engines: the native `stax` binary reading `store.db` off the
# disk, and the wasm artifact a browser would load reading the same file's bytes
# out of its own linear memory. Every case in `parity/wasm-cases.txt` runs on
# both and the stdout bytes are compared with `cmp`, exit codes included.
#
#   ./wasm-differ.sh                     # default store + cases
#   STAX_WASM_STORE=/path/store.db ./wasm-differ.sh
#   ./wasm-differ.sh W-dec-cache W-file-cli   # only these ids
#
# NOT wired into ci.sh, deliberately, for the same two reasons the wave-6 hooks
# differ stayed out: it needs a wasm toolchain (a wasm32 clang and the
# wasm-bindgen CLI) that the CI box does not have, and a 227 MB store import is
# minutes, not seconds. `rust/demo/README.md` documents the standalone run.
#
# The store: `.parity-state/wasm9/home/store.db`, a 227 MB subset of the
# maintainer's real store (a `sqlite3 backup()` snapshot of the live file with
# all message partitions but 202607/202608 deleted, then VACUUMed). The subset
# exists because wasm32's address space cannot hold the 3.9 GB original — see
# DIV-332. Both engines read the SAME file, so the subset weakens the *coverage*
# of the proof, never its equality.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
STATE="${STAX_WASM_STATE:-$HERE/.parity-state/wasm9}"
STORE="${STAX_WASM_STORE:-$STATE/home/store.db}"
CASES="${STAX_WASM_CASES:-$HERE/parity/wasm-cases.txt}"
CLI="${STAX_CLI:-$HERE/target/release/stax}"
NODE="${STAX_NODE:-node}"
OUT="$STATE/differ"
WASM_OUT="$OUT/wasm"
CLI_OUT="$OUT/cli"

fail() { echo "wasm-differ: $*" >&2; exit 2; }

[ -f "$STORE" ] || fail "no store at $STORE (see rust/demo/README.md for how it is built)"
[ -x "$CLI" ] || fail "no native CLI at $CLI — cargo build -p stax-cli --release"
[ -f "$HERE/demo/pkg-node/stax_wasm.js" ] || fail "no wasm build — run rust/demo/build.sh first"
command -v "$NODE" >/dev/null || fail "node is needed to drive the wasm artifact"

rm -rf "$OUT"; mkdir -p "$WASM_OUT" "$CLI_OUT" "$OUT/diffs"

# One clock for the whole run. The native CLI reads its own wall clock a
# fraction of a second later; the ranker's recency term is 1/(1+days), so a
# sub-second offset moves a score by ~1e-5 and can only reorder rows that are
# tied to five decimal places. Recorded rather than engineered away — pinning
# the CLI's clock would mean an env var the reference does not have.
NOW="$(date +%s)"
CWD="$HERE"
STOREPATH="$STORE"

ids=("$@")
want() {
    [ ${#ids[@]} -eq 0 ] && return 0
    for id in "${ids[@]}"; do [ "$id" = "$1" ] && return 0; done
    return 1
}

# ── collect the cases ────────────────────────────────────────────────────────
requests="$OUT/requests.jsonl"
: > "$requests"
declare -a CASE_IDS CASE_ARGV
while IFS=$'\t' read -r id argv request; do
    case "$id" in ''|'#'*) continue ;; esac
    want "$id" || continue
    [ -n "${request:-}" ] || fail "case $id has no wasm request (tab-separated, three fields)"
    subst() { printf '%s' "$1" | sed -e "s|@NOW@|$NOW|g" -e "s|@CWD@|$CWD|g" -e "s|@STOREPATH@|$STOREPATH|g"; }
    CASE_IDS+=("$id")
    CASE_ARGV+=("$(subst "$argv")")
    printf '{"id":"%s","request":%s}\n' "$id" "$(subst "$request")" >> "$requests"
done < "$CASES"

[ ${#CASE_IDS[@]} -gt 0 ] || fail "no cases selected"

# ── the wasm side: one boot, one import, N queries ───────────────────────────
"$NODE" "$HERE/demo/differ.js" "$STORE" "$WASM_OUT" < "$requests" || fail "the wasm side failed to run"

# ── the native side ──────────────────────────────────────────────────────────
export STACKUNDERFLOW_HOME="$(dirname "$STORE")"
cd "$CWD" || fail "cannot cd to $CWD"
for i in "${!CASE_IDS[@]}"; do
    id="${CASE_IDS[$i]}"
    eval "\"\$CLI\" ${CASE_ARGV[$i]}" > "$CLI_OUT/$id.out" 2> "$CLI_OUT/$id.err"
    echo "$?" > "$CLI_OUT/$id.code"
done

# ── compare ──────────────────────────────────────────────────────────────────
identical=0; divergent=0; errored=0
for id in "${CASE_IDS[@]}"; do
    if [ ! -s "$WASM_OUT/$id.code" ]; then
        echo "ERROR    $id — the wasm side produced nothing"; errored=$((errored + 1)); continue
    fi
    wcode="$(cat "$WASM_OUT/$id.code")"; ccode="$(cat "$CLI_OUT/$id.code")"
    if [ -f "$WASM_OUT/$id.err" ]; then
        echo "ERROR    $id — wasm engine error: $(head -1 "$WASM_OUT/$id.err")"
        errored=$((errored + 1)); continue
    fi
    if cmp -s "$WASM_OUT/$id.out" "$CLI_OUT/$id.out" && [ "$wcode" = "$ccode" ]; then
        identical=$((identical + 1))
    else
        divergent=$((divergent + 1))
        {
            echo "--- cli  (exit $ccode)"
            echo "+++ wasm (exit $wcode)"
            diff -u "$CLI_OUT/$id.out" "$WASM_OUT/$id.out"
        } > "$OUT/diffs/$id.diff"
        echo "DIVERGE  $id — $OUT/diffs/$id.diff"
    fi
done

total=$((identical + divergent + errored))
echo
echo "wasm-differ: $total cases — $identical identical / $divergent divergent / $errored errors"
echo "  store   $STORE ($(stat -c%s "$STORE") bytes)"
echo "  cli     $CLI"
echo "  wasm    $HERE/demo/pkg-node/stax_wasm_bg.wasm ($(stat -c%s "$HERE/demo/pkg-node/stax_wasm_bg.wasm") bytes)"
echo "  timings $WASM_OUT/_timings.tsv"
[ $((divergent + errored)) -eq 0 ]

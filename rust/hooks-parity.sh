#!/usr/bin/env bash
# The hook differ — byte parity for the layer that runs inside an agent's hook
# budget.
#
# For every recorded invocation in `parity/hook-cases.txt` this runs BOTH
# implementations — `stackunderflow hooks run <id>` and `stax-hooks hooks run
# <id>` — with the same stdin, the same extra environment, and their own private
# copy of the same store state, then compares FIVE things:
#
#   1. stdout, byte for byte      (the `hookSpecificOutput` envelope)
#   2. stderr, byte for byte      (must be empty; a hook that talks is a bug)
#   3. the exit code              (always 0 — a non-zero PreToolUse hook BLOCKS)
#   4. the `captured_events` rows each side wrote, minus `ts`
#   5. the governance JSON each side left behind, with timestamps masked
#
# Comparisons 4 and 5 exist because four of the nine hooks are WRITERS and
# print nothing at all: a stdout-only differ would call `stackunderflow-stop`
# identical while one side silently recorded a different `payload_json`.
#
# Why its own homes rather than the shared `.parity-state/fresh`: the capture
# hooks write, `store.db.connect` opens read-write and sets `journal_mode =
# WAL`, and the shared states are fleet infrastructure. Each run copies the
# synthetic source home twice, under `.parity-state/hooks/`, and both copies are
# thrown away next run.
#
# Usage
#   rust/hooks-parity.sh                 # the whole corpus
#   rust/hooks-parity.sh --only I-start  # id substring filter
#   rust/hooks-parity.sh --build-state   # (re)build the synthetic home
#   rust/hooks-parity.sh --list          # print the corpus and exit
#   rust/hooks-parity.sh --bench         # the PERF.md latency rows
#
# Exit: 0 when every case is byte-identical, 1 on any divergence, 2 on a setup
# failure (missing venv / missing state).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
STATE_DIR="${STAX_PARITY_STATE_DIR:-$HERE/.parity-state}"
HOOK_DIR="$STATE_DIR/hooks"
SRC_HOME="$HOOK_DIR/synthetic-src"
PY_HOME="$HOOK_DIR/py-home"
RS_HOME="$HOOK_DIR/rs-home"
DIFFS="$HOOK_DIR/diffs"
CASES="${STAX_HOOK_CASES:-$HERE/parity/hook-cases.txt}"
RS_BIN="${STAX_HOOK_RS_BIN:-$HERE/target/release/stax-hooks}"

ONLY=""
DO_BUILD=0
DO_LIST=0
DO_BENCH=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --build-state) DO_BUILD=1; shift ;;
        --list) DO_LIST=1; shift ;;
        --bench) DO_BENCH=1; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "hooks-parity: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [ "$DO_LIST" = 1 ]; then
    grep -v '^#' "$CASES" | grep -v '^[[:space:]]*$' | cut -f1,2
    exit 0
fi

# ── setup ────────────────────────────────────────────────────────────────────

if [ ! -x "$PY_BIN" ]; then
    echo "hooks-parity: SETUP FAILURE — no Python CLI at $PY_BIN" >&2
    echo "            (set STAX_PARITY_PY_BIN, or run ci.sh --skip-parity)" >&2
    exit 2
fi

py_interp="$PY_ROOT/.venv/bin/python"
[ -x "$py_interp" ] || py_interp="python3"

if [ "$DO_BUILD" = 1 ] || [ ! -f "$SRC_HOME/store.db" ]; then
    echo "hooks-parity: building the synthetic state at $SRC_HOME"
    PYTHONPATH="$REPO_ROOT" "$py_interp" "$HERE/parity/build_hook_state.py" "$SRC_HOME" --force || exit 2
    [ "$DO_BUILD" = 1 ] && exit 0
fi

# DIV-484: build EVERY run, not only when the binary is missing. This used to
# be `if [ ! -x "$RS_BIN" ]`, which meant a green run could be measured against
# a binary older than the code it claims to gate — caught in the act on
# 2026-08-04, when the checked-out `stax-hooks` predated the agent-inbox
# interject and the first run of the new rows was green against bytes that did
# not contain the feature. `endpoint-parity.sh` has always built unconditionally
# at its top; this is the same rule, for the same reason. An up-to-date tree
# rebuilds in under a second.
echo "hooks-parity: building the release binary (the gate compares shipped bytes)"
if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
( cd "$HERE" && cargo build --release -p stax-hooks --quiet ) || exit 2
[ -x "$RS_BIN" ] || { echo "hooks-parity: no binary at $RS_BIN" >&2; exit 2; }

# Determinism: pin everything either implementation could read differently. The
# hook path reads far less of the environment than the CLI does, but the two
# `proactive_*` knobs and the recall deadline are all env-settable, so they are
# unset here and re-set per case from the corpus's own column.
export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
unset STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS
unset STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS
unset STACKUNDERFLOW_PROACTIVE_ENABLED
unset STACKUNDERFLOW_PROACTIVE_DISABLED
unset STACKUNDERFLOW_PROACTIVE_TYPES
unset STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION
unset STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS
# The recall deadline is a WALL-CLOCK race against a CPython process start, and
# both implementations enforce it identically — which means that at the shipped
# 1.5s a loaded box makes the differ measure the machine rather than the port.
# Reproduced: `R-edit-risky` diverged once (Python 0.6s, Rust's child pushed past
# 1.5s while both suites ran) and passed alone every time after. The deadline is
# therefore pinned wide here, and `R-tiny-deadline` pins the OTHER end — that
# both sides go silent when it genuinely expires.
export STACKUNDERFLOW_RECALL_TIMEOUT=30

# The reference must be THIS worktree's Python, not whatever tree the venv's
# editable install points at — the same pin gate 4 carries, for the same reason.
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

# `recall.py` shells the bare name `stackunderflow` — the portability contract —
# and the Rust hook deliberately spawns the same one. Put the venv first so BOTH
# sides resolve the identical child process; without this the Python side finds
# it through the venv's own bin dir and the Rust side finds nothing, and the
# recall cases would compare "a warning" against "silence" for the wrong reason.
export PATH="$(dirname "$PY_BIN"):$PATH"

# Both sides get a private copy of the same starting state, every run.
rm -rf "$PY_HOME" "$RS_HOME" "$DIFFS"
mkdir -p "$DIFFS"
cp -r "$SRC_HOME" "$PY_HOME" || exit 2
cp -r "$SRC_HOME" "$RS_HOME" || exit 2

pass=0; fail=0; known=0
failed_ids=()

# ── one case ─────────────────────────────────────────────────────────────────

run_case() {
    local id="$1" hook_id="$2" flags="$3" envspec="$4" stdin="$5"
    local known_open=0
    case "$id" in
        !*) known_open=1; id="${id#!}" ;;
    esac

    local args=("hooks" "run" "$hook_id")
    [ "$flags" = "capture" ] && args+=("--capture-content")

    local work; work="$(mktemp -d)"
    if [ "$stdin" = "-" ]; then
        : > "$work/stdin"
    else
        printf '%s' "$stdin" > "$work/stdin"
    fi

    # The per-case environment column, applied identically to both sides.
    local envargs=()
    if [ "$envspec" != "-" ]; then
        local IFS=','
        for pair in $envspec; do envargs+=("$pair"); done
    fi

    env "${envargs[@]}" STACKUNDERFLOW_HOME="$PY_HOME" \
        "$PY_BIN" "${args[@]}" <"$work/stdin" >"$work/py.out" 2>"$work/py.err"
    local py_rc=$?
    env "${envargs[@]}" STACKUNDERFLOW_HOME="$RS_HOME" \
        "$RS_BIN" "${args[@]}" <"$work/stdin" >"$work/rs.out" 2>"$work/rs.err"
    local rs_rc=$?

    local ok=1 why=""
    cmp -s "$work/py.out" "$work/rs.out" || { ok=0; why="stdout"; }
    cmp -s "$work/py.err" "$work/rs.err" || { ok=0; why="$why stderr"; }
    [ "$py_rc" = "$rs_rc" ] || { ok=0; why="$why exit($py_rc vs $rs_rc)"; }

    if [ "$ok" = 1 ]; then
        if [ "$known_open" = 1 ]; then
            known=$((known + 1))
            printf '  OPEN  %-28s (known-open, currently identical)\n' "$id"
        else
            pass=$((pass + 1))
        fi
        rm -rf "$work"
        return 0
    fi

    {
        printf '=== %s  (%s%s)\n' "$id" "$hook_id" \
            "$([ "$flags" = capture ] && printf ' --capture-content')"
        printf -- '--- env: %s\n' "$envspec"
        printf -- '--- stdin:\n%s\n' "$stdin"
        printf -- '--- diverged on: %s\n' "$why"
        printf -- '--- stdout diff (python | rust)\n'
        diff -u "$work/py.out" "$work/rs.out"
        printf -- '--- stderr diff (python | rust)\n'
        diff -u "$work/py.err" "$work/rs.err"
    } > "$DIFFS/$id.diff" 2>&1

    if [ "$known_open" = 1 ]; then
        known=$((known + 1))
        printf '  OPEN  %-28s %s\n' "$id" "$why"
        rm -rf "$work"
        return 0
    fi
    fail=$((fail + 1))
    failed_ids+=("$id")
    printf '  FAIL  %-28s %s\n' "$id" "$why"
    rm -rf "$work"
    return 1
}

# ── the store-side comparison ────────────────────────────────────────────────
#
# `ts` is `datetime.now(UTC).isoformat()` on both sides and the two processes
# run milliseconds apart, so it is masked rather than compared — and because
# `captured_events` is UNIQUE on `(ts, hook_id, session_id)`, masking it is also
# what makes the row counts comparable at all.
dump_events() {
    "$py_interp" - "$1" <<'PYDUMP'
import sqlite3, sys
conn = sqlite3.connect(f"file:{sys.argv[1]}/store.db?mode=ro", uri=True)
rows = conn.execute(
    "SELECT project_id, session_id, hook_id, event_kind, payload_json "
    "FROM captured_events ORDER BY id"
).fetchall()
for row in rows:
    print("\t".join("NULL" if v is None else str(v) for v in row))
PYDUMP
}

# The governance file carries two wall-clock timestamps (`sessions[*].ts` and
# every cooldown expiry). Mask any ISO-8601 datetime; everything else — the
# SHA-1 fingerprints, the counters, the key ORDER — is compared verbatim,
# which is the point: the fingerprint is a cross-implementation contract.
dump_state() {
    local file="$1/proactive_state.json"
    [ -f "$file" ] || { echo "(no proactive_state.json)"; return; }
    sed -E 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.+-]+/<TS>/g' "$file"
}

# The inbox tree — comparison 6, and it exists for the same reason as 4 and 5.
# `agent_inbox.render_for_injection` has ONE side effect, a rename
# (`*.json` → `*.seen.json`), and stdout can only show it INDIRECTLY: a later
# fire that re-announces a message it should have consumed. That is enough to
# catch a side that never renames, and nothing else — not a side that renames
# to a different name, not a side that rewrites or deletes the corrupt file it
# is supposed to skip. Names + content hashes make all of it falsifiable.
# (DIV-460: before porting a behaviour, ask what the gate would see if the port
# got it wrong; if the answer is "nothing", fix the gate first.)
dump_inbox() {
    (
        cd "$1" 2>/dev/null || return
        [ -d inbox ] || { echo "(no inbox)"; return; }
        find inbox -type f -print0 | LC_ALL=C sort -z | xargs -0 -r md5sum
    )
}

# ── the bench ────────────────────────────────────────────────────────────────

bench() {
    local reps="${STAX_HOOK_BENCH_REPS:-30}"
    echo "hooks-parity: latency, $reps reps per side per case (wall clock, spawn to exit)"
    echo
    printf '%-36s %12s %12s %8s\n' "case" "python" "rust" "ratio"
    for spec in \
        "stackunderflow-inject-session-start|{\"cwd\":\"/tmp/stax/hook/parity/proj\"}" \
        "stackunderflow-inject-pre-tool-use|{\"tool_input\":{\"file_path\":\"/tmp/stax/hook/parity/proj/services/discovery.py\"}}" \
        "stackunderflow-stop|{\"session_id\":\"hook-parity-session-0001\",\"cwd\":\"/tmp/stax/hook/parity/proj\"}" \
        "stackunderflow-posttool-nudge|{\"tool_name\":\"Bash\",\"cwd\":\"/tmp/stax/hook/parity/proj\",\"tool_response\":{\"stdout\":\"ok\"}}"
    do
        local hook_id="${spec%%|*}" payload="${spec#*|}"
        local py_ms rs_ms
        py_ms="$(_time_loop "$reps" "$PY_HOME" "$PY_BIN" "$hook_id" "$payload")"
        rs_ms="$(_time_loop "$reps" "$RS_HOME" "$RS_BIN" "$hook_id" "$payload")"
        printf '%-36s %10s ms %10s ms %7sx\n' "${hook_id#stackunderflow-}" "$py_ms" "$rs_ms" \
            "$(awk -v p="$py_ms" -v r="$rs_ms" 'BEGIN{if(r>0)printf "%.1f", p/r; else print "-"}')"
    done
}

_time_loop() {
    local reps="$1" home="$2" bin="$3" hook_id="$4" payload="$5"
    local start end
    start="$(date +%s%N)"
    for _ in $(seq "$reps"); do
        printf '%s' "$payload" | STACKUNDERFLOW_HOME="$home" "$bin" hooks run "$hook_id" >/dev/null 2>&1
    done
    end="$(date +%s%N)"
    awk -v s="$start" -v e="$end" -v n="$reps" 'BEGIN{printf "%.2f", (e-s)/1000000/n}'
}

if [ "$DO_BENCH" = 1 ]; then
    bench
    exit 0
fi

# ── the run ──────────────────────────────────────────────────────────────────

echo "hooks-parity: $(grep -cv -e '^#' -e '^[[:space:]]*$' "$CASES") cases"
echo "  python  $PY_BIN"
echo "  rust    $RS_BIN"
echo "  state   $SRC_HOME (copied to py-home / rs-home)"
echo

while IFS=$'\t' read -r id hook_id flags envspec stdin; do
    case "$id" in ''|'#'*) continue ;; esac
    [ -n "$ONLY" ] && case "$id" in *"$ONLY"*) ;; *) continue ;; esac
    run_case "$id" "$hook_id" "$flags" "$envspec" "$stdin"
done < "$CASES"

echo
echo "hooks-parity: store-side comparison"
store_ok=1
if ! diff -u <(dump_events "$PY_HOME") <(dump_events "$RS_HOME") > "$DIFFS/captured_events.diff"; then
    store_ok=0
    echo "  FAIL  captured_events rows differ — see $DIFFS/captured_events.diff"
    head -40 "$DIFFS/captured_events.diff"
else
    rm -f "$DIFFS/captured_events.diff"
    echo "  ok    captured_events: $(dump_events "$PY_HOME" | wc -l) rows identical (ts masked)"
fi
if ! diff -u <(dump_state "$PY_HOME") <(dump_state "$RS_HOME") > "$DIFFS/proactive_state.diff"; then
    store_ok=0
    echo "  FAIL  proactive_state.json differs — see $DIFFS/proactive_state.diff"
    head -40 "$DIFFS/proactive_state.diff"
else
    rm -f "$DIFFS/proactive_state.diff"
    echo "  ok    proactive_state.json identical (ISO timestamps masked)"
fi
if ! diff -u <(dump_inbox "$PY_HOME") <(dump_inbox "$RS_HOME") > "$DIFFS/inbox.diff"; then
    store_ok=0
    echo "  FAIL  inbox tree differs — see $DIFFS/inbox.diff"
    head -40 "$DIFFS/inbox.diff"
else
    rm -f "$DIFFS/inbox.diff"
    echo "  ok    inbox: $(dump_inbox "$PY_HOME" | wc -l) files identical (names + content)"
fi

echo
if [ "$fail" = 0 ] && [ "$store_ok" = 1 ]; then
    echo "hooks-parity: $pass identical / 0 divergent / $known known-open — GREEN"
    exit 0
fi
echo "hooks-parity: $pass identical / $fail DIVERGENT / $known known-open"
for id in "${failed_ids[@]:-}"; do [ -n "$id" ] && echo "    $id  ($DIFFS/$id.diff)"; done
[ "$store_ok" = 0 ] && echo "    store-side comparison FAILED"
exit 1

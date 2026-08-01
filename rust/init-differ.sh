#!/usr/bin/env bash
# The `init` / `start` differ — the half of wave 7 that `parity-cli.sh` cannot hold.
#
# `parity-cli.sh` needs a case to terminate. `stax start` blocks on the server
# and `stax init` always falls through into it, so **every byte a successful
# boot prints is unreachable from that harness**. This runs the two verbs for
# real, on :8100, kills them once the boot line lands, and compares:
#
#   1. stdout, byte for byte, up to and including `  Ctrl+C to stop`
#   2. stderr, byte for byte
#   3. the skills destination tree, byte for byte (`diff -r`)
#   4. `config.json`'s KEY SET — never its bytes; see DIV-303
#
# It is therefore also the wave's **boot smoke test**: nothing prints
# `StackUnderflow is live at …` unless a socket is actually accepting, so a green
# run is proof the dashboard came up, on both implementations, against a store
# neither of them had before the run started.
#
# ── ports ────────────────────────────────────────────────────────────────────
#
# :8100 only, probed free before use, one implementation at a time. :8095 is the
# maintainer's Python server and :8096 is the campaign server — this touches
# neither. The probe is a CONNECT, not a bind: a bind test false-positives on
# TIME_WAIT when both sides set SO_REUSEADDR (wave-5 finding 13).
#
# ── the scratch homes ────────────────────────────────────────────────────────
#
# Each implementation gets its OWN copy of the scenario's seed at the SAME path,
# with `$HOME` and `$STACKUNDERFLOW_HOME` both pointed inside it, so neither can
# observe the other's writes and neither can reach the maintainer's `~/.claude`.
# The watcher is disabled by env because Python's lifespan would otherwise start
# a `watchdog` observer over the real `~/.claude` for the few seconds it lives.
#
# ── what is deliberately NOT compared ────────────────────────────────────────
#
# `store.db` and `search_index.db`. The reference's lifespan kicks off a
# background ingest over the live corpus and constructs five services; the port
# does neither (DIV-305). Comparing those files would measure the gap the ledger
# already records, on every run, forever. The skills tree and the printed bytes
# are what `init` is FOR, and those are compared exactly.
#
# Usage
#   rust/init-differ.sh                # every scenario
#   rust/init-differ.sh --only force   # id substring filter
#   rust/init-differ.sh --keep         # leave the scratch trees behind
#
# Exit: 0 when every scenario matches, 1 on divergence, 2 on setup.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
WORK="${STAX_INIT_WORK:-$HERE/.parity-state/init}"
PORT="${STAX_INIT_PORT:-8100}"
BOOT_WAIT="${STAX_INIT_BOOT_WAIT:-25}"

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,48p' "$0"; exit 0 ;;
        *) echo "init-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[ -x "$PY_BIN" ] || { echo "init-differ: SETUP FAILURE — no Python CLI at $PY_BIN" >&2; exit 2; }
if [ ! -x "$RS_BIN" ]; then
    echo "init-differ: building the release binary"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli -p stax-server --quiet ) || exit 2
fi
[ -x "$HERE/target/release/stax-server" ] || {
    echo "init-differ: SETUP FAILURE — `stax start` spawns target/release/stax-server (DIV-308)" >&2
    exit 2
}

# The port must be free BEFORE anything is launched, and a connect probe is the
# honest test. 8095/8096 are never touched.
if command -v python3 >/dev/null 2>&1; then
    if python3 - "$PORT" <<'PY'
import socket, sys
s = socket.socket()
s.settimeout(0.4)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
PY
    then
        echo "init-differ: SETUP FAILURE — something is already listening on :$PORT" >&2
        exit 2
    fi
fi

export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"
# The lifespan would otherwise spawn a watchdog observer over the real ~/.claude
# for the seconds this run lives. Read-only either way, but a differ should not
# be starting file watchers on the maintainer's home.
export STACKUNDERFLOW_DISABLE_WATCHER=1
export STACKUNDERFLOW_DISABLE_LOCK=1
# `auto_browser` defaults ON. Neither side may open a browser on the
# maintainer's desktop, so every scenario passes the no-browser flag AND the
# setting is pinned off, belt and braces.
export STACKUNDERFLOW_AUTO_BROWSER=0

rm -rf "$WORK"
mkdir -p "$WORK/diffs"

pass=0; fail=0; log_lines_dropped=0
failed_ids=()

# Seed a scratch home. `$1` = destination, `$2` = seed kind.
#
#   bare      nothing — the fresh-install path
#   current   the shipped SKILL.md files already in place, byte-identical
#   modified  one destination SKILL.md edited by hand
seed_home() {
    local dest="$1" kind="$2"
    mkdir -p "$dest/.claude/skills" "$dest/state" || return 1
    local shipped="$REPO_ROOT/stackunderflow/skills"
    case "$kind" in
        bare) ;;
        current|modified)
            for dir in "$shipped"/*/; do
                local name; name="$(basename "$dir")"
                [ -f "$dir/SKILL.md" ] || continue
                mkdir -p "$dest/.claude/skills/$name"
                cp "$dir/SKILL.md" "$dest/.claude/skills/$name/SKILL.md"
            done
            if [ "$kind" = "modified" ]; then
                local first
                first="$(basename "$(ls -d "$shipped"/*/ | sort | head -1)")"
                printf 'LOCAL EDIT — this must survive without --skills-force\n' \
                    > "$dest/.claude/skills/$first/SKILL.md"
            fi
            ;;
        *) return 1 ;;
    esac
    return 0
}

# Run ONE implementation to the boot line and interrupt it.
#
# `setsid` puts the child in its own process group so the INT reaches the whole
# tree — the Rust side spawns `stax-server` (DIV-308) and the Python side runs
# uvicorn on a thread; a bare `kill $pid` would orphan the former. This is
# wave-5 finding 13 restated: `kill $!` does not kill what the child spawned.
run_side() {
    local bin="$1" home="$2" out="$3" err="$4" rc_file="$5"; shift 5
    (
        cd "$home" || exit 90
        HOME="$home" STACKUNDERFLOW_HOME="$home/state" \
            setsid "$bin" "$@" >"$out" 2>"$err" </dev/null &
        child=$!
        # Wait for the boot line rather than sleeping a flat interval: the
        # reference's own 1.0 s sleep is what DIV-306 is about, and a differ
        # should not inherit a race it is measuring.
        for _ in $(seq 1 "$((BOOT_WAIT * 10))"); do
            grep -q 'Ctrl+C to stop' "$out" 2>/dev/null && break
            kill -0 "$child" 2>/dev/null || break
            sleep 0.1
        done
        kill -INT -- "-$child" 2>/dev/null
        sleep 0.6
        kill -KILL -- "-$child" 2>/dev/null
        wait "$child" 2>/dev/null
        echo $? > "$rc_file"
    )
}

# `$1` id, `$2` seed kind, then the argv both sides get.
run_scenario() {
    local id="$1" seed="$2"; shift 2
    if [ -n "$ONLY" ]; then
        case "$id" in *"$ONLY"*) ;; *) return 0 ;; esac
    fi

    local base="$WORK/$id"
    local py_home="$base/py" rs_home="$base/rs"
    mkdir -p "$py_home" "$rs_home"
    seed_home "$py_home" "$seed" || { setup_fail "$id" "seed"; return 1; }
    seed_home "$rs_home" "$seed" || { setup_fail "$id" "seed"; return 1; }

    run_side "$PY_BIN" "$py_home" "$base/py.out" "$base/py.err" "$base/py.rc" "$@"
    run_side "$RS_BIN" "$rs_home" "$base/rs.out" "$base/rs.err" "$base/rs.rc" "$@"

    # stdout is compared up to the boot line: everything after it is the
    # shutdown path, whose exact bytes depend on WHEN the signal landed.
    local py_cut="$base/py.cut" rs_cut="$base/rs.cut"
    sed -n '1,/Ctrl+C to stop/p' "$base/py.out" > "$py_cut"
    sed -n '1,/Ctrl+C to stop/p' "$base/rs.out" > "$rs_cut"

    local ok=1 notes=""
    if ! grep -q 'StackUnderflow is live at' "$py_cut"; then
        ok=0; notes="$notes\n  python never reached the boot line"
    fi
    if ! grep -q 'StackUnderflow is live at' "$rs_cut"; then
        ok=0; notes="$notes\n  rust never reached the boot line"
    fi
    cmp -s "$py_cut" "$rs_cut" || { ok=0; notes="$notes\n  stdout differs"; }

    # The ONE normalisation this differ performs, and it is counted every run.
    #
    # `server.py` calls `logging.basicConfig(level=INFO)`, so the reference's
    # lifespan narrates itself on stderr: services initialised, the v008
    # migration's row count, the price book, the ingest result. Four of those
    # five lines ARE DIV-305 — surfaces this wave deliberately did not port —
    # and the fifth is the logging framework itself. Comparing them would fail
    # every run for a reason already in the ledger.
    #
    # Scoped, symmetric, and narrow: only lines matching Python's default
    # `%(asctime)s - %(name)s - %(levelname)s - ` prefix are dropped, from BOTH
    # sides. The exposure warning, Click's usage blocks and any panic keep their
    # bytes, because none of them is shaped like that. Wave-8's lesson, restated:
    # an output normalisation must be scoped to the surface that justified it.
    local py_e="$base/py.err.norm" rs_e="$base/rs.err.norm"
    local log_re='^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2},[0-9]{3} - .* - [A-Z]+ - '
    grep -Ev "$log_re" "$base/py.err" > "$py_e" 2>/dev/null
    grep -Ev "$log_re" "$base/rs.err" > "$rs_e" 2>/dev/null
    local dropped_py dropped_rs
    dropped_py=$(( $(wc -l < "$base/py.err") - $(wc -l < "$py_e") ))
    dropped_rs=$(( $(wc -l < "$base/rs.err") - $(wc -l < "$rs_e") ))
    log_lines_dropped=$((log_lines_dropped + dropped_py + dropped_rs))
    if [ "$dropped_rs" -gt 0 ]; then
        ok=0
        notes="$notes\n  the PORT emitted $dropped_rs python-logging-shaped line(s) — it has no logging framework, so this normalisation must never fire on the rust side"
    fi
    cmp -s "$py_e" "$rs_e" || { ok=0; notes="$notes\n  stderr differs (after the logging filter)"; }

    local tree_diff
    tree_diff="$(diff -r "$py_home/.claude/skills" "$rs_home/.claude/skills" 2>&1)" \
        || { ok=0; notes="$notes\n  the skills trees differ"; }

    # DIV-303: `config.json` carries `__version__` and a wall clock. Its KEYS are
    # comparable; its bytes never will be, not even between two runs of one
    # implementation.
    local py_keys rs_keys
    py_keys="$(config_keys "$py_home/state/config.json")"
    rs_keys="$(config_keys "$rs_home/state/config.json")"
    if [ "$py_keys" != "$rs_keys" ]; then
        ok=0; notes="$notes\n  config.json keys differ: [$py_keys] vs [$rs_keys]"
    fi

    if [ "$ok" = 1 ]; then
        pass=$((pass + 1))
        printf '  ok    %-28s %s\n' "$id" "$(grep -o 'live at .*' "$py_cut" | head -1)"
        [ "$KEEP" = 1 ] || rm -rf "$base"
        return 0
    fi

    fail=$((fail + 1)); failed_ids+=("$id")
    {
        printf '=== %s ===\n' "$id"
        printf 'argv: %s\n' "$*"
        # shellcheck disable=SC2059 — `notes` is built with literal \n markers.
        printf "$notes\n"
        printf -- '--- stdout (python) vs (rust) ---\n'
        diff -u "$py_cut" "$rs_cut" | head -60
        printf -- '\n--- stderr (python) vs (rust) ---\n'
        diff -u "$base/py.err" "$base/rs.err" | head -40
        printf -- '\n--- skills tree ---\n%s\n' "$tree_diff"
    } > "$WORK/diffs/$id.diff"
    printf '  FAIL  %-28s see %s\n' "$id" "$WORK/diffs/$id.diff"
    return 1
}

config_keys() {
    [ -f "$1" ] || { printf '<absent>'; return; }
    python3 - "$1" <<'PY' 2>/dev/null || printf '<unreadable>'
import json, sys
with open(sys.argv[1]) as fh:
    print(",".join(sorted(json.load(fh))))
PY
}

setup_fail() {
    fail=$((fail + 1)); failed_ids+=("$1")
    printf '  SETUP %-28s (%s)\n' "$1" "$2"
}

echo "init-differ: python=$PY_BIN"
echo "             rust=$RS_BIN"
echo "             port=$PORT (probed free)"
echo "             work=$WORK"

printf '\n=== start ===\n'
run_scenario "start-headless" bare start --headless --port "$PORT" --no-watcher --no-lock
run_scenario "start-fresh"    bare start --headless --port "$PORT" --fresh --no-watcher --no-lock

printf '\n=== init (no skills) ===\n'
run_scenario "init-plain" bare init --no-browser --port "$PORT"

printf '\n=== init --install-skills ===\n'
run_scenario "init-skills-fresh" bare \
    init --no-browser --port "$PORT" --install-skills --skills-dest ".claude/skills"
run_scenario "init-skills-current" current \
    init --no-browser --port "$PORT" --install-skills --skills-dest ".claude/skills"
run_scenario "init-skills-modified" modified \
    init --no-browser --port "$PORT" --install-skills --skills-dest ".claude/skills"
run_scenario "init-skills-force" modified \
    init --no-browser --port "$PORT" --install-skills --skills-force --skills-dest ".claude/skills"

total=$((pass + fail))
printf '\n=== init/start tally ===\n'
printf 'scenarios: %s   identical: %s   DIVERGENT: %s\n' "$total" "$pass" "$fail"
printf 'python-logging lines filtered: %s (rust side must always be 0)\n' "$log_lines_dropped"
if [ "$fail" -gt 0 ]; then
    printf '\ndivergent:\n'
    printf '  %s\n' "${failed_ids[@]}"
    printf '\ndiffs: %s\n' "$WORK/diffs"
    exit 1
fi
printf 'both implementations booted on :%s and printed the same bytes.\n' "$PORT"
exit 0

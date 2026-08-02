#!/usr/bin/env bash
# The telephone differ — byte parity for the agent-to-agent messaging surface.
#
# `stax msg send` / `stax msg inbox` and the hook interject that delivers a
# cross-machine message into a *running* agent turn (agent-remotes.md Phase 3).
# For every row in `parity/telephone-cases.txt` this runs BOTH implementations
# with the same arguments, each against its OWN private copy of the same seeded
# `$STACKUNDERFLOW_HOME`, and compares stdout, stderr, the exit code, and the
# resulting home tree.
#
# ── THE REFERENCE LIVES ON ANOTHER BRANCH ────────────────────────────────────
#
# The telephone is NOT in this worktree's frozen `stackunderflow/`. It landed on
# `feat/unified-python` (commits 2d81ce3 + c30109a), which is the branch every
# Python change goes to from now on. So this differ, alone among the campaign's
# harnesses, does NOT default to `$REPO_ROOT` for its reference: it needs a
# checkout of that branch, and it says so rather than silently comparing against
# a tree where `msg` does not exist.
#
#   git worktree add /tmp/unified-ref feat/unified-python
#   STAX_PARITY_PY_PATH=/tmp/unified-ref rust/telephone-differ.sh
#
# `STAX_PARITY_PY_BIN` still points at the venv's `stackunderflow` console
# script (the default is the same one `parity-cli.sh` uses). That is safe with
# `PYTHONPATH` set: a console script puts its own `bin/` on `sys.path[0]`, never
# the cwd, so the PYTHONPATH entry wins over the venv's editable `.pth`. Running
# `python -m` from a checkout would NOT be safe, and that is why this uses the
# script.
#
# **`parity-cli.sh`'s own default pin is untouched.** Its rows keep comparing
# against the frozen tree, exactly as they did; the telephone gets its own
# harness rather than an override on the shared one — the same precedent
# `hooks-parity.sh` and `sync-parity.sh` set for a surface with its own state
# and its own budget.
#
# ── NO NETWORK, EVER ─────────────────────────────────────────────────────────
#
# `msg send`'s success path spawns `ssh`. Nothing here does. The send rows are
# the legs that return BEFORE anything is spawned (an unparseable `--to`), and
# the argv `put` *would* have exec'd is compared as a value through the two
# probes — `parity/telephone_probe.py` intercepts `subprocess.run` and
# `stax-telephone-parity` prints `put_invocation`. Same shape `sync-parity.sh`
# established, for the same reason.
#
# ── Three sections ───────────────────────────────────────────────────────────
#
#   cli    `msg inbox` / `msg send` through the two real binaries
#   hook   `hooks run <inject-id>` through both, incl. the seven ids that must
#          NOT touch the inbox
#   probe  the payload writer, the ssh argv, and the two clock formatters
#
# Usage
#   rust/telephone-differ.sh                 # everything
#   rust/telephone-differ.sh --only T-inbox  # id substring filter
#   rust/telephone-differ.sh --build-state   # (re)build the seeded homes
#   rust/telephone-differ.sh --list          # print the corpus and exit
#   rust/telephone-differ.sh --keep          # keep the per-case scratch dirs
#
# Exit: 0 when every case is byte-identical, 1 on any divergence, 2 on setup
# failure. NOT wired into ci.sh — it needs a checkout of another branch, which
# gate 0's clean-checkout build cannot assume; standalone, like the hook differ.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
STATE_DIR="${STAX_PARITY_STATE_DIR:-$HERE/.parity-state}"
TEL_DIR="$STATE_DIR/telephone"
SEEDS="$TEL_DIR/seeds"
DIFFS="$TEL_DIR/diffs"
CASES="${STAX_TELEPHONE_CASES:-$HERE/parity/telephone-cases.txt}"
RS_BIN="${STAX_TELEPHONE_RS_BIN:-$HERE/target/release/stax}"
RS_HOOK_BIN="${STAX_HOOK_RS_BIN:-$HERE/target/release/stax-hooks}"
RS_PROBE="${STAX_TELEPHONE_RS_PROBE:-$HERE/target/release/stax-telephone-parity}"
PY_PROBE="$HERE/parity/telephone_probe.py"

ONLY=""
DO_BUILD=0
DO_LIST=0
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --build-state) DO_BUILD=1; shift ;;
        --list) DO_LIST=1; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,60p' "$0"; exit 0 ;;
        *) echo "telephone-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

parse_rows() {
    grep -v '^[[:space:]]*#' "$CASES" | grep -v '^[[:space:]]*$'
}

if [ "$DO_LIST" = 1 ]; then
    parse_rows | awk -F'|' '{gsub(/^[ \t]+|[ \t]+$/,"",$1); gsub(/^[ \t]+|[ \t]+$/,"",$2); printf "%-22s %s\n", $1, $2}'
    exit 0
fi

# ── setup ────────────────────────────────────────────────────────────────────

[ -x "$PY_INTERP" ] || PY_INTERP="$(command -v python3 || true)"
if [ -z "$PY_INTERP" ] || [ ! -x "$PY_INTERP" ]; then
    echo "telephone-differ: SETUP FAILURE — no Python interpreter" >&2
    exit 2
fi

if [ "$DO_BUILD" = 1 ]; then
    "$PY_INTERP" "$HERE/parity/build_telephone_state.py" "$SEEDS" --force || exit 2
    exit 0
fi

if [ ! -x "$PY_BIN" ]; then
    echo "telephone-differ: SETUP FAILURE — no Python CLI at $PY_BIN" >&2
    echo "            (set STAX_PARITY_PY_BIN)" >&2
    exit 2
fi

# Determinism: the same pins gate 4 carries, so a divergence is the port's.
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
unset STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS
unset STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS
unset STACKUNDERFLOW_PROACTIVE_ENABLED
unset STACKUNDERFLOW_PROACTIVE_DISABLED
unset STAX_ANCHOR_DB

# The reference is the branch the telephone lives on — see the header. Unlike
# every other harness there is no sane default here, so an unset override is
# checked for the FEATURE rather than assumed away.
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

if ! "$PY_BIN" msg --help >/dev/null 2>&1; then
    echo "telephone-differ: SETUP FAILURE — the reference has no \`msg\` group." >&2
    echo "            The telephone lives on feat/unified-python. Point" >&2
    echo "            STAX_PARITY_PY_PATH at a checkout of it:" >&2
    echo "              git worktree add /tmp/unified-ref feat/unified-python" >&2
    echo "              STAX_PARITY_PY_PATH=/tmp/unified-ref $0" >&2
    exit 2
fi

for binary in "$RS_BIN" "$RS_HOOK_BIN" "$RS_PROBE"; do
    if [ ! -x "$binary" ]; then
        echo "telephone-differ: building the release binaries (the gate compares shipped bytes)"
        if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
        ( cd "$HERE" && cargo build --release -p stax-cli -p stax-hooks --quiet ) || exit 2
        break
    fi
done

if [ ! -f "$SEEDS/.built" ]; then
    echo "telephone-differ: seeding homes"
    "$PY_INTERP" "$HERE/parity/build_telephone_state.py" "$SEEDS" --force || exit 2
fi

rm -rf "$DIFFS"; mkdir -p "$DIFFS"

pass=0; fail=0; known=0; skipped=0
failed_ids=()

# The home tree, in two resolutions, and the split is deliberate.
#
# Under `inbox/` every file is md5'd: filenames (`.json` vs `.seen.json`) plus
# contents ARE the inbox's whole state, and comparing them is what makes
# `--ack` and the hook's mark-seen side effect provable rather than asserted.
#
# Everything else is listed by NAME only. A capture hook creates and writes
# `store.db` on its first fire, and that file carries rowids and a
# `datetime.now(UTC)` — the hook differ compares those with its own masking, and
# re-comparing them here would make this harness fail on another surface's
# clock. Names-only still catches the thing this differ must catch: a file
# appearing on one side and not the other.
dump_tree() {
    local root="$1"
    ( cd "$root" 2>/dev/null || return 0
      find inbox -type f -print0 2>/dev/null | LC_ALL=C sort -z | xargs -0 -r md5sum 2>/dev/null
      find . -type f -not -path './inbox/*' -print 2>/dev/null | LC_ALL=C sort | sed 's/^/name-only /'
    ) || true
}

# ── one case ─────────────────────────────────────────────────────────────────

run_case() {
    local id="$1" kind="$2" seed="$3" argv="$4" stdin="${5:-}"
    local known_open=0
    case "$id" in
        !*) known_open=1; id="${id#!}" ;;
    esac

    local work; work="$(mktemp -d)"
    local py_rc rs_rc

    case "$kind" in
    cli|hook)
        if [ ! -d "$SEEDS/$seed" ]; then
            skipped=$((skipped + 1))
            printf '  SKIP  %-22s no seed %s\n' "$id" "$seed"
            rm -rf "$work"; return 0
        fi
        # Both sides get their own copy of the same seed AT THE SAME PATH:
        # `$STACKUNDERFLOW_HOME` is echoed nowhere, but a differing path would
        # still be a differing input, and the tree diff needs a common root.
        cp -r "$SEEDS/$seed" "$work/py-home"
        cp -r "$SEEDS/$seed" "$work/rs-home"
        ;;
    probe) ;;
    *)
        echo "telephone-differ: unknown kind '$kind' on row $id" >&2
        return 1 ;;
    esac

    case "$kind" in
    cli)
        # shellcheck disable=SC2086
        eval "set -- $argv"
        env STACKUNDERFLOW_HOME="$work/py-home" \
            "$PY_BIN" "$@" </dev/null >"$work/py.out" 2>"$work/py.err"
        py_rc=$?
        env STACKUNDERFLOW_HOME="$work/rs-home" \
            "$RS_BIN" "$@" </dev/null >"$work/rs.out" 2>"$work/rs.err"
        rs_rc=$?
        ;;
    hook)
        printf '%s' "$stdin" > "$work/stdin"
        env STACKUNDERFLOW_HOME="$work/py-home" \
            "$PY_BIN" hooks run "$argv" <"$work/stdin" >"$work/py.out" 2>"$work/py.err"
        py_rc=$?
        env STACKUNDERFLOW_HOME="$work/rs-home" \
            "$RS_HOOK_BIN" hooks run "$argv" <"$work/stdin" >"$work/rs.out" 2>"$work/rs.err"
        rs_rc=$?
        ;;
    probe)
        # shellcheck disable=SC2086
        eval "set -- $argv"
        "$PY_INTERP" "$PY_PROBE" "$@" >"$work/py.out" 2>"$work/py.err"
        py_rc=$?
        "$RS_PROBE" "$@" >"$work/rs.out" 2>"$work/rs.err"
        rs_rc=$?
        ;;
    esac

    local ok=1 why=""
    cmp -s "$work/py.out" "$work/rs.out" || { ok=0; why="stdout"; }
    cmp -s "$work/py.err" "$work/rs.err" || { ok=0; why="$why stderr"; }
    [ "$py_rc" = "$rs_rc" ] || { ok=0; why="$why exit($py_rc vs $rs_rc)"; }

    if [ "$kind" != "probe" ]; then
        dump_tree "$work/py-home" | sed "s#$work/py-home##" > "$work/py.tree"
        dump_tree "$work/rs-home" | sed "s#$work/rs-home##" > "$work/rs.tree"
        cmp -s "$work/py.tree" "$work/rs.tree" || { ok=0; why="$why tree"; }
    fi

    if [ "$ok" = 1 ]; then
        if [ "$known_open" = 1 ]; then
            known=$((known + 1))
            printf '  OPEN  %-22s (known-open, currently identical)\n' "$id"
        else
            pass=$((pass + 1))
        fi
        [ "$KEEP" = 1 ] || rm -rf "$work"
        return 0
    fi

    {
        printf '=== %s  (%s, seed=%s)\n' "$id" "$kind" "$seed"
        printf -- '--- argv: %s\n' "$argv"
        [ -n "$stdin" ] && printf -- '--- stdin: %s\n' "$stdin"
        printf -- '--- diverged on: %s\n' "$why"
        printf -- '--- stdout diff (python | rust)\n'
        diff -u "$work/py.out" "$work/rs.out"
        printf -- '--- stderr diff (python | rust)\n'
        diff -u "$work/py.err" "$work/rs.err"
        if [ "$kind" != "probe" ]; then
            printf -- '--- home tree diff (python | rust)\n'
            diff -u "$work/py.tree" "$work/rs.tree"
        fi
    } > "$DIFFS/$id.diff" 2>&1

    if [ "$known_open" = 1 ]; then
        known=$((known + 1))
        printf '  OPEN  %-22s %s\n' "$id" "$why"
        [ "$KEEP" = 1 ] || rm -rf "$work"
        return 0
    fi
    fail=$((fail + 1))
    failed_ids+=("$id")
    printf '  FAIL  %-22s %s\n' "$id" "$why"
    [ "$KEEP" = 1 ] || rm -rf "$work"
    return 1
}

# ── the bench ────────────────────────────────────────────────────────────────

echo "telephone-differ: reference $PY_BIN"
echo "                  PYTHONPATH=${PYTHONPATH%%:*}"
echo "                  port      $RS_BIN / $RS_HOOK_BIN"
echo

section=""
while IFS='|' read -r id kind seed argv stdin; do
    id="$(printf '%s' "$id" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    kind="$(printf '%s' "$kind" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    seed="$(printf '%s' "$seed" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    argv="$(printf '%s' "$argv" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    stdin="$(printf '%s' "${stdin:-}" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    [ -z "$id" ] && continue
    if [ -n "$ONLY" ] && [ "${id#*"$ONLY"}" = "$id" ]; then continue; fi
    if [ "$kind" != "$section" ]; then
        section="$kind"
        printf '── %s ──\n' "$kind"
    fi
    run_case "$id" "$kind" "$seed" "$argv" "$stdin"
done < <(parse_rows)

echo
total=$((pass + fail + known + skipped))
printf 'telephone-differ: %d cases · %d identical · %d divergent · %d known-open · %d skipped\n' \
    "$total" "$pass" "$fail" "$known" "$skipped"
if [ "$fail" -gt 0 ]; then
    printf '  failed: %s\n' "${failed_ids[*]}"
    printf '  diffs under %s\n' "$DIFFS"
    exit 1
fi
exit 0

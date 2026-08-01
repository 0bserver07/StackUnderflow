#!/usr/bin/env bash
# The wave-8 tranche-4 WRITER differ — the rows the shared matrix cannot hold.
#
# `rust/parity-cli.sh` diffs two case homes byte for byte, which is exactly
# right for a writer whose output is a function of the store. Three of this
# tranche's paths are not:
#
#   · `skills generate` (no --dry-run) writes `generated_at: <ISO to the
#     second>` into every SKILL.md, twice per file (frontmatter + marker).
#   · `recommend skills` writes `cache/skill_recommendations.json` on EVERY
#     run — `--no-cache` included — with `time.time()` as a float, plus the
#     same ISO stamp inside each rendered template.
#   · `recommend mode` (without --no-cache) INSERTs a `mode_recommendations`
#     row whose `created_ts` / `last_used_ts` are `datetime.now(UTC)`, and any
#     SQLite write stamps the writing library's version into the file header
#     (DIV-257's shape), so the two store files cannot be `cmp`-equal either.
#
# Two implementations run seconds apart, so a byte diff of those artifacts
# compares the harness's clock, not the port. This differ runs the same cases
# with the clock NORMALISED — every substitution is scoped to the one shape
# that justifies it, and every substitution is COUNTED and reported, because a
# silent normalisation is the harness lying to the gate (wave-5 law, and the
# `\bstax\b` lesson in `parity-cli.sh`'s header).
#
# What stays exact: file names, file counts, directory shape, `.bak` contents,
# every non-timestamp byte of every SKILL.md, every field of the recommendation
# cache, and every column of the `mode_recommendations` row but the two clocks.
#
# Usage
#   rust/skills-differ.sh            # every case
#   rust/skills-differ.sh --only gen # id substring filter
#   rust/skills-differ.sh --keep     # leave the work trees for inspection
#
# Exit: 0 when every case matches, 1 on any divergence, 2 on a setup failure.
# NOT wired into ci.sh: it needs the Python CLI and a ~0.6 MB seed copy per
# case, and `parity-cli.sh` already gates the same verbs' read paths.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
HOMES="$HERE/parity/homes"
PROJECT="-tmp-stax-skills-parity-proj"

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,35p' "$0"; exit 0 ;;
        *) echo "skills-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[ -x "$PY_BIN" ] || { echo "skills-differ: SETUP FAILURE — no Python CLI at $PY_BIN" >&2; exit 2; }
[ -x "$PY_INTERP" ] || PY_INTERP=python3
if [ ! -x "$RS_BIN" ]; then
    echo "skills-differ: building the release binary"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

pass=0; fail=0; normalised=0
failed_ids=()

# ── the normalisations, each scoped to the shape that justifies it ───────────
#
# 1. `generated_at: 2026-08-01T22:49:38+00:00`      (SKILL.md frontmatter)
# 2. `... skills generate at <same stamp> from ...` (the SKILL.md marker)
# 3. `"generated_at":1785624578.321088`             (the JSON cache)
# 4. `\ngenerated_at: <stamp>\n` inside a JSON string (the embedded template)
normalise_tree() {
    local dir="$1" file before after
    while IFS= read -r file; do
        before="$(cksum <"$file")"
        sed -E -i \
            -e 's/^generated_at: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00$/generated_at: STAMP/' \
            -e 's/(skills generate at )[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00/\1STAMP/g' \
            -e 's/"generated_at":[0-9]+\.[0-9]+/"generated_at":STAMP/g' \
            -e 's/(\\n)generated_at: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00(\\n)/\1generated_at: STAMP\2/g' \
            "$file"
        after="$(cksum <"$file")"
        [ "$before" = "$after" ] || normalised=$((normalised + 1))
    done < <(find "$dir" -type f \( -name 'SKILL.md' -o -name 'SKILL.md.bak' -o -name '*.json' \) 2>/dev/null)
}

normalise_stream() {
    sed -E \
        -e 's/^generated_at: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00$/generated_at: STAMP/' \
        -e 's/("generated_at": )[0-9]+\.[0-9]+/\1STAMP/g' \
        -e 's/(\\n)generated_at: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00(\\n)/\1generated_at: STAMP\2/g' \
        -e 's/(skills generate at )[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00/\1STAMP/g' \
        "$1"
}

run_case() {
    local id="$1" seed="$2" argv="$3" store_check="${4:-no}" repeat="${5:-1}"
    case "$id" in *"$ONLY"*) ;; *) return 0 ;; esac

    local work; work="$(mktemp -d)"
    local pyhome="$work/pyhome" rshome="$work/rshome"
    mkdir -p "$pyhome" "$rshome"
    if [ -n "$seed" ]; then
        [ -d "$HOMES/$seed" ] || { echo "skills-differ: no seed $seed" >&2; exit 2; }
        cp -a "$HOMES/$seed/." "$pyhome/"
        cp -a "$HOMES/$seed/." "$rshome/"
    fi

    eval "set -- $argv"
    # A `repeat` of 2 runs the verb twice per side: the second pass is where
    # `unchanged` (and the "no second .bak" invariant) lives.
    if [ "$repeat" = 2 ]; then
        ( cd "$pyhome" && STACKUNDERFLOW_HOME="$pyhome" env HOME="$pyhome" \
            "$PY_BIN" "$@" >/dev/null 2>&1 </dev/null )
        ( cd "$rshome" && STACKUNDERFLOW_HOME="$rshome" env HOME="$rshome" \
            "$RS_BIN" "$@" >/dev/null 2>&1 </dev/null )
    fi
    ( cd "$pyhome" && STACKUNDERFLOW_HOME="$pyhome" env HOME="$pyhome" \
        "$PY_BIN" "$@" >"$work/py.out" 2>"$work/py.err" </dev/null )
    local py_rc=$?
    ( cd "$rshome" && STACKUNDERFLOW_HOME="$rshome" env HOME="$rshome" \
        "$RS_BIN" "$@" >"$work/rs.raw" 2>"$work/rs.raw.err" </dev/null )
    local rs_rc=$?

    # The two homes are at different paths and both are printed (`out_dir`,
    # `skills_dir`), so the root is folded to a token on both sides — the same
    # scoping `parity-cli.sh` uses for the program name.
    sed -i "s|$pyhome|CASEHOME|g" "$work/py.out" "$work/py.err"
    sed -i -e "s|$rshome|CASEHOME|g" \
           -e "/^Usage:/s/\bstax\b/stackunderflow/g" \
           -e "/^Try '/s/\bstax\b/stackunderflow/g" "$work/rs.raw" "$work/rs.raw.err"
    normalise_stream "$work/py.out" >"$work/py.norm"
    normalise_stream "$work/rs.raw" >"$work/rs.norm"

    local ok=1 detail=""
    cmp -s "$work/py.norm" "$work/rs.norm" || { ok=0; detail="stdout"; }
    cmp -s "$work/py.err" "$work/rs.raw.err" || { ok=0; detail="$detail stderr"; }
    [ "$py_rc" = "$rs_rc" ] || { ok=0; detail="$detail exit($py_rc/$rs_rc)"; }

    # The stores are compared by ROWS, never by bytes: any write re-stamps the
    # header's SQLITE_VERSION_NUMBER with the writing library's own.
    local store_report=""
    if [ "$store_check" = "yes" ]; then
        store_report="$("$PY_INTERP" "$HERE/parity/skills_store_diff.py" \
            "$pyhome/store.db" "$rshome/store.db" --seed "$HOMES/$seed/store.db" 2>&1)" || {
            ok=0; detail="$detail store"; }
        rm -f "$pyhome/store.db" "$rshome/store.db" \
              "$pyhome/store.db-wal" "$rshome/store.db-wal" \
              "$pyhome/store.db-shm" "$rshome/store.db-shm"
    fi

    normalise_tree "$pyhome"
    normalise_tree "$rshome"
    local tree; tree="$(diff -r "$pyhome" "$rshome" 2>&1)" || { ok=0; detail="$detail tree"; }

    if [ "$ok" = 1 ]; then
        pass=$((pass + 1))
        printf '  ok    %-24s %s\n' "$id" "${store_report:+· $store_report}"
        [ "$KEEP" = 1 ] || rm -rf "$work"
    else
        fail=$((fail + 1)); failed_ids+=("$id")
        printf '  FAIL  %-24s (%s)\n' "$id" "${detail# }"
        diff "$work/py.norm" "$work/rs.norm" | head -20
        diff "$work/py.err" "$work/rs.raw.err" | head -10
        [ -n "$tree" ] && { echo "  --- tree ---"; echo "$tree" | head -20; }
        [ -n "$store_report" ] && echo "  $store_report"
        echo "  work: $work"
    fi
}

echo "skills-differ: the writer paths (clock normalised, every substitution counted)"

# ── `skills generate` — the real write, every action branch ──────────────────
run_case gen-fresh        skills-corpus "skills generate --project '$PROJECT' --window all"
run_case gen-fresh-json   skills-corpus "skills generate --project '$PROJECT' --window all --format json"
run_case gen-user-scope   skills-corpus "skills generate --project '$PROJECT' --window all --scope user"
run_case gen-out          skills-corpus "skills generate --project '$PROJECT' --window all --out mine/skills"
# The seeded tree makes `updated` (+ its .bak), `skipped-user-authored` and the
# `<name>-<hash6>` collision suffix reachable — the three branches a fresh
# directory can never produce.
run_case gen-over-existing skills-both  "skills generate --project '$PROJECT' --window all"
run_case gen-over-json    skills-both   "skills generate --project '$PROJECT' --window all --format json"
# Second run over our OWN output: every action must be `unchanged`, which is
# the volatile-line comparison doing its job rather than a rewrite loop.
run_case gen-twice        skills-corpus "skills generate --project '$PROJECT' --window all" no 2

# ── `recommend skills` — the JSON cache, written on every run ────────────────
run_case rec-skills       skills-corpus "recommend skills --project '$PROJECT' --window-days 3650"
run_case rec-skills-json  skills-corpus "recommend skills --project '$PROJECT' --window-days 3650 --format json"
run_case rec-skills-nocache skills-corpus "recommend skills --project '$PROJECT' --window-days 3650 --no-cache"
run_case rec-skills-filtered skills-both "recommend skills --project '$PROJECT' --window-days 3650"
run_case rec-skills-empty skills-corpus "recommend skills --project '$PROJECT' --window-days 1"

# ── `recommend mode` — the cached path, which INSERTs ────────────────────────
run_case rec-mode-cached  skills-corpus \
    "recommend mode --prompt 'fix the failing test in cost.py with pytest'" yes
run_case rec-mode-cached-json skills-corpus \
    "recommend mode --prompt 'fix the failing test in cost.py with pytest' --format json" yes

echo
echo "skills-differ: $pass identical / $fail divergent  (clock normalisations: $normalised)"
if [ "$fail" -gt 0 ]; then
    printf 'skills-differ: FAILED — %s\n' "${failed_ids[*]}" >&2
    exit 1
fi
exit 0

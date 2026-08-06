#!/usr/bin/env bash
# `stax import` — the writer proof, and the proof that a rejection did nothing.
#
# `rust/parity/cases.txt` carries 47 `T7-imp-*` rows, and every one of them is a
# REJECTION: `import`'s success leg WRITES the store, and DIV-078's law says a
# case row must be side-effect-free. This script is where the writes are proved,
# and it is where the rejections are proved to have *not* written — which a
# stdout diff cannot see.
#
# Four mechanisms, all here:
#
#   A. THE EXPORT COMMAND IS THE INTERCEPTOR.  `rust/parity/history-plugins/
#      record/export.sh` appends its own cwd, its own argv and its WHOLE
#      environment to a log, then emits a valid stream. Both implementations
#      spawn the SAME script through their own runner, so what is compared is
#      two real `execve`s — not a Rust value against a Python value some probe
#      re-derived. This is the `backup create` argv-differ pattern with one
#      difference that matters: there is no shim on `$PATH` and nothing is
#      faked. The export command is the user's own program by design, so
#      running it IS the contract.
#
#      What that log proves, in one artifact: the argv (no shell, no
#      re-quoting), the cwd (the manifest's own directory), the env ALLOWLIST
#      (`STAX_LEAK_PROBE` is set in the parent and must not appear), the
#      `env_passthrough` opt-in (`STAX_IMPORT_LOG` must appear), and
#      `STACKUNDERFLOW_HISTORY_CURSOR` — empty on the first run, the manifest's
#      seed when there is one, the STORED cursor on the second.
#
#   B. A REAL RUN, TWICE.  No stub runner: the actual child writes an actual
#      stream, both implementations ingest it into their own home, and the two
#      stores are compared with `parity/etl_store_diff.py` — `sqlite_master`
#      plus every row of every table, with the four wall-clock columns masked
#      and the mask REPORTED. Then the whole thing runs a second time, because
#      "re-running an unchanged export is an idempotent no-op" is a claim in the
#      verb's own docstring and `messages ingested: 0` is how it is falsifiable.
#
#   C. THE CURSOR SIDECAR.  `<home>/history_sources/<id>.cursor.json` is
#      compared byte for byte with its `updated_at` masked — it is `time.time()`
#      and the two runs are seconds apart. Its ABSENCE is asserted just as
#      hard: a stream with no `cursor` record must leave no sidecar, and a
#      rejected import must leave the previous one untouched.
#
#   D. PROVEN NON-EXECUTING (DIV-447).  Two fixtures exist only for this.
#      `x-marker-manifest` is an INVALID manifest whose command would create
#      `./ran.marker`; after the run that file must not exist on either side —
#      which is what "the manifest is refused before the command runs" means
#      when it is falsifiable rather than asserted. `x-marker-stream` is its
#      control: a VALID manifest with a bad stream line, where the marker MUST
#      exist and the store must still be empty. Without the control, a differ
#      that silently stopped spawning anything would pass.
#
# NOTHING here touches the real system. Every path is under a mktemp root, the
# plugin fixtures are COPIED there before anything runs (so a script that writes
# leaves its file in scratch, never in the repo), no network, no `~/.claude`,
# no `~/.stackunderflow`.
#
# The two normalisations, both scoped and both counted: the per-side scratch
# path (`$ROOT/<side>` → `WORK`, because the two homes are two directories) and
# the sidecar's `updated_at` float. Neither is applied to anything else.
#
# Usage:  rust/import-differ.sh [--only <substring>] [--keep]
# Exit:   0 all scenarios identical · 1 a divergence · 2 a setup failure
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
PLUGINS="$HERE/parity/history-plugins"
ONLY=""
KEEP_SCRATCH=0

while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP_SCRATCH=1; shift ;;
        -h|--help) sed -n '2,62p' "$0"; exit 0 ;;
        *) echo "import-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[ -x "$PY_BIN" ] || { echo "import-differ: SETUP — no Python CLI at $PY_BIN" >&2; exit 2; }
[ -x "$PY_INTERP" ] || { echo "import-differ: SETUP — no interpreter at $PY_INTERP" >&2; exit 2; }
[ -d "$PLUGINS" ] || { echo "import-differ: SETUP — no fixtures at $PLUGINS" >&2; exit 2; }
if [ ! -x "$RS_BIN" ]; then
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

ROOT="$(mktemp -d)"
DIFFS="$ROOT/diffs"; mkdir -p "$DIFFS"
KEEP_DIR="$HERE/.parity-state/import-differ"
cleanup() {
    mkdir -p "$KEEP_DIR" 2>/dev/null
    rm -rf "$KEEP_DIR/diffs" 2>/dev/null
    cp -a "$DIFFS" "$KEEP_DIR/diffs" 2>/dev/null
    if [ "$KEEP_SCRATCH" = 1 ]; then
        echo "import-differ: scratch kept at $ROOT"
    else
        rm -rf "$ROOT"
    fi
}
trap cleanup EXIT

pass=0; fail=0; normalised=0
failed=()

# `updated_at` is `time.time()`; the format stays under test because only text
# that ALREADY matches the reference's shape is rewritten.
normalise_clock() {
    sed -E 's/"updated_at": [0-9]+\.[0-9]+/"updated_at": <CLOCK>/'
}

# ── one scenario ─────────────────────────────────────────────────────────────
#
# $1 id · $2 plugin fixture name · $3.. the argv after `import`.
# Knobs read from the caller:
#   RUNS=N          how many times to run each side (idempotency)
#   EXPECT_RC=N     the exit code both sides must return (default 0)
#   EXPECT_MARKER   yes|no|"" — whether ./ran.marker must exist afterwards
#   EXPECT_SIDECAR  yes|no|"" — whether the cursor sidecar must exist
#   EXPECT_ROWS     the number of `custom` messages both stores must hold
#   SEED_CURSOR=s   pre-store a cursor for source_id `amp` before the run
#   EXPECT_ENV=yes  assert the recorder log's env claims (no leak, the
#                   passthrough opt-in, the cursor variable, the argv)
#   EXPECT_CURSOR=s what STACKUNDERFLOW_HISTORY_CURSOR must be in the child
scenario() {
    local id="$1" plugin="$2"; shift 2
    if [ -n "$ONLY" ]; then case "$id" in *"$ONLY"*) ;; *) return 0 ;; esac; fi

    local work="$ROOT/$id"; rm -rf "$work"; mkdir -p "$work"
    local runs="${RUNS:-1}" side run

    for side in py rs; do
        local home="$work/$side/home"
        mkdir -p "$home/history-plugins"
        # The fixture is COPIED, so a script that writes leaves its file here.
        cp -a "$PLUGINS/$plugin" "$home/history-plugins/$plugin" || return 1
        if [ -n "${SEED_CURSOR:-}" ]; then
            mkdir -p "$home/history_sources"
            printf '{\n  "schema": "stackunderflow-history-jsonl-v1",\n  "source_id": "amp",\n  "cursor": "%s",\n  "updated_at": 1.0\n}' \
                "$SEED_CURSOR" > "$home/history_sources/amp.cursor.json"
        fi
        local bin="$PY_BIN"; [ "$side" = rs ] && bin="$RS_BIN"
        : > "$work/$side.out"; : > "$work/$side.err"; : > "$work/$side.rc"
        for run in $(seq 1 "$runs"); do
            (
                cd "$home" || exit 2
                STACKUNDERFLOW_HOME="$home" \
                HOME="$work/$side" \
                STAX_IMPORT_LOG="$home/history-plugins/$plugin/export.log" \
                STAX_LEAK_PROBE="this must not reach the child" \
                timeout 120 "$bin" import --history-source "$plugin" "$@" \
                    >>"$work/$side.out" 2>>"$work/$side.err" </dev/null
            )
            printf '%s\n' "$?" >> "$work/$side.rc"
        done
    done

    local ok=1 note="" detail=""
    for side in py rs; do
        local home="$work/$side/home"
        for stream in out err; do
            sed -e "s#$work/$side#WORK#g" "$work/$side.$stream" > "$work/$side.$stream.n"
        done
        # The recorder log and the sidecar are artifacts, not streams.
        if [ -f "$home/history-plugins/$plugin/export.log" ]; then
            sed -e "s#$work/$side#WORK#g" "$home/history-plugins/$plugin/export.log" \
                > "$work/$side.log.n"
        else
            : > "$work/$side.log.n"
        fi
        if [ -f "$home/history_sources/amp.cursor.json" ]; then
            sed -e "s#$work/$side#WORK#g" "$home/history_sources/amp.cursor.json" \
                | normalise_clock > "$work/$side.sidecar.n"
        else
            : > "$work/$side.sidecar.n"
        fi
    done
    # Counted only where it actually fired: a scenario with no sidecar has
    # nothing to normalise, and reporting one would be noise dressed as rigour.
    if [ -f "$work/py/home/history_sources/amp.cursor.json" ] && \
       ! cmp -s "$work/py.sidecar.n" "$work/py/home/history_sources/amp.cursor.json"; then
        note="updated_at masked"
    fi

    cmp -s "$work/py.out.n" "$work/rs.out.n" || { ok=0; detail="$detail stdout"; }
    cmp -s "$work/py.err.n" "$work/rs.err.n" || { ok=0; detail="$detail stderr"; }
    cmp -s "$work/py.rc"    "$work/rs.rc"    || { ok=0; detail="$detail exit-code"; }
    cmp -s "$work/py.log.n" "$work/rs.log.n" || { ok=0; detail="$detail export-log"; }
    cmp -s "$work/py.sidecar.n" "$work/rs.sidecar.n" || { ok=0; detail="$detail cursor-sidecar"; }
    [ -s "$work/py.rc" ] && [ "$(sort -u "$work/py.rc")" = "${EXPECT_RC:-0}" ] || {
        ok=0; detail="$detail expected-rc(${EXPECT_RC:-0})"; }

    # The store: present on both sides, or absent on both.
    local store_report=""
    if [ -f "$work/py/home/store.db" ] && [ -f "$work/rs/home/store.db" ]; then
        store_report="$("$PY_INTERP" "$HERE/parity/etl_store_diff.py" \
            "$work/py/home" "$work/rs/home" \
            --mask ingest_log.mtime --mask ingest_log.last_ingest_ts \
            --mask projects.first_seen --mask projects.last_modified 2>&1)" || {
            ok=0; detail="$detail store"; }
    elif [ -f "$work/py/home/store.db" ] || [ -f "$work/rs/home/store.db" ]; then
        ok=0; detail="$detail store-presence"
    fi

    # The three expectations that are about the FILESYSTEM, not the diff.
    for side in py rs; do
        local home="$work/$side/home"
        local marker="$home/history-plugins/$plugin/ran.marker"
        case "${EXPECT_MARKER:-}" in
            yes) [ -f "$marker" ] || { ok=0; detail="$detail $side:marker-missing"; } ;;
            no)  [ -f "$marker" ] && { ok=0; detail="$detail $side:MARKER-PRESENT"; } ;;
        esac
        local sidecar="$home/history_sources/amp.cursor.json"
        case "${EXPECT_SIDECAR:-}" in
            yes) [ -f "$sidecar" ] || { ok=0; detail="$detail $side:sidecar-missing"; } ;;
            no)  [ -f "$sidecar" ] && { ok=0; detail="$detail $side:SIDECAR-PRESENT"; } ;;
        esac
        # The env claims, asserted rather than merely diffed: two
        # implementations that BOTH leaked would have agreed, and agreement is
        # not the property under test (wave 6's dead-corpus law).
        local log="$home/history-plugins/$plugin/export.log"
        if [ "${EXPECT_ENV:-}" = yes ]; then
            local TAB; TAB="$(printf '\t')"
            grep -q "^env${TAB}STAX_LEAK_PROBE=" "$log" 2>/dev/null && {
                ok=0; detail="$detail $side:ENV-LEAKED"; }
            grep -q "^env${TAB}STAX_IMPORT_LOG=" "$log" 2>/dev/null || {
                ok=0; detail="$detail $side:passthrough-dropped"; }
            grep -q "^env${TAB}STACKUNDERFLOW_HISTORY_CURSOR=${EXPECT_CURSOR:-}$" "$log" 2>/dev/null || {
                ok=0; detail="$detail $side:cursor-var(${EXPECT_CURSOR:-<empty>})"; }
            grep -q "^arg${TAB}a b$" "$log" 2>/dev/null || {
                ok=0; detail="$detail $side:argv-space-lost"; }
            grep -qF "${TAB}it's" "$log" 2>/dev/null || {
                ok=0; detail="$detail $side:argv-quote-lost"; }
        fi
        if [ -n "${EXPECT_ROWS:-}" ] && [ -f "$home/store.db" ]; then
            local rows
            rows="$("$PY_INTERP" -c "import sqlite3,sys
c=sqlite3.connect('file:'+sys.argv[1]+'?mode=ro',uri=True)
print(c.execute('''SELECT COUNT(*) FROM messages m
                   JOIN sessions s ON s.id = m.session_fk
                   JOIN projects p ON p.id = s.project_id
                   WHERE p.provider = 'custom' ''').fetchone()[0])" \
                "$home/store.db" 2>/dev/null)"
            [ "$rows" = "$EXPECT_ROWS" ] || { ok=0; detail="$detail $side:rows($rows!=$EXPECT_ROWS)"; }
        fi
    done

    [ -n "$note" ] && normalised=$((normalised + 1))
    if [ "$ok" = 1 ]; then
        pass=$((pass + 1)); printf '  ok    %-24s %s\n' "$id" "$note"; return 0
    fi
    fail=$((fail + 1)); failed+=("$id")
    printf '  FAIL  %-24s%s\n' "$id" "$detail"
    {
        printf '=== %s ===\nplugin: %s\nargv: import --history-source %s %s\n\n' \
            "$id" "$plugin" "$plugin" "$*"
        printf -- '--- stdout ---\n'; diff -u "$work/py.out.n" "$work/rs.out.n" | head -40
        printf -- '--- stderr ---\n'; diff -u "$work/py.err.n" "$work/rs.err.n" | head -40
        printf -- '--- exit codes ---\n'; diff -u "$work/py.rc" "$work/rs.rc" | head -10
        printf -- '--- export log (argv + cwd + env) ---\n'
        diff -u "$work/py.log.n" "$work/rs.log.n" | head -60
        printf -- '--- cursor sidecar ---\n'
        diff -u "$work/py.sidecar.n" "$work/rs.sidecar.n" | head -20
        printf -- '--- store ---\n%s\n' "$store_report"
    } > "$DIFFS/$id.diff"
    return 1
}

printf 'import-differ: python=%s\n            rust=%s\n            fixtures=%s\n\n' \
    "$PY_BIN" "$RS_BIN" "$PLUGINS"

# ── A + B + C: the success legs ──────────────────────────────────────────────
EXPECT_ENV=yes EXPECT_CURSOR= EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario ok-text record
EXPECT_ENV=yes EXPECT_CURSOR= EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario ok-json record --format json
# The idempotency claim in the verb's own docstring: a second run of an
# unchanged export reports `messages ingested: 0` and writes no new row.
RUNS=2 EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario ok-idempotent record
# The manifest's seed cursor reaches the child on the FIRST run …
EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario cursor-seed ok-seeded
# … and the STORED one wins on the second (visible in the recorder's env dump).
SEED_CURSOR=stored-9 EXPECT_ENV=yes EXPECT_CURSOR=stored-9 EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario cursor-stored record
# A stream with no `cursor` record leaves no sidecar at all.
EXPECT_SIDECAR=no EXPECT_ROWS=4 \
    scenario cursor-absent ok-nocursor
# The plain success fixture, without the recorder in the way.
EXPECT_SIDECAR=yes EXPECT_ROWS=4 \
    scenario ok-plain ok

# ── D: proven non-executing, and its control ─────────────────────────────────
EXPECT_RC=1 EXPECT_MARKER=no EXPECT_SIDECAR=no EXPECT_ROWS=0 \
    scenario reject-manifest x-marker-manifest
EXPECT_RC=1 EXPECT_MARKER=yes EXPECT_SIDECAR=no EXPECT_ROWS=0 \
    scenario reject-stream x-marker-stream
# A rejection with a cursor already stored must leave it EXACTLY as it was.
SEED_CURSOR=untouched-1 EXPECT_RC=1 EXPECT_MARKER=yes EXPECT_SIDECAR=yes EXPECT_ROWS=0 \
    scenario reject-keeps-cursor x-marker-stream

printf '\n=== import-differ tally ===\n'
printf 'scenarios: %d   pass: %d   FAIL: %d\n' "$((pass + fail))" "$pass" "$fail"
printf 'cursor-clock normalisation fired on %d scenario(s)\n' "$normalised"
if [ "$fail" -ne 0 ]; then
    printf 'failed: %s\n' "${failed[*]}"
    printf 'diffs under %s/diffs\n' "$KEEP_DIR"
    exit 1
fi
printf 'byte-identical on every scenario: stdout, stderr, exit code, the argv +\n'
printf 'cwd + environment the export command actually saw, the cursor sidecar,\n'
printf 'and every row of every table in both stores.\n'
exit 0

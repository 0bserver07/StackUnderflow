#!/usr/bin/env bash
# `backup create` / `backup restore` / `backup auto` — the writer proof.
#
# These three verbs cannot be rows in `parity-cli.sh`'s matrix: `create` names
# its destination `datetime.now().strftime("%Y%m%d-%H%M%S")`, so two runs
# seconds apart disagree by construction, and `auto`'s interesting branch is
# macOS-only. The proof is therefore three separate mechanisms, all here:
#
#   A. ARGV INTERCEPTION.  A fake `rsync` (and a fake `ssh`) is placed FIRST on
#      $PATH. It appends its own `"$@"` to a log, writes an injected stderr and
#      exits with an injected code. Both implementations are intercepted by the
#      SAME shim, so what is compared is two real spawns — not a Rust value
#      against a Python value that some probe re-derived. The injected exit code
#      is how rsync's 23 / 24 tolerances get crossed on both sides; a constant a
#      port copies needs a case that crosses it (wave-6 law).
#
#   B. A REAL RUN.  No shim: the actual rsync copies an actual scratch
#      `~/.claude` into an actual backup root, twice, and the two resulting trees
#      are diffed recursively.
#
#   C. A GENERATED FILE.  `rust/parity/backup_auto_plist.py` drives the REAL
#      `cli.py` with `platform.system()` faked to Darwin and `subprocess.run`
#      stubbed, captures the launchd plist, and `cargo test --test plist_golden`
#      diffs `backup::darwin_plist` against it byte for byte.
#
# NOTHING here touches the real system. No launchctl, no crontab, no ssh, no
# network, no `~/.claude`, no `~/Library`. Every path is under a mktemp root and
# every external program that would leave the box is a shim.
#
# The one normalisation: the `%Y%m%d-%H%M%S` stamp in a destination path is
# rewritten to `TIMESTAMP` on both sides before comparison, and every case that
# needed it is counted and reported. It is scoped to that shape and nothing else.
#
# Usage:  rust/backup-differ.sh [--only <substring>]
# Exit:   0 all scenarios identical · 1 a divergence · 2 a setup failure
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"
ONLY=""

while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP_SCRATCH=1; shift ;;
        -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
        *) echo "backup-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[ -x "$PY_BIN" ] || { echo "backup-differ: SETUP — no Python CLI at $PY_BIN" >&2; exit 2; }
if [ ! -x "$RS_BIN" ]; then
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

ROOT="$(mktemp -d)"
# The evidence outlives a failure: diffs are copied out before the cleanup, and
# `--keep` leaves the whole scratch tree for inspection.
KEEP_DIR="$HERE/.parity-state/backup-differ"
cleanup() {
    mkdir -p "$KEEP_DIR" 2>/dev/null
    rm -rf "$KEEP_DIR/diffs" 2>/dev/null
    cp -a "$ROOT/diffs" "$KEEP_DIR/diffs" 2>/dev/null
    if [ "${KEEP_SCRATCH:-0}" = 1 ]; then
        echo "backup-differ: scratch kept at $ROOT"
    else
        rm -rf "$ROOT"
    fi
}
trap cleanup EXIT
DIFFS="$ROOT/diffs"; mkdir -p "$DIFFS"

pass=0; fail=0; normalised=0
failed=()

# ── the shims ────────────────────────────────────────────────────────────────
#
# `$SHIM/rsync` logs one line per invocation — the argv, tab-separated so an
# argument containing a space stays one field — and answers with the injected
# code and stderr. `$SHIM/ssh` does the same. Neither copies anything, which is
# the point: scenario A is about the command line, not the transfer.
SHIM="$ROOT/shim"; mkdir -p "$SHIM"
cat > "$SHIM/rsync" <<'SHIMEOF'
#!/usr/bin/env bash
{ printf 'rsync'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$STAX_SHIM_LOG"
[ -n "${STAX_SHIM_STDERR:-}" ] && printf '%s\n' "$STAX_SHIM_STDERR" >&2
exit "${STAX_SHIM_RC:-0}"
SHIMEOF
cat > "$SHIM/ssh" <<'SHIMEOF'
#!/usr/bin/env bash
{ printf 'ssh'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$STAX_SHIM_LOG"
exit "${STAX_SHIM_SSH_RC:-0}"
SHIMEOF
chmod +x "$SHIM/rsync" "$SHIM/ssh"

# A scratch `~/.claude` with a little of everything the excludes name, so an
# exclude that silently stopped working shows up as a copied file.
seed_claude() {
    local dir="$1"
    mkdir -p "$dir/projects/alpha" "$dir/debug" "$dir/plugins" "$dir/cache" "$dir/todos"
    printf '{"a": 1}\n'  > "$dir/projects/alpha/one.jsonl"
    printf '{"b": 2}\n'  > "$dir/projects/alpha/two.jsonl"
    printf 'settings\n'  > "$dir/settings.json"
    printf 'todo\n'      > "$dir/todos/t.json"
    printf 'noisy\n'     > "$dir/debug/log.txt"
    printf 'binary\n'    > "$dir/plugins/thing"
    printf 'rebuildable\n' > "$dir/cache/c"
}

# A `$STACKUNDERFLOW_HOME` holding the four critical artifacts `_capture_state`
# copies, so the "State: captured …" line has something to say and the SQLite
# online-backup path is exercised rather than skipped.
#
# Built ONCE and copied to both sides. Two separate builds would not do: the
# CPython and rusqlite SQLite libraries stamp different `SQLITE_VERSION_NUMBER`
# values into a database they create (3053001 vs 3053002 on this host, one byte
# at offset 96), so per-side seeding would have made the sources differ before
# either implementation ran and turned a green comparison into an accident.
STATE_SEED="$ROOT/state-seed"
build_state_seed() {
    [ -d "$STATE_SEED" ] && return 0
    mkdir -p "$STATE_SEED"
    "$PY_INTERP" - "$STATE_SEED" <<'PYEOF'
import sqlite3, sys, pathlib
root = pathlib.Path(sys.argv[1])
for name in ("store.db", "search_index.db", "qa_pairs.db"):
    conn = sqlite3.connect(root / name)
    conn.execute("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)")
    conn.execute("INSERT INTO t (v) VALUES ('x')")
    conn.commit()
    conn.close()
(root / "tags.json").write_text('{"tags": []}\n')
PYEOF
}

seed_state() {
    local dir="$1"
    build_state_seed
    mkdir -p "$dir"
    cp -a "$STATE_SEED/." "$dir/"
}

# `%Y%m%d-%H%M%S` → `TIMESTAMP`. Scoped to exactly that shape.
normalise() {
    sed -E 's/[0-9]{8}-[0-9]{6}/TIMESTAMP/g'
}

# ── one scenario ─────────────────────────────────────────────────────────────
#
# $1 id · $2 rsync exit code · $3 rsync stderr · $4.. the argv.
# Environment knobs read from the caller: SEED_PREVIOUS, SSH_RC, STDIN_TEXT,
# NO_SHIM (scenario B), NO_STATE.
scenario() {
    local id="$1" rc="$2" shim_stderr="$3"; shift 3
    if [ -n "$ONLY" ]; then case "$id" in *"$ONLY"*) ;; *) return 0 ;; esac; fi

    local work="$ROOT/$id"; rm -rf "$work"; mkdir -p "$work"
    local side out
    for side in py rs; do
        local home="$work/$side/home" claude="$work/$side/claude"
        mkdir -p "$home"
        seed_claude "$claude"
        [ "${NO_STATE:-0}" = 1 ] || seed_state "$home"
        if [ "${SEED_PREVIOUS:-0}" = 1 ]; then
            mkdir -p "$home/backups/20200101-000000/projects/alpha"
            printf 'old\n' > "$home/backups/20200101-000000/projects/alpha/one.jsonl"
        fi
        local log="$work/$side.argv"; : > "$log"
        local run_path="$PATH"
        [ "${NO_SHIM:-0}" = 1 ] || run_path="$SHIM:$PATH"
        local bin="$PY_BIN"; [ "$side" = rs ] && bin="$RS_BIN"
        (
            cd "$claude" || exit 2
            PATH="$run_path" \
            STACKUNDERFLOW_HOME="$home" \
            CLAUDE_CONFIG_DIR="$claude" \
            HOME="$work/$side" \
            STAX_SHIM_LOG="$log" \
            STAX_SHIM_RC="$rc" \
            STAX_SHIM_STDERR="$shim_stderr" \
            STAX_SHIM_SSH_RC="${SSH_RC:-0}" \
            timeout 120 "$bin" "$@" \
                >"$work/$side.out" 2>"$work/$side.err" <<< "${STDIN_TEXT:-}"
        )
        printf '%s\n' "$?" > "$work/$side.rc"
    done

    # Both sides get the same normalisation, and the two homes differ only in
    # their `py`/`rs` path component, which is normalised out too.
    local ok=1 note=""
    for side in py rs; do
        sed -e "s#$work/$side#WORK#g" "$work/$side.out" | normalise > "$work/$side.out.n"
        sed -e "s#$work/$side#WORK#g" "$work/$side.err" | normalise > "$work/$side.err.n"
        sed -e "s#$work/$side#WORK#g" "$work/$side.argv" | normalise > "$work/$side.argv.n"
    done
    cmp -s "$work/py.out.n" "$work/py.out" || note="stamp"
    cmp -s "$work/py.out.n" "$work/rs.out.n" || ok=0
    cmp -s "$work/py.err.n" "$work/rs.err.n" || ok=0
    cmp -s "$work/py.argv.n" "$work/rs.argv.n" || ok=0
    cmp -s "$work/py.rc" "$work/rs.rc" || ok=0

    # Scenario B additionally diffs the trees the real rsync produced.
    local tree_diff=""
    if [ "${NO_SHIM:-0}" = 1 ]; then
        # The restored `~/.claude` is the artifact `restore` exists to produce,
        # and it must be identical whichever implementation wrote it. (For
        # `create` it is an invariant instead: the source tree is read-only, so
        # a difference here would mean one side mutated what it was copying.)
        local claude_diff
        claude_diff="$(diff -r "$work/py/claude" "$work/rs/claude" 2>&1)" || {
            tree_diff="restored ~/.claude differs:
$claude_diff"; ok=0; }
        local py_backup rs_backup
        py_backup="$(find "$work/py/home/backups" -mindepth 1 -maxdepth 1 -type d | sort | tail -1)"
        rs_backup="$(find "$work/rs/home/backups" -mindepth 1 -maxdepth 1 -type d | sort | tail -1)"
        if [ -z "$py_backup" ] || [ -z "$rs_backup" ]; then
            tree_diff="one side produced no backup directory (py='$py_backup' rs='$rs_backup')"
            ok=0
        else
            # Everything except the databases must be byte-identical.
            tree_diff="$(diff -r -x '*.db' "$py_backup" "$rs_backup" 2>&1)" || ok=0
            # The databases get a STRICTER check than `cmp`, not a weaker one.
            # Both sides start from the SAME seeded artifacts (built once and
            # copied), so `_capture_state`'s SQLite online-backup output differs
            # only where the writing *library* stamps itself: the 4-byte
            # `SQLITE_VERSION_NUMBER` at offset 96 (3053001 for CPython's
            # SQLite, 3053002 for rusqlite's bundled one). DIV-257. The helper
            # asserts that offsets 96..99 are the ONLY difference and that the
            # two databases are otherwise identical page for page AND row for
            # row — anything else fails the scenario.
            # Only `create` captures state; a `restore` scenario's newest backup
            # directory is the untouched seed, so there is nothing to compare.
            if [ -d "$py_backup/stackunderflow-state" ] || [ -d "$rs_backup/stackunderflow-state" ]; then
                local db_report
                db_report="$("$PY_INTERP" "$HERE/parity/sqlite_header_diff.py" \
                    "$py_backup/stackunderflow-state" "$rs_backup/stackunderflow-state" 2>&1)" || {
                    tree_diff="$tree_diff
$db_report"; ok=0; }
            fi
        fi
    fi

    [ -n "$note" ] && normalised=$((normalised + 1))
    if [ "$ok" = 1 ]; then
        pass=$((pass + 1)); printf '  ok    %-26s %s\n' "$id" "$note"; return 0
    fi
    fail=$((fail + 1)); failed+=("$id")
    {
        printf '=== %s ===\nargv: %s\n\n' "$id" "$*"
        printf -- '--- stdout ---\n'; diff -u "$work/py.out.n" "$work/rs.out.n" | head -60
        printf -- '\n--- stderr ---\n'; diff -u "$work/py.err.n" "$work/rs.err.n" | head -40
        printf -- '\n--- spawned argv ---\n'; diff -u "$work/py.argv.n" "$work/rs.argv.n" | head -60
        printf -- '\n--- exit ---\npy=%s rs=%s\n' "$(cat "$work/py.rc")" "$(cat "$work/rs.rc")"
        [ -n "$tree_diff" ] && printf -- '\n--- produced trees ---\n%s\n' "$tree_diff"
    } > "$DIFFS/$id.diff"
    printf '  FAIL  %-26s (see %s)\n' "$id" "$DIFFS/$id.diff"
    return 1
}

echo "backup-differ: python=$PY_BIN"
echo "               rust=$RS_BIN"
echo "               scratch=$ROOT"

# ── A. argv interception ─────────────────────────────────────────────────────
echo
echo "=== A. argv construction (fake rsync/ssh first on PATH) ==="
scenario A-create-fresh        0  ""  backup create
SEED_PREVIOUS=1 \
scenario A-create-linkdest     0  ""  backup create
scenario A-create-label        0  ""  backup create --label 'a-b_c'
# DIV-234's class, twice: the sanitiser runs BEFORE `if label:`, so a label of
# only punctuation becomes '' and the name loses its suffix, while '0' — falsy
# in a shell, truthy in Python — keeps it.
scenario A-create-label-punct  0  ""  backup create --label '///'
scenario A-create-label-zero   0  ""  backup create --label '0'
scenario A-create-label-empty  0  ""  backup create --label ''
scenario A-create-label-mixed  0  ""  backup create --label 'a/b c!d'
scenario A-create-keep1        0  ""  backup create --keep 1
SEED_PREVIOUS=1 \
scenario A-create-keep1-prune  0  ""  backup create --keep 1
scenario A-create-rc24        24  "file has vanished: /x/a.jsonl
rsync error: some files vanished (code 24)"  backup create
scenario A-create-rc23        23  "rsync: link_stat \"/x/b\" failed
rsync error: partial transfer (code 23)"     backup create
scenario A-create-rc23-quiet  23  ""  backup create
scenario A-create-rc1          1  "rsync: command not understood
rsync error: syntax error (code 1)"          backup create
scenario A-create-to-bad       0  ""  backup create --to 'not-a-url'
scenario A-create-to-empty     0  ""  backup create --to ''
scenario A-create-to-ssh       0  ""  backup create --to 'ssh://box/srv/backups'
SEED_PREVIOUS=1 \
scenario A-create-to-ssh-prev  0  ""  backup create --to 'ssh://u@box:2222/srv/backups'
SSH_RC=1 \
scenario A-create-to-ssh-mkdir 0  ""  backup create --to 'ssh://box/srv/backups'
NO_STATE=1 \
scenario A-create-no-state     0  ""  backup create

SEED_PREVIOUS=1 STDIN_TEXT=y \
scenario A-restore-yes         0  ""  backup restore 20200101-000000
SEED_PREVIOUS=1 STDIN_TEXT=n \
scenario A-restore-no          0  ""  backup restore 20200101-000000
SEED_PREVIOUS=1 STDIN_TEXT=y \
scenario A-restore-rc2         2  "rsync: chgrp failed"  backup restore 20200101-000000
SEED_PREVIOUS=1 STDIN_TEXT=yes \
scenario A-restore-word        0  ""  backup restore 20200101-000000

# ── B. a real run, real rsync, two trees ─────────────────────────────────────
echo
echo "=== B. real local runs (no shim; the produced trees are diffed) ==="
NO_SHIM=1 \
scenario B-create-real         0  ""  backup create
NO_SHIM=1 SEED_PREVIOUS=1 \
scenario B-create-real-link    0  ""  backup create --label nightly
NO_SHIM=1 SEED_PREVIOUS=1 STDIN_TEXT=y \
scenario B-restore-real        0  ""  backup restore 20200101-000000

# ── C. the generated launchd plist ───────────────────────────────────────────
echo
echo "=== C. generated file: the launchd plist ==="
plist_ok=1
PLIST_WORK="$ROOT/plist"; mkdir -p "$PLIST_WORK"
PLIST_BIN="/usr/local/bin/stackunderflow"
PLIST_STATE="$PLIST_WORK/state"
if ! "$PY_INTERP" "$HERE/parity/backup_auto_plist.py" \
        "$PLIST_WORK/home" "$PLIST_BIN" "$PLIST_WORK/reference.plist" \
        > "$PLIST_WORK/py.out" 2> "$PLIST_WORK/py.err"; then
    echo "  FAIL  plist-generate            (see $PLIST_WORK/py.err)"
    sed 's/^/        /' "$PLIST_WORK/py.err" | head -20
    plist_ok=0
else
    # The probe's `$STACKUNDERFLOW_HOME` is what lands in the plist's log paths.
    PLIST_STATE="$PLIST_WORK/home/state"
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    if ( cd "$HERE" && \
         STAX_PLIST_GOLDEN="$PLIST_WORK/reference.plist" \
         STAX_PLIST_BIN="$PLIST_BIN" \
         STAX_PLIST_STATE="$PLIST_STATE" \
         cargo test --release -p stax-cli --test plist_golden --quiet \
       ) > "$PLIST_WORK/rs.out" 2>&1; then
        echo "  ok    plist-golden               ($(wc -c < "$PLIST_WORK/reference.plist") B, byte-identical)"
        pass=$((pass + 1))
    else
        echo "  FAIL  plist-golden"
        cp "$PLIST_WORK/rs.out" "$DIFFS/plist-golden.diff"
        sed 's/^/        /' "$PLIST_WORK/rs.out" | head -30
        plist_ok=0
    fi
    # The reference must not have shelled out to launchctl for real.
    if grep -q 'SPAWNED: launchctl load' "$PLIST_WORK/py.err"; then
        echo "  ok    launchctl-intercepted      (the reference's spawn was stubbed, not run)"
        pass=$((pass + 1))
    else
        echo "  FAIL  launchctl-intercepted      (the probe never saw the launchctl spawn)"
        plist_ok=0
    fi
fi
[ "$plist_ok" = 1 ] || { fail=$((fail + 1)); failed+=("plist"); }

# ── tally ────────────────────────────────────────────────────────────────────
echo
echo "=== backup-differ tally ==="
printf 'scenarios: %s   pass: %s   FAIL: %s\n' "$((pass + fail))" "$pass" "$fail"
printf 'timestamp normalisation fired on %s scenario(s)\n' "$normalised"
if [ "$fail" -gt 0 ]; then
    printf '\nfailing:\n'; printf '  %s\n' "${failed[@]}"
    printf '\ndiffs: %s\n' "$DIFFS"
    # Keep the evidence: the trap would delete it.
    printf 'copied to: %s\n' "$KEEP_DIR/diffs"
    exit 1
fi
echo "every scenario identical: stdout, stderr, exit code, spawned argv, produced trees."
exit 0

#!/usr/bin/env bash
# The wave-4 ingest gate: full-ingest equivalence, Python vs Rust.
#
# Builds ONE fixture tree, copies it into two scratch homes with identical
# freshly-migrated stores, runs `run_ingest` over each — Python's from the venv,
# Rust's from `stax-ingest-parity` — and diffs projects / sessions / messages /
# usage_events / ingest_log **full-row**. The store IS the contract, so the
# comparison is of rows, not of counts.
#
#   rust/ingest-parity.sh [--keep]
#
# `--keep` leaves the scratch homes in place for inspection. Exit 0 = every
# table byte-identical; 1 = a diff; 2 = setup could not run.
#
# What the scratch homes contain, and why:
#   * the repo's `tests/mock-data/` claude + codex trees, which are the shapes
#     the Python suite itself asserts on;
#   * a sample of the maintainer's REAL `~/.claude/projects` sessions, because a
#     fixture corpus proves the parser and only real data proves the corner
#     cases (sidechains, tool-only turns, 86%-empty content_text).
# The live store under `stackunderflow-data` is never opened — the stores here
# are minted fresh by `schema.apply()`.
set -uo pipefail

cd "$(dirname "$0")"
RUST_DIR="$PWD"
REPO="$(git rev-parse --show-toplevel)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-/media/tmos-bumblebe/dev_dev/year26/jul26/StackUnderflow}"
PY="${STAX_PARITY_PY:-$PY_ROOT/.venv/bin/python}"
# How many real ~/.claude project directories to sample. 0 = fixtures only.
REAL_PROJECTS="${STAX_INGEST_REAL_PROJECTS:-6}"

if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi

KEEP=0
for arg in "$@"; do
    case "$arg" in
        --keep) KEEP=1 ;;
        *) echo "ingest-parity.sh: unknown argument '$arg'" >&2; exit 2 ;;
    esac
done

if [ ! -x "$PY" ]; then
    echo "GATE SETUP: no Python interpreter at $PY" >&2
    exit 2
fi

WORK="$(mktemp -d -t stax-ingest-parity-XXXXXX)"
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT
echo "workdir    $WORK"

# ── 1. the fixture tree ──────────────────────────────────────────────────────
TREE="$WORK/tree"
mkdir -p "$TREE/.claude/projects" "$TREE/.codex/sessions"
cp -r "$REPO/tests/mock-data/-Users-test-dev-ai-music" "$TREE/.claude/projects/"
cp -r "$REPO/tests/mock-data/codex-sessions/." "$TREE/.codex/sessions/"

if [ "$REAL_PROJECTS" -gt 0 ] && [ -d "$HOME/.claude/projects" ]; then
    # Smallest-first so the gate stays a few seconds rather than a few minutes;
    # the point is shape coverage, and `du` order is not correlated with shape.
    while IFS= read -r dir; do
        [ -n "$dir" ] || continue
        cp -r "$dir" "$TREE/.claude/projects/" 2>/dev/null || true
    done < <(du -s "$HOME"/.claude/projects/*/ 2>/dev/null \
             | sort -n | awk -v n="$REAL_PROJECTS" 'NR>2 && NR<=n+2 {print $2}')
fi
FIXTURE_FILES="$(find "$TREE" -name '*.jsonl' | wc -l)"
FIXTURE_BYTES="$(du -sb "$TREE" | cut -f1)"
echo "fixtures   files=$FIXTURE_FILES bytes=$FIXTURE_BYTES"
if [ "$FIXTURE_FILES" = 0 ]; then
    echo "GATE SETUP: the fixture tree is empty" >&2
    exit 2
fi

# ── 2. two homes, two identical fresh stores ─────────────────────────────────
# `cp -a`, not `cp -r`: the two homes must have BIT-IDENTICAL mtimes, because
# `ingest_log.mtime` is a REAL column dumped as its IEEE-754 bits and the ingest
# fast path compares it for exact equality. A plain `cp -r` gave the two copies
# timestamps 4 ms apart and the diff caught it — which is the differ working, and
# is why the fix is to make the inputs equal rather than to round the column.
for side in py rs; do
    cp -a "$TREE" "$WORK/home-$side"
    mkdir -p "$WORK/home-$side/.stackunderflow"
done
"$PY" - "$WORK" <<'PYEOF' || exit 2
import pathlib, shutil, sys
work = pathlib.Path(sys.argv[1])
from stackunderflow.store import db, schema
seed = work / "seed.db"
conn = db.connect(seed)
schema.apply(conn)
conn.close()
for side in ("py", "rs"):
    shutil.copy(seed, work / f"home-{side}" / ".stackunderflow" / "store.db")
print(f"schema     applied to both stores from one seed ({seed.stat().st_size} bytes)")
PYEOF

# One env for both sides: HOME scopes every adapter's root, and the three
# overrides below are cleared so an ambient value on the developer's machine
# cannot point one implementation at a different tree from the other.
run_scoped() {
    local home="$1"; shift
    env -u CLAUDE_CONFIG_DIR -u XDG_CONFIG_HOME -u XDG_DATA_HOME -u FACTORY_DIR \
        -u STACKUNDERFLOW_HOME -u CODEX_HOME \
        HOME="$home" "$@"
}

# ── 3. Python's pass ─────────────────────────────────────────────────────────
echo
echo "=== python run_ingest ==="
run_scoped "$WORK/home-py" "$PY" - "$WORK/home-py" <<'PYEOF' || exit 2
import pathlib, sys, time
home = pathlib.Path(sys.argv[1])
from stackunderflow.store import db
from stackunderflow.adapters import registered
from stackunderflow.ingest import run_ingest
conn = db.connect(home / ".stackunderflow" / "store.db")
started = time.perf_counter()
counts = run_ingest(conn, registered())
elapsed = (time.perf_counter() - started) * 1000
print(f"pass       elapsed_ms={elapsed:.1f}")
for provider, added in counts.items():
    print(f"provider   {provider}={added}")
print("messages   ", conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0])
print("events     ", conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0])
conn.close()
PYEOF

# ── 4. Rust's pass ───────────────────────────────────────────────────────────
echo
echo "=== rust run_ingest ==="
cargo build --release -p stax-etl --bin stax-ingest-parity --quiet || exit 2
BIN="$RUST_DIR/target/release/stax-ingest-parity"
run_scoped "$WORK/home-rs" "$BIN" ingest "$WORK/home-rs" || exit 1

# ── 5. dump + diff ───────────────────────────────────────────────────────────
echo
echo "=== dumps ==="
"$BIN" dump "$WORK/home-py/.stackunderflow/store.db" "$WORK/dump-py" >/dev/null || exit 2
"$BIN" dump "$WORK/home-rs/.stackunderflow/store.db" "$WORK/dump-rs" >/dev/null || exit 2

# The two homes are two directories, so `ingest_log.file_path` and nothing else
# carries the side's own name. Canonicalising it is not hiding a difference: it
# is removing the harness's own variable so the columns that ARE the contract —
# mtime bits, size, processed_offset, last_rowid, storage_kind — are compared on
# equal terms. Everything else in the dump is compared verbatim.
canonicalise_homes() {
    sed -i -e "s#$WORK/home-py#<HOME>#g" -e "s#$WORK/home-rs#<HOME>#g" "$1"/*.tsv
}
canonicalise_homes "$WORK/dump-py"
canonicalise_homes "$WORK/dump-rs"

# The deferred-hook gap, reported rather than hidden. `sessions.team_id` and its
# three siblings come from `claude_teams.materialize_team_metadata` (RS-2-004,
# wave 2, OPEN), which the wave-4 PostIngestHook stubs — so the columns are
# excluded from the diff above and counted here instead.
echo
echo "=== deferred hook (RS-2-004 claude_teams, DIV-042) ==="
printf '  python  %s\n' "$(grep sessions_with_team_metadata "$WORK/dump-py/deferred_hook.txt" | tr -d '\t' | sed 's/sessions_with_team_metadata/sessions with team metadata: /')"
printf '  rust    %s\n' "$(grep sessions_with_team_metadata "$WORK/dump-rs/deferred_hook.txt" | tr -d '\t' | sed 's/sessions_with_team_metadata/sessions with team metadata: /')"

echo
echo "=== per-table diff ==="
STATUS=0
for table in projects sessions messages usage_events ingest_log; do
    py="$WORK/dump-py/$table.tsv"
    rs="$WORK/dump-rs/$table.tsv"
    py_rows=$(( $(wc -l < "$py") - 1 ))
    rs_rows=$(( $(wc -l < "$rs") - 1 ))
    if diff -q "$py" "$rs" >/dev/null; then
        printf '  %-14s py=%-7s rs=%-7s  IDENTICAL\n' "$table" "$py_rows" "$rs_rows"
    else
        differing=$(diff "$py" "$rs" | grep -c '^[<>]')
        printf '  %-14s py=%-7s rs=%-7s  %s DIFFERING LINES\n' \
            "$table" "$py_rows" "$rs_rows" "$differing"
        diff "$py" "$rs" | head -20
        STATUS=1
    fi
done

# ── 6. idempotence: a second pass on both sides adds nothing ─────────────────
echo
echo "=== idempotence (second pass, both sides) ==="
run_scoped "$WORK/home-py" "$PY" - "$WORK/home-py" <<'PYEOF' || exit 2
import pathlib, sys
home = pathlib.Path(sys.argv[1])
from stackunderflow.store import db
from stackunderflow.adapters import registered
from stackunderflow.ingest import run_ingest
conn = db.connect(home / ".stackunderflow" / "store.db")
before = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
run_ingest(conn, registered())
after = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
print(f"  python         messages {before} -> {after}  {'OK' if before == after else 'REGRESSED'}")
conn.close()
PYEOF
run_scoped "$WORK/home-rs" "$BIN" ingest "$WORK/home-rs" | sed 's/^/  rust  /'

"$BIN" dump "$WORK/home-py/.stackunderflow/store.db" "$WORK/dump-py2" >/dev/null
"$BIN" dump "$WORK/home-rs/.stackunderflow/store.db" "$WORK/dump-rs2" >/dev/null
canonicalise_homes "$WORK/dump-py2"
canonicalise_homes "$WORK/dump-rs2"
for table in projects sessions messages usage_events ingest_log; do
    for side in py rs; do
        if ! diff -q "$WORK/dump-$side/$table.tsv" "$WORK/dump-${side}2/$table.tsv" >/dev/null; then
            echo "  $side $table CHANGED on the second pass — not idempotent"
            STATUS=1
        fi
    done
done
[ "$STATUS" = 0 ] && echo "  both sides unchanged by a second pass"

echo
if [ "$STATUS" = 0 ]; then
    echo "WAVE-4 INGEST GATE: GREEN — every table byte-identical, both sides idempotent"
else
    echo "WAVE-4 INGEST GATE: RED"
fi
exit "$STATUS"

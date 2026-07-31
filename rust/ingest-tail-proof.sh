#!/usr/bin/env bash
# The wave-4 live-tail proof: append a session line, time the row, check the
# watermark against a Python ingest of the same append.
#
#   rust/ingest-tail-proof.sh [--keep]
#
# The spec's gate (§4, wave 4): "live tail: write a session file, row lands
# < 400ms, watermark parity". Three things, measured in that order:
#
#   1. LATENCY — a COPY-based home, the Rust watcher running against it, a line
#      appended to a real session file. The clock starts *before* the append and
#      stops when the row is readable from a second connection, so inotify
#      delivery, the 200 ms debounce, the ingest transaction and the mart refresh
#      are all inside the number. Timing the cycle callback instead would leave
#      the debounce out and flatter the result by half the budget.
#   2. WATERMARK PARITY — a second home gets the byte-identical append and is
#      ingested by Python's `run_ingest`. The two `ingest_log` rows must agree on
#      `processed_offset`, which is the claim that a watcher-driven tail resumes
#      from the same place a batch pass would.
#   3. ROW PARITY — the appended message itself is diffed full-row between the
#      two stores, so "it landed fast" cannot pass while "it landed right" fails.
#
# Exit 0 = under budget and identical; 1 = over budget or divergent; 2 = setup.
set -uo pipefail

cd "$(dirname "$0")"
RUST_DIR="$PWD"
REPO="$(git rev-parse --show-toplevel)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-/media/tmos-bumblebe/dev_dev/year26/jul26/StackUnderflow}"
PY="${STAX_PARITY_PY:-$PY_ROOT/.venv/bin/python}"
BUDGET_MS=400

if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
[ -x "$PY" ] || { echo "GATE SETUP: no interpreter at $PY" >&2; exit 2; }

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

WORK="$(mktemp -d -t stax-tail-proof-XXXXXX)"
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT
echo "workdir    $WORK"

# ── the COPY-based home ──────────────────────────────────────────────────────
TREE="$WORK/tree"
mkdir -p "$TREE/.claude/projects"
cp -r "$REPO/tests/mock-data/-Users-test-dev-ai-music" "$TREE/.claude/projects/"
# One real project too: the append is modelled on the file's own last line, so a
# real transcript exercises the real parse path.
real="$(du -s "$HOME"/.claude/projects/*/ 2>/dev/null | sort -n | sed -n '3p' | cut -f2)"
[ -n "$real" ] && cp -a "$real" "$TREE/.claude/projects/" 2>/dev/null

for side in rs py; do
    cp -a "$TREE" "$WORK/home-$side"
    mkdir -p "$WORK/home-$side/.stackunderflow"
done

"$PY" - "$WORK" <<'PYEOF' || exit 2
import pathlib, shutil, sys
work = pathlib.Path(sys.argv[1])
from stackunderflow.store import db, schema
seed = work / "seed.db"
conn = db.connect(seed); schema.apply(conn); conn.close()
for side in ("rs", "py"):
    shutil.copy(seed, work / f"home-{side}" / ".stackunderflow" / "store.db")
PYEOF

run_scoped() {
    local home="$1"; shift
    env -u CLAUDE_CONFIG_DIR -u XDG_CONFIG_HOME -u XDG_DATA_HOME -u FACTORY_DIR \
        -u STACKUNDERFLOW_HOME -u CODEX_HOME HOME="$home" "$@"
}

cargo build --release -p stax-etl --bin stax-ingest-parity --quiet || exit 2
BIN="$RUST_DIR/target/release/stax-ingest-parity"

# ── prime both stores with the pre-append state ──────────────────────────────
echo
echo "=== prime (both sides ingest the tree as it stands) ==="
run_scoped "$WORK/home-rs" "$BIN" ingest "$WORK/home-rs" | sed 's/^/  rust    /'
run_scoped "$WORK/home-py" "$PY" - "$WORK/home-py" <<'PYEOF' || exit 2
import pathlib, sys
from stackunderflow.store import db
from stackunderflow.adapters import registered
from stackunderflow.ingest import run_ingest
conn = db.connect(pathlib.Path(sys.argv[1]) / ".stackunderflow" / "store.db")
counts = run_ingest(conn, registered())
print("  python   messages", conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0])
conn.close()
PYEOF

# The session file the append goes to — the same relative path on both sides.
# The append is modelled on the file's last user/assistant line, so the target
# must have one — a summary-only transcript would measure a cycle that correctly
# does nothing. Pick the largest file that carries a conversational line.
REL=""
while IFS= read -r candidate; do
    if grep -qE '"type":"(assistant|user)"' "$WORK/home-rs/$candidate"; then
        REL="$candidate"; break
    fi
done < <(cd "$WORK/home-rs" && find .claude/projects -name '*.jsonl' -size +4k -printf '%s\t%p\n' \
         | sort -rn | cut -f2)
[ -n "$REL" ] || { echo "GATE SETUP: no session file to append to" >&2; exit 2; }
echo "target     $REL"

# ── 1. the measurement ───────────────────────────────────────────────────────
echo
echo "=== live tail (rust watcher) ==="
run_scoped "$WORK/home-rs" "$BIN" tail "$WORK/home-rs" "$WORK/home-rs/$REL" 7
TAIL_STATUS=$?

# ── 2 + 3. the same append, ingested by Python ───────────────────────────────
echo
echo "=== the same append, python run_ingest ==="
# Byte-identical: copy the file the watcher's target became, so the two trees
# hold the same bytes rather than two independently-generated appends.
cp -a "$WORK/home-rs/$REL" "$WORK/home-py/$REL"
run_scoped "$WORK/home-py" "$PY" - "$WORK/home-py" <<'PYEOF' || exit 2
import pathlib, sys
from stackunderflow.store import db
from stackunderflow.adapters import registered
from stackunderflow.ingest import run_ingest
conn = db.connect(pathlib.Path(sys.argv[1]) / ".stackunderflow" / "store.db")
run_ingest(conn, registered())
print("  python   messages", conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0])
conn.close()
PYEOF

echo
echo "=== watermark + row parity ==="
"$BIN" dump "$WORK/home-rs/.stackunderflow/store.db" "$WORK/dump-rs" >/dev/null
"$BIN" dump "$WORK/home-py/.stackunderflow/store.db" "$WORK/dump-py" >/dev/null
sed -i "s#$WORK/home-rs#<HOME>#g" "$WORK/dump-rs"/*.tsv
sed -i "s#$WORK/home-py#<HOME>#g" "$WORK/dump-py"/*.tsv

STATUS=0
for table in projects sessions messages usage_events ingest_log; do
    if diff -q "$WORK/dump-py/$table.tsv" "$WORK/dump-rs/$table.tsv" >/dev/null; then
        printf '  %-14s IDENTICAL (%s rows)\n' "$table" \
            "$(( $(wc -l < "$WORK/dump-rs/$table.tsv") - 1 ))"
    else
        printf '  %-14s DIVERGENT\n' "$table"
        diff "$WORK/dump-py/$table.tsv" "$WORK/dump-rs/$table.tsv" | head -10
        STATUS=1
    fi
done

echo
if [ "$TAIL_STATUS" != 0 ]; then
    echo "LIVE-TAIL PROOF: RED — over the ${BUDGET_MS}ms budget"
    exit 1
fi
if [ "$STATUS" != 0 ]; then
    echo "LIVE-TAIL PROOF: RED — the watcher's rows are not the batch pass's rows"
    exit 1
fi
echo "LIVE-TAIL PROOF: GREEN — under ${BUDGET_MS}ms, watermark and rows identical"

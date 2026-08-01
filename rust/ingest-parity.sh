#!/usr/bin/env bash
# The wave-4 ingest gate: full-ingest equivalence, Python vs Rust.
#
# Builds ONE fixture tree, copies it into two scratch homes with identical
# freshly-migrated stores, runs `run_ingest` over each — Python's from the venv,
# Rust's from `stax-ingest-parity` — and diffs projects / sessions / messages /
# usage_events / ingest_log / agent_teams / commit_session_link **full-row**.
# The store IS the contract, so the comparison is of rows, not of counts.
#
# The last three of those are the post-ingest hook's: `sessions`' four team
# columns, `agent_teams`, and `commit_session_link`. They joined the diff when
# DIV-042 closed (the hook body — `claude_teams.materialize_team_metadata` +
# `link_commits_to_sessions` — was a stub through waves 4-6). Nothing is
# excluded from `sessions` any more.
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
# ── 1b. the agent-teams corpus ───────────────────────────────────────────────
#
# WHY THIS IS SYNTHESISED AND NOT SAMPLED. The four `sessions` team columns +
# `agent_teams` are written by the post-ingest hook, and on this machine every
# team-tagged session lives in ONE project — the 377 MB `-Users-yadkonrad-…
# jan26-StackUnderflow`, the largest of the 29. So `STAX_INGEST_REAL_PROJECTS`
# has to reach 29 before a single team column is non-NULL, and every cheaper run
# of this gate would compare four columns of NULL against four columns of NULL
# and call it identical. That is the campaign's own lesson stated twice already
# (wave 6: "a differ passing first-try on an untested constant is dead corpus";
# wave 5: "a differ that under-reads agrees by accident"), so the small legs get
# a corpus that crosses every branch the hook has:
#
#   config path      `teams/gate-team/config.json` → discover_teams
#   jsonl path       a TeamCreate/Agent transcript → discover_teams_from_jsonl,
#                    whose `config_json` is a SYNTHESISED `json.dumps` and is
#                    therefore the one byte contract with no reference file
#   Explore skip     an `Agent` block the reference refuses to make a member of
#   linker step 1    the config's `leadSessionId`            → role=lead
#   linker step 2    a session whose first record has `teamName`/`agentId`
#   spawn prompt     …whose member has NO prompt, so it falls to the TASK's
#                    `description` (discover_tasks + the owner match)
#   linker step 2.5  a worker whose first user text matches BUILDER_RE
#   linker step 3    a sidechain session whose `parentUuid` is a uuid of the
#                    lead — the fixpoint path
#   discover_tasks   `.lock` / `.highwatermark` / `notes.txt` / broken JSON,
#                    all of which must be skipped, and numeric-vs-stem ids
CLAUDE_PROJECT="$TREE/.claude/projects/-Users-test-dev-ai-music"
TEAM_CWD="/Users/test/dev/ai-music"
LEAD_SID="10000000-0000-4000-8000-000000000001"
JSONL_LEAD_SID="10000000-0000-4000-8000-000000000005"

# A user record. $1 file stem/session id, $2 uuid, $3 parentUuid (or null),
# $4 isSidechain, $5 the message text, $6 extra top-level JSON (may be empty).
team_user_record() {
    printf '{"parentUuid":%s,"isSidechain":%s,"userType":"external","cwd":"%s",' \
        "$3" "$4" "$TEAM_CWD"
    printf '"sessionId":"%s","version":"1.0.17","type":"user","uuid":"%s",' "$1" "$2"
    printf '"timestamp":"2026-04-01T00:00:00.000Z"%s,' "$6"
    printf '"message":{"role":"user","content":[{"type":"text","text":"%s"}]}}\n' "$5"
}

# The lead's own transcript — two records, so it owns two uuids.
{
    team_user_record "$LEAD_SID" "u-lead-1" null false "kick off the team" ""
    team_user_record "$LEAD_SID" "u-lead-2" '"u-lead-1"' false "second turn" ""
} > "$CLAUDE_PROJECT/$LEAD_SID.jsonl"

# Linker step 2: `teamName` + `agentId` on the FIRST record, which is the only
# record `_build_hints_for_projects` peeks at.
team_user_record "10000000-0000-4000-8000-000000000002" "u-w1-1" null false \
    "starting work" ',"teamName":"gate-team","agentId":"w1@gate-team"' \
    > "$CLAUDE_PROJECT/10000000-0000-4000-8000-000000000002.jsonl"

# Linker step 2.5: BUILDER_RE over the first user text. The backticks are the
# pattern's own delimiters and have to survive the heredoc-free quoting above.
team_user_record "10000000-0000-4000-8000-000000000003" "u-w2-1" null false \
    "You are \`w2\` on \`gate-team\`. Do the thing." "" \
    > "$CLAUDE_PROJECT/10000000-0000-4000-8000-000000000003.jsonl"

# Linker step 3: a sidechain whose parent uuid belongs to the lead.
team_user_record "10000000-0000-4000-8000-000000000004" "u-sc-1" '"u-lead-2"' true \
    "sub-sub work" "" \
    > "$CLAUDE_PROJECT/10000000-0000-4000-8000-000000000004.jsonl"

# The jsonl-fallback team: TeamCreate + two Agents, one of them an Explore that
# must NOT become a member.
{
    team_user_record "$JSONL_LEAD_SID" "u-jl-1" null false "build me a team" ""
    printf '{"parentUuid":"u-jl-1","isSidechain":false,"cwd":"%s","sessionId":"%s",' \
        "$TEAM_CWD" "$JSONL_LEAD_SID"
    printf '"type":"assistant","uuid":"u-jl-2","timestamp":"2026-04-01T00:01:02.345Z",'
    printf '"message":{"role":"assistant","model":"claude-opus-4-6","content":['
    printf '{"type":"tool_use","id":"t1","name":"TeamCreate","input":{"team_name":"jsonl-team","description":"reconstructed from the transcript"}},'
    printf '{"type":"tool_use","id":"t2","name":"Agent","input":{"team_name":"jsonl-team","name":"w9","subagent_type":"general-purpose","prompt":"the w9 spawn prompt"}},'
    printf '{"type":"tool_use","id":"t3","name":"Agent","input":{"team_name":"jsonl-team","name":"scout","subagent_type":"Explore","prompt":"never a member"}}'
    printf ']}}\n'
} > "$CLAUDE_PROJECT/$JSONL_LEAD_SID.jsonl"

mkdir -p "$TREE/.claude/teams/gate-team" "$TREE/.claude/tasks/gate-team"
# `inboxes`-only directories are what the implicit `default` team looks like —
# the reference skips them for having no config.json.
mkdir -p "$TREE/.claude/teams/default/inboxes"
cat > "$TREE/.claude/teams/gate-team/config.json" <<CONFIGEOF
{
  "leadAgentId": "lead@gate-team",
  "leadSessionId": "$LEAD_SID",
  "description": "the gate's team — em dash included on purpose",
  "createdAt": 1700000000123,
  "members": [
    {"agentId": "lead@gate-team", "name": "team-lead", "agentType": "team-lead",
     "model": "claude-opus-4-6", "cwd": "$TEAM_CWD"},
    {"agentId": "w1@gate-team", "cwd": "$TEAM_CWD"},
    {"agentId": "w2@gate-team", "name": "w2", "cwd": "$TEAM_CWD",
     "prompt": "the w2 spawn prompt"}
  ]
}
CONFIGEOF
printf '{"id":1,"owner":"w1","subject":"s","description":"the task description"}\n' \
    > "$TREE/.claude/tasks/gate-team/1.json"
printf '{"id":10,"owner":"nobody","subject":"later","description":"unowned"}\n' \
    > "$TREE/.claude/tasks/gate-team/10.json"
printf '{ broken\n' > "$TREE/.claude/tasks/gate-team/2.json"
printf 'not json at all\n' > "$TREE/.claude/tasks/gate-team/notes.txt"
printf 'lock\n' > "$TREE/.claude/tasks/gate-team/.lock"
printf '3\n' > "$TREE/.claude/tasks/gate-team/.highwatermark"

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

# The post-ingest hook's own count. `sessions.team_id` and its three siblings
# come from `claude_teams.materialize_team_metadata` (RS-2-004) via
# `ClaudeAdapter.materialize_metadata`. Until DIV-042 closed, the port stubbed
# that hook and the four columns were EXCLUDED from the `sessions` diff below
# with the gap counted here instead (41 sessions of 162 on the 1 GB corpus,
# against 0 in the port). The exclusion is gone: the columns are diffed, and
# these two numbers have to match — a hook that stopped running would collapse
# this line even if the row diff were somehow satisfied.
echo
echo "=== post-ingest hook — team metadata (RS-2-004, DIV-042 CLOSED) ==="
printf '  python  %s\n' "$(grep sessions_with_team_metadata "$WORK/dump-py/deferred_hook.txt" | tr -d '\t' | sed 's/sessions_with_team_metadata/sessions with team metadata: /')"
printf '  rust    %s\n' "$(grep sessions_with_team_metadata "$WORK/dump-rs/deferred_hook.txt" | tr -d '\t' | sed 's/sessions_with_team_metadata/sessions with team metadata: /')"
PY_TEAMED="$(awk -F'\t' '/sessions_with_team_metadata/ {print $2}' "$WORK/dump-py/deferred_hook.txt")"
RS_TEAMED="$(awk -F'\t' '/sessions_with_team_metadata/ {print $2}' "$WORK/dump-rs/deferred_hook.txt")"

echo
echo "=== per-table diff ==="
STATUS=0
if [ "$PY_TEAMED" != "$RS_TEAMED" ]; then
    echo "  team metadata  py=$PY_TEAMED rs=$RS_TEAMED  DIVERGENT (DIV-042 regressed)"
    STATUS=1
fi
for table in projects sessions messages usage_events ingest_log agent_teams commit_session_link; do
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
for table in projects sessions messages usage_events ingest_log agent_teams commit_session_link; do
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

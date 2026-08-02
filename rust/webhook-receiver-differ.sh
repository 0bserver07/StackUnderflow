#!/usr/bin/env bash
# The webhook-receiver differ — `ingest webhook serve`, both implementations.
#
# `cli.py`'s `ingest webhook serve` builds a SECOND, bare FastAPI app carrying
# only `/api/webhooks/{github,gitlab,ci}` and serves it on its own port, so a
# tunnel can reach the receiver without reaching the dashboard behind it. The
# port's twin is `stax-server --webhooks-only`, and `stax ingest webhook serve`
# spawns it (DIV-308's shape).
#
# This boots BOTH receivers — the reference on :8104, the port on :8105 — and
# compares raw HTTP responses byte for byte. **Never :8095** (the maintainer's
# Python server) and never :8096 (`stax-server`'s own default, which the
# reference happens to share with this verb — recorded, not resolved here).
#
# ── What this reaches that `endpoint-parity.sh` cannot ───────────────────────
#
# Gate 6 proves the three webhook endpoints only in their UNCONFIGURED state:
# its own header says "that is the only leg the parity harness can reach",
# because a configured leg needs the same secret exported into BOTH server
# processes and `endpoint-parity.sh` is shared ground no batch may re-shape.
# This harness owns both spawns, so it exports the secrets and crosses the
# **403 bad-signature** leg on all three endpoints — the branch where the
# length-check-then-`compare_digest` order lives.
#
# Every case here is SIDE-EFFECT-FREE by construction (DIV-059): 503 returns
# before the body is read, 403 before it is parsed, and neither touches the
# store. No case carries a VALID signature, so nothing is ever written — which
# is why the two receivers can share one store copy and why this script never
# needs the per-case homes the CLI harness has.
#
# ── No outbound network ──────────────────────────────────────────────────────
#
# Both listeners bind 127.0.0.1 and every request is a loopback socket. Nothing
# resolves a name, nothing leaves the machine.
#
# Usage
#   rust/webhook-receiver-differ.sh                # everything
#   rust/webhook-receiver-differ.sh --only 503     # id substring filter
#   rust/webhook-receiver-differ.sh --keep         # leave the servers running
#
# Exit: 0 when every case is byte-identical, 1 on any divergence, 2 on setup
# failure. NOT wired into ci.sh — it binds two ports for ~20 seconds.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
PY_INTERP="${STAX_PARITY_PY_INTERP:-$PY_ROOT/.venv/bin/python}"
STATE_DIR="${STAX_PARITY_STATE_DIR:-$HERE/.parity-state}"
WORK="$STATE_DIR/webhook-receiver"
DIFFS="$WORK/diffs"
RS_BIN="${STAX_TELEPHONE_RS_BIN:-$HERE/target/release/stax}"
RAW="$HERE/parity/raw_request.py"

PY_PORT="${STAX_RECEIVER_PY_PORT:-8104}"
RS_PORT="${STAX_RECEIVER_RS_PORT:-8105}"

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
        *) echo "webhook-receiver-differ: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

for port in "$PY_PORT" "$RS_PORT"; do
    if [ "$port" = "8095" ]; then
        echo "webhook-receiver-differ: refusing :8095 — it is the maintainer's server" >&2
        exit 2
    fi
done

[ -x "$PY_INTERP" ] || PY_INTERP="$(command -v python3 || true)"
if [ -z "$PY_INTERP" ] || [ ! -x "$PY_BIN" ]; then
    echo "webhook-receiver-differ: SETUP FAILURE — no reference Python" >&2
    exit 2
fi

export LC_ALL=C.UTF-8 LANG=C.UTF-8 TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

# The secrets both receivers see. Exported here so BOTH processes inherit the
# same values — the thing gate 6 could not arrange, and the whole reason the
# 403 leg is reachable at all.
export STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET="parity-github-secret"
export STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET="parity-gitlab-secret"
export STACKUNDERFLOW_CI_WEBHOOK_SECRET="parity-ci-secret"

rm -rf "$WORK"; mkdir -p "$DIFFS"

# One store, shared: every case is side-effect-free, so there is nothing for the
# two receivers to race on. The store comes from the parity states when they
# exist (the real migrations), and is otherwise built by the reference's own
# `schema.apply` — never transcribed.
HOME_DIR="$WORK/home"
mkdir -p "$HOME_DIR"
if [ -f "$STATE_DIR/fresh/store.db" ]; then
    cp "$STATE_DIR/fresh/store.db" "$HOME_DIR/store.db"
else
    "$PY_INTERP" - "$HOME_DIR/store.db" <<'PYSCHEMA' || exit 2
import sqlite3, sys
from stackunderflow.store import schema
conn = sqlite3.connect(sys.argv[1])
schema.apply(conn)
conn.commit()
conn.close()
PYSCHEMA
fi
export STACKUNDERFLOW_HOME="$HOME_DIR"

if [ ! -x "$RS_BIN" ]; then
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli -p stax-server --quiet ) || exit 2
fi
# `stax ingest webhook serve` resolves `stax-server` as a SIBLING of itself, so
# the release dir must carry both. Building the CLI alone would make the verb
# fall back to a bare `stax-server` on PATH, which is not this worktree's.
if [ ! -x "$(dirname "$RS_BIN")/stax-server" ]; then
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-server --quiet ) || exit 2
fi

# ── boot both receivers ──────────────────────────────────────────────────────
#
# Finding 13, paid for once already: `kill $!` on a backgrounded subshell does
# not kill what it spawned, and the next run then diffs the PREVIOUS run's
# server. `exec` inside the subshell, and a CONNECT probe (never a bind probe —
# a bind test false-positives on TIME_WAIT) before any case runs.

PY_PID=""; RS_PID=""
cleanup() {
    [ "$KEEP" = 1 ] && return 0
    [ -n "$PY_PID" ] && kill "$PY_PID" 2>/dev/null
    [ -n "$RS_PID" ] && kill "$RS_PID" 2>/dev/null
    # `stax ingest webhook serve` is a PARENT of the real listener, so the
    # child needs killing too — the same lesson as finding 13, one level down.
    pkill -f "stax-server --webhooks-only --host 127.0.0.1 --port $RS_PORT" 2>/dev/null
    wait 2>/dev/null
    return 0
}
trap cleanup EXIT INT TERM

( exec "$PY_BIN" ingest webhook serve --host 127.0.0.1 --port "$PY_PORT" \
    >"$WORK/py.boot" 2>&1 ) &
PY_PID=$!
( exec "$RS_BIN" ingest webhook serve --host 127.0.0.1 --port "$RS_PORT" \
    >"$WORK/rs.boot" 2>&1 ) &
RS_PID=$!

wait_for() {
    local port="$1" tries=0
    while [ "$tries" -lt 100 ]; do
        if "$PY_INTERP" - "$port" <<'PYPROBE' 2>/dev/null; then return 0; fi
import socket, sys
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.3):
    pass
PYPROBE
        tries=$((tries + 1)); sleep 0.2
    done
    return 1
}

for spec in "$PY_PORT reference" "$RS_PORT port"; do
    set -- $spec
    if ! wait_for "$1"; then
        echo "webhook-receiver-differ: SETUP FAILURE — $2 never bound :$1" >&2
        sed -n '1,20p' "$WORK/${2:0:2}.boot" 2>/dev/null >&2
        cat "$WORK/py.boot" "$WORK/rs.boot" 2>/dev/null >&2
        exit 2
    fi
done

pass=0; fail=0; known=0
failed_ids=(); known_ids=()

# A case id prefixed `!` is KNOWN-OPEN: reported every run, never fatal, and
# each one carries its reason here rather than in a comment nobody greps.
#
#   !W-404-trailing  DIV-133, the maintainer's. FastAPI's `redirect_slashes`
#                    answers `POST /api/webhooks/github/` with a 307 to the
#                    slash-less path; axum has no such rule. It is the same
#                    router ruling the dashboard surface is parked on, and it
#                    reaches the receiver app too — which is worth knowing,
#                    because a 307 on a POST is what makes a webhook sender
#                    retry against a redirect.
#
#   !W-404-openapi   DIV-441 (NEW, this leg, for the desk). The reference's
#   !W-404-docs      receiver is a full `FastAPI()`, so it serves
#                    `/openapi.json`, `/docs` and `/redoc` — a complete schema
#                    document and an interactive UI — on the SAME port the
#                    verb's own help tells the operator to expose to a public
#                    tunnel. The port serves none of them. This is a REAL
#                    divergence and the port's behaviour is arguably the safer
#                    one, which is exactly why it is not quietly closed either
#                    way: it is a maintainer decision about the reference, not
#                    a transcription gap. The measured bytes are in the diff.

# The boot banners are a case too: they are the verb's only stdout, and one of
# them is the configured-receivers line this run's exported secrets select.
diff_banners() {
    # The reference prints its listening line and then uvicorn is silent at
    # log_level=warning; the port prints the same banner and then `stax-server`
    # announces its bound address on stdout. That extra line is the port's and
    # is stripped here, scoped to exactly it, and reported.
    # Two scoped, counted normalisations, both introduced by the HARNESS:
    #   1. the port number — this script chose two different ones on purpose,
    #      so the listening line cannot match and the number is masked;
    #   2. `stax-server`'s own bound-address line, which the reference has no
    #      counterpart for because its uvicorn runs in-process at
    #      log_level=warning. It is the port's extra line, it is CONSTANT, and
    #      it is stripped rather than pretended away.
    sed -e '/^stax-server listening on /d' \
        -e "s#:$RS_PORT/api/webhooks/#:<PORT>/api/webhooks/#" \
        "$WORK/rs.boot" > "$WORK/rs.banner"
    sed -e "s#:$PY_PORT/api/webhooks/#:<PORT>/api/webhooks/#" \
        "$WORK/py.boot" > "$WORK/py.banner"
    if cmp -s "$WORK/py.banner" "$WORK/rs.banner"; then
        pass=$((pass + 1))
        return 0
    fi
    {
        printf '=== W-banner\n--- stdout diff (python | rust)\n'
        diff -u "$WORK/py.banner" "$WORK/rs.banner"
    } > "$DIFFS/W-banner.diff"
    fail=$((fail + 1)); failed_ids+=("W-banner")
    printf '  FAIL  %-26s banner\n' "W-banner"
    return 1
}

run_case() {
    local id="$1"; shift
    local known_open=0
    case "$id" in
        !*) known_open=1; id="${id#!}" ;;
    esac
    if [ -n "$ONLY" ] && [ "${id#*"$ONLY"}" = "$id" ]; then return 0; fi
    "$PY_INTERP" "$RAW" 127.0.0.1 "$PY_PORT" "$@" > "$WORK/$id.py" 2>&1
    "$PY_INTERP" "$RAW" 127.0.0.1 "$RS_PORT" "$@" > "$WORK/$id.rs" 2>&1
    if cmp -s "$WORK/$id.py" "$WORK/$id.rs"; then
        if [ "$known_open" = 1 ]; then
            known=$((known + 1)); known_ids+=("$id")
            printf '  OPEN  %-26s (known-open, currently identical)\n' "$id"
        else
            pass=$((pass + 1))
        fi
        return 0
    fi
    {
        printf '=== %s\n--- request: %s\n--- response diff (python | rust)\n' "$id" "$*"
        diff -u "$WORK/$id.py" "$WORK/$id.rs"
    } > "$DIFFS/$id.diff"
    if [ "$known_open" = 1 ]; then
        known=$((known + 1)); known_ids+=("$id")
        printf '  OPEN  %-26s response\n' "$id"
        return 0
    fi
    fail=$((fail + 1)); failed_ids+=("$id")
    printf '  FAIL  %-26s response\n' "$id"
    return 1
}

echo "webhook-receiver-differ: reference :$PY_PORT · port :$RS_PORT"
echo "  comparing status + content-type + content-length + body bytes"
echo "  masked on EVERY row (constant, stated, never varying): date, server,"
echo "  header order — see parity/raw_request.py for why each one"
echo

diff_banners

# ── the three endpoints, every leg that writes nothing ───────────────────────
BODY='{"action":"closed","pull_request":{"number":1}}'

for ep in github gitlab ci; do
    # A signature that is the RIGHT LENGTH but the WRONG BYTES: this is the leg
    # that reaches `compare_digest` rather than bailing on the length check.
    SIG64="$(printf '0%.0s' $(seq 1 64))"
    run_case "W-$ep-403-wrong"  POST "/api/webhooks/$ep" \
        --header "content-type: application/json" \
        --header "x-hub-signature-256: sha256=$SIG64" \
        --header "x-gitlab-token: wrong-token" \
        --header "x-ci-signature: sha256=$SIG64" \
        --body "$BODY"
    # A signature that is too SHORT: bails BEFORE the constant-time compare.
    run_case "W-$ep-403-short"  POST "/api/webhooks/$ep" \
        --header "content-type: application/json" \
        --header "x-hub-signature-256: sha256=00" \
        --header "x-gitlab-token: x" \
        --header "x-ci-signature: sha256=00" \
        --body "$BODY"
    # No signature header at all.
    run_case "W-$ep-403-absent" POST "/api/webhooks/$ep" \
        --header "content-type: application/json" --body "$BODY"
    # Malformed body behind a bad signature — proves the gate fires FIRST.
    run_case "W-$ep-403-badbody" POST "/api/webhooks/$ep" \
        --header "content-type: application/json" \
        --header "x-hub-signature-256: sha256=$SIG64" \
        --header "x-gitlab-token: wrong" \
        --header "x-ci-signature: sha256=$SIG64" \
        --body '{not json'
    # Wrong methods on a registered path — FastAPI's 405, not axum's.
    run_case "W-$ep-405-get"    GET  "/api/webhooks/$ep"
    run_case "W-$ep-405-put"    PUT  "/api/webhooks/$ep"
    run_case "W-$ep-405-head"   HEAD "/api/webhooks/$ep"
done

# ── what the receiver app does NOT serve ─────────────────────────────────────
# The dashboard is not mounted here, and that is the verb's entire point: every
# one of these is a 404 on the receiver and a 200 on the dashboard.
run_case "W-404-root"       GET "/"
run_case "W-404-api"        GET "/api/projects"
run_case "W-404-static"     GET "/static/react/index.html"
run_case "W-404-unknown"    GET "/api/webhooks/nope"
run_case "!W-404-trailing"  POST "/api/webhooks/github/"
run_case "!W-404-openapi"   GET "/openapi.json"
run_case "!W-404-docs"      GET "/docs"

echo
total=$((pass + fail + known))
printf 'webhook-receiver-differ: %d cases · %d identical · %d divergent · %d known-open\n' \
    "$total" "$pass" "$fail" "$known"
[ "$known" -gt 0 ] && printf '  known-open: %s\n' "${known_ids[*]}"
if [ "$fail" -gt 0 ]; then
    printf '  failed: %s\n' "${failed_ids[*]}"
    printf '  diffs under %s\n' "$DIFFS"
    exit 1
fi
exit 0

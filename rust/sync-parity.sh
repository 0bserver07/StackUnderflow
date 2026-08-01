#!/usr/bin/env bash
# The sync differ — byte parity for the layer that moves data between machines.
#
# For every row in `parity/sync-cases.txt` this runs BOTH implementations
# (`parity/sync_parity.py` and the `stax-sync-parity` binary) with the same
# arguments, each against its OWN private copy of the same fixture state, and
# compares stdout byte for byte. Both halves print one line of
# `json.dumps(obj, separators=(",", ":"))` / `pyjson::dumps_compact`, so the
# comparison is `diff`, not a shape check.
#
# It never opens a socket. `ssh_store` is differed as **argv** — the exact list
# that would reach `execve`, including the remote shell command and its
# `shlex.quote`ing. The `LocalShellTransport` round trip (the same remote
# commands, run under `sh -c` against a scratch directory) is a crate unit test,
# where a directory can stand in for a host.
#
# Three sections:
#
#   1. corpus   — every row in `parity/sync-cases.txt`
#   2. crypto   — CROSS-implementation age interop: Python encrypts, Rust
#                 decrypts, and back. Ciphertext is randomised per blob, so this
#                 is the only way to compare it at all — and it is wave 7's
#                 stated runnable proof.
#   3. cli      — the four `sync` verbs through the real binaries (skipped with
#                 a reason until the Rust verbs land)
#
# Usage
#   rust/sync-parity.sh                  # everything
#   rust/sync-parity.sh --only V-pull    # id substring filter
#   rust/sync-parity.sh --build-state    # (re)build the synthetic stores
#   rust/sync-parity.sh --list           # print the corpus and exit
#   rust/sync-parity.sh --keep           # keep the per-case scratch dirs
#
# Exit: 0 when every case is identical, 1 on any divergence, 2 on setup failure.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../StackUnderflow" 2>/dev/null && pwd || true)}"
STATE_DIR="${STAX_PARITY_STATE_DIR:-$HERE/.parity-state}"
SYNC_DIR="$STATE_DIR/sync"
FIXTURES="$SYNC_DIR/fixtures"
RUNS="$SYNC_DIR/runs"
CASES="${STAX_SYNC_CASES:-$HERE/parity/sync-cases.txt}"
RS_BIN="${STAX_SYNC_RS_BIN:-$HERE/target/release/stax-sync-parity}"

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
        -h|--help) sed -n '2,35p' "$0"; exit 0 ;;
        *) echo "sync-parity: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [ "$DO_LIST" = 1 ]; then
    grep -v '^#' "$CASES" | grep -v '^[[:space:]]*$' | cut -f1,2
    exit 0
fi

# ── setup ────────────────────────────────────────────────────────────────────

# The reference interpreter must have the `[sync]` extra: `pyrage` is what the
# crypto section compares against, and `sync/keys.py` imports it for
# `generate_identity`. The corpus sections do not need it (that is the
# reference's own dependency-free design), so a venv without it still runs
# sections 1 and 3 — the crypto section says so and is skipped.
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/python}"
if [ ! -x "$PY_BIN" ]; then
    PY_BIN="$(command -v python3 || true)"
fi
if [ -z "$PY_BIN" ] || [ ! -x "$PY_BIN" ]; then
    echo "sync-parity: SETUP FAILURE — no Python interpreter" >&2
    exit 2
fi

if [ "$DO_BUILD" = 1 ] || [ ! -f "$FIXTURES/merged.db" ]; then
    echo "sync-parity: building the synthetic stores at $FIXTURES"
    PYTHONPATH="$REPO_ROOT" "$PY_BIN" "$HERE/parity/build_sync_state.py" "$FIXTURES" --force || exit 2
    [ "$DO_BUILD" = 1 ] && exit 0
fi

if [ ! -x "$RS_BIN" ]; then
    echo "sync-parity: building the release binary (the gate compares shipped bytes)"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-sync --quiet ) || exit 2
fi

# Determinism: pin everything either implementation could read differently.
# Nothing in `sync/` reads the locale or the clock on these paths (every stamp
# is injected by the corpus), but the CLI section shells the real binaries and
# those do.
export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
unset STACKUNDERFLOW_SYNC_KEY

# The reference must be THIS worktree's Python, not whatever tree the venv's
# editable install points at — the same pin every other gate carries.
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

rm -rf "$RUNS"
mkdir -p "$RUNS"

pass=0
fail=0
skipped=0
FAILED_IDS=()

# ── per-case scratch ─────────────────────────────────────────────────────────
#
# Every case gets its OWN copy of every store it names, per implementation.
# `push` and `pull` WRITE (`sync_outbox`, `sync_cursors`, the `<mart>_remote`
# tables), and a shared fixture would let case N see case N-1's writes — which
# is exactly the class of bug the hooks differ hit when it reused a home.
prepare() {
    local run_dir="$1" impl="$2"
    local dir="$run_dir/$impl"
    mkdir -p "$dir/state" "$dir/bucket"
    cp "$FIXTURES/empty.db"    "$dir/empty.db"
    cp "$FIXTURES/device-a.db" "$dir/device-a.db"
    cp "$FIXTURES/device-b.db" "$dir/device-b.db"
    cp "$FIXTURES/merged.db"   "$dir/merged.db"
}

expand() {
    local token="$1" dir="$2"
    case "$token" in
        @A)      printf '%s' "$dir/device-a.db" ;;
        @B)      printf '%s' "$dir/device-b.db" ;;
        @EMPTY)  printf '%s' "$dir/empty.db" ;;
        @MERGED) printf '%s' "$dir/merged.db" ;;
        @BUCKET) printf '%s' "$dir/bucket" ;;
        @STATE)  printf '%s' "$dir/state" ;;
        *)       printf '%s' "$token" ;;
    esac
}

# The two implementations run in directories whose NAMES differ, and several ops
# echo a path back (`keys-identity-path`, `keys-store-file`). Normalising the
# run directory to a fixed token is the only output rewriting this differ does,
# and it is scoped to that one string — the harness lesson from the wave-5
# rename, applied before it could bite.
normalise() {
    sed -e "s#$1#<RUN>#g"
}

run_case() {
    local id="$1"; shift
    local op="$1"; shift
    local run_dir="$RUNS/$id"
    mkdir -p "$run_dir"
    prepare "$run_dir" py
    prepare "$run_dir" rs

    local py_args=() rs_args=()
    local token
    for token in "$@"; do
        py_args+=("$(expand "$token" "$run_dir/py")")
        rs_args+=("$(expand "$token" "$run_dir/rs")")
    done

    "$PY_BIN" "$HERE/parity/sync_parity.py" "$op" "${py_args[@]}" \
        >"$run_dir/py.out" 2>"$run_dir/py.err"
    local py_rc=$?
    "$RS_BIN" "$op" "${rs_args[@]}" >"$run_dir/rs.out" 2>"$run_dir/rs.err"
    local rs_rc=$?

    normalise "$run_dir/py" <"$run_dir/py.out" >"$run_dir/py.norm"
    normalise "$run_dir/rs" <"$run_dir/rs.out" >"$run_dir/rs.norm"

    if [ "$py_rc" != "$rs_rc" ]; then
        printf '  DIVERGENT %-24s exit %s (python) vs %s (rust)\n' "$id" "$py_rc" "$rs_rc"
        sed -n '1,6p' "$run_dir/py.err" | sed 's/^/    py: /'
        sed -n '1,6p' "$run_dir/rs.err" | sed 's/^/    rs: /'
        fail=$((fail + 1)); FAILED_IDS+=("$id"); return
    fi
    if ! cmp -s "$run_dir/py.norm" "$run_dir/rs.norm"; then
        printf '  DIVERGENT %-24s stdout\n' "$id"
        diff <(fold -w160 "$run_dir/py.norm") <(fold -w160 "$run_dir/rs.norm") \
            | sed -n '1,20p' | sed 's/^/    /'
        fail=$((fail + 1)); FAILED_IDS+=("$id"); return
    fi
    pass=$((pass + 1))
    [ "$KEEP" = 1 ] || rm -rf "$run_dir"
}

# ── section 1: the corpus ────────────────────────────────────────────────────

echo "sync-parity: corpus"
while IFS= read -r line; do
    case "$line" in ''|'#'*) continue ;; esac
    # TAB IS IFS WHITESPACE. Setting `IFS=$'\t'` does NOT stop bash collapsing
    # runs of it, so `a<TAB><TAB>b` splits to two fields and an empty argument
    # silently vanishes — which turned `shlex.quote("")` into a
    # missing-argument error rather than a case, on the Rust side only, and
    # looked exactly like a real divergence. Translate to the unit separator
    # (0x1f), which is NOT IFS whitespace, and split on that. The sentinel then
    # preserves a TRAILING empty field, which even a non-whitespace IFS drops.
    IFS=$'\x1f' read -r -a fields <<<"${line//$'\t'/$'\x1f'}"$'\x1f''.'
    unset 'fields[${#fields[@]}-1]'
    id="${fields[0]}"
    op="${fields[1]}"
    [ -n "$ONLY" ] && case "$id" in *"$ONLY"*) ;; *) continue ;; esac
    run_case "$id" "$op" "${fields[@]:2}"
done <"$CASES"

# ── section 2: cross-implementation age interop ──────────────────────────────
#
# THE point of taking the `age` dependency (see the crate manifest): both
# implementations call the same audited Rust code, so a shard encrypted by one
# must decrypt in the other. Ciphertext cannot be byte-compared — age mints a
# fresh ephemeral key per blob — so the comparison is the ROUND TRIP.

crypto_section() {
    if ! "$PY_BIN" -c 'import pyrage' >/dev/null 2>&1; then
        echo "  SKIPPED — the reference interpreter has no 'pyrage' (pip install 'stackunderflow[sync]')"
        skipped=$((skipped + 1))
        return
    fi
    local dir="$RUNS/crypto"
    mkdir -p "$dir"

    # 1. Each side mints an identity; each side must agree on the OTHER's
    #    fingerprint, which is the value `sync init` stores and every later
    #    `run_push` compares against.
    "$PY_BIN" "$HERE/parity/sync_parity.py" cipher-genkey >"$dir/py.key" || return
    "$RS_BIN" cipher-genkey >"$dir/rs.key" || return
    local py_secret py_recipient rs_secret rs_recipient
    py_secret=$("$PY_BIN" -c 'import json,sys;print(json.load(open(sys.argv[1]))["secret"])' "$dir/py.key")
    py_recipient=$("$PY_BIN" -c 'import json,sys;print(json.load(open(sys.argv[1]))["recipient"])' "$dir/py.key")
    rs_secret=$("$PY_BIN" -c 'import json,sys;print(json.load(open(sys.argv[1]))["secret"])' "$dir/rs.key")
    rs_recipient=$("$PY_BIN" -c 'import json,sys;print(json.load(open(sys.argv[1]))["recipient"])' "$dir/rs.key")

    local id
    for id in crypto-fp-of-py-key crypto-fp-of-rs-key; do
        local recipient="$py_recipient"
        [ "$id" = crypto-fp-of-rs-key ] && recipient="$rs_recipient"
        "$PY_BIN" "$HERE/parity/sync_parity.py" keys-fingerprint "$recipient" >"$dir/$id.py"
        "$RS_BIN" keys-fingerprint "$recipient" >"$dir/$id.rs"
        if cmp -s "$dir/$id.py" "$dir/$id.rs"; then
            pass=$((pass + 1))
        else
            printf '  DIVERGENT %-24s fingerprint\n' "$id"
            fail=$((fail + 1)); FAILED_IDS+=("$id")
        fi
    done

    # 2. `recipient_for(secret)` must agree across implementations for BOTH
    #    keys — the X25519 scalar multiplication, not just the digest.
    for id in crypto-recip-py crypto-recip-rs; do
        local secret="$py_secret"
        [ "$id" = crypto-recip-rs ] && secret="$rs_secret"
        "$PY_BIN" "$HERE/parity/sync_parity.py" cipher-recipient "$secret" >"$dir/$id.py"
        "$RS_BIN" cipher-recipient "$secret" >"$dir/$id.rs"
        if cmp -s "$dir/$id.py" "$dir/$id.rs"; then
            pass=$((pass + 1))
        else
            printf '  DIVERGENT %-24s recipient_for\n' "$id"
            diff "$dir/$id.py" "$dir/$id.rs" | sed 's/^/    /'
            fail=$((fail + 1)); FAILED_IDS+=("$id")
        fi
    done

    # 3. The round trips. The payload is a REAL shard's canonical bytes, so the
    #    interop proof is about the thing that actually crosses the wire.
    local shard_b64
    shard_b64=$("$PY_BIN" -c '
import base64, json, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from stackunderflow.sync import serialize
conn = sqlite3.connect(sys.argv[1]); conn.row_factory = sqlite3.Row
print(base64.b64encode(serialize.build_shards(conn)[0].to_bytes()).decode())
' "$FIXTURES/device-a.db" "$REPO_ROOT")

    crypto_roundtrip() {
        local id="$1" enc="$2" dec="$3" recipient="$4" secret="$5"
        local ct
        if [ "$enc" = py ]; then
            ct=$("$PY_BIN" "$HERE/parity/sync_parity.py" cipher-encrypt "$recipient" "$shard_b64" \
                 | "$PY_BIN" -c 'import json,sys;print(json.load(sys.stdin)["ciphertext"])')
        else
            ct=$("$RS_BIN" cipher-encrypt "$recipient" "$shard_b64" \
                 | "$PY_BIN" -c 'import json,sys;print(json.load(sys.stdin)["ciphertext"])')
        fi
        local out
        if [ "$dec" = py ]; then
            out=$("$PY_BIN" "$HERE/parity/sync_parity.py" cipher-decrypt "$secret" "$ct")
        else
            out=$("$RS_BIN" cipher-decrypt "$secret" "$ct")
        fi
        local want="{\"ok\":true,\"plaintext\":\"$shard_b64\"}"
        if [ "$out" = "$want" ]; then
            pass=$((pass + 1))
        else
            printf '  DIVERGENT %-24s round trip %s→%s\n' "$id" "$enc" "$dec"
            printf '    got  %s\n' "${out:0:200}"
            fail=$((fail + 1)); FAILED_IDS+=("$id")
        fi
    }

    crypto_roundtrip crypto-py-to-rs py rs "$py_recipient" "$py_secret"
    crypto_roundtrip crypto-rs-to-py rs py "$py_recipient" "$py_secret"
    crypto_roundtrip crypto-rs-key-py-to-rs py rs "$rs_recipient" "$rs_secret"
    crypto_roundtrip crypto-rs-key-rs-to-py rs py "$rs_recipient" "$rs_secret"

    # 4. The wrong key must fail, with the SAME message, on both sides — the
    #    string a user sees through `sync pull failed: {exc}`.
    local wrong_ct
    wrong_ct=$("$RS_BIN" cipher-encrypt "$rs_recipient" "$shard_b64" \
               | "$PY_BIN" -c 'import json,sys;print(json.load(sys.stdin)["ciphertext"])')
    "$PY_BIN" "$HERE/parity/sync_parity.py" cipher-decrypt "$py_secret" "$wrong_ct" >"$dir/wrong.py"
    "$RS_BIN" cipher-decrypt "$py_secret" "$wrong_ct" >"$dir/wrong.rs"
    if cmp -s "$dir/wrong.py" "$dir/wrong.rs"; then
        pass=$((pass + 1))
    else
        printf '  DIVERGENT %-24s wrong-key message\n' crypto-wrong-key
        diff "$dir/wrong.py" "$dir/wrong.rs" | sed 's/^/    /'
        fail=$((fail + 1)); FAILED_IDS+=(crypto-wrong-key)
    fi
}

echo "sync-parity: crypto interop"
crypto_section

# ── section 3: the CLI verbs ─────────────────────────────────────────────────

echo "sync-parity: cli verbs"
PY_CLI="${STAX_PARITY_PY_CLI:-$PY_ROOT/.venv/bin/stackunderflow}"
RS_CLI="${STAX_SYNC_CLI_BIN:-$HERE/target/release/stax}"
cli_case() {
    local id="$1"; shift
    local store="$1"; shift
    local dir="$RUNS/cli-$id"
    mkdir -p "$dir/py" "$dir/rs"
    cp "$FIXTURES/$store" "$dir/py/store.db"
    cp "$FIXTURES/$store" "$dir/rs/store.db"

    STACKUNDERFLOW_HOME="$dir/py" "$PY_CLI" "$@" >"$dir/py.out" 2>"$dir/py.err"
    local py_rc=$?
    STACKUNDERFLOW_HOME="$dir/rs" "$RS_CLI" "$@" >"$dir/rs.out" 2>"$dir/rs.err"
    local rs_rc=$?

    if [ "$py_rc" = "$rs_rc" ] && cmp -s "$dir/py.out" "$dir/rs.out"; then
        pass=$((pass + 1))
        [ "$KEEP" = 1 ] || rm -rf "$dir"
    else
        printf '  DIVERGENT %-24s exit %s/%s\n' "cli-$id" "$py_rc" "$rs_rc"
        diff "$dir/py.out" "$dir/rs.out" | sed -n '1,20p' | sed 's/^/    /'
        fail=$((fail + 1)); FAILED_IDS+=("cli-$id")
    fi
}

if [ ! -x "$PY_CLI" ]; then
    echo "  SKIPPED — no Python CLI at $PY_CLI"
    skipped=$((skipped + 1))
elif [ ! -x "$RS_CLI" ] || ! "$RS_CLI" sync --help >/dev/null 2>&1; then
    echo "  SKIPPED — the Rust binary has no 'sync' verb group yet (RS-7-010/016..019 open)"
    skipped=$((skipped + 1))
else
    cli_case status-off      empty.db     sync status
    cli_case status-off-json empty.db     sync status --json
    cli_case status-on       device-a.db  sync status
    cli_case status-on-json  device-a.db  sync status --json
    cli_case status-merged   merged.db    sync status --json
    cli_case push-unconfig   empty.db     sync push
    cli_case pull-unconfig   empty.db     sync pull
    cli_case pull-unconfig-j empty.db     sync pull --json
    cli_case init-bad-scheme empty.db     sync init --bucket gs://nope
    cli_case init-bad-ssh    empty.db     sync init --bucket ssh://host
fi

# ── report ───────────────────────────────────────────────────────────────────

echo
printf 'sync-parity: %d identical / %d divergent / %d skipped section(s)\n' \
    "$pass" "$fail" "$skipped"
if [ "$fail" -gt 0 ]; then
    printf 'divergent: %s\n' "${FAILED_IDS[*]}"
    echo "evidence kept under $RUNS"
    exit 1
fi
[ "$KEEP" = 1 ] && echo "scratch kept under $RUNS"
exit 0

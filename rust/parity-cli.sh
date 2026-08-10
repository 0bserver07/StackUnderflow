#!/usr/bin/env bash
# The permanent byte-diff harness — CLI drop-in parity is the P0 gate.
#
# Maintainer directive (rust/DIRECTIVE-PARITY-P0.md): "i need to be able to work
# a drop-in for the python — no unexplained divergence." So this runs every
# verb against BOTH store states the maintainer's machine can be in and diffs
# stdout, stderr and the exit code byte for byte against the Python CLI.
#
#   fresh   $STACKUNDERFLOW_HOME with a store and no populated FTS sidecar
#           → discovery takes its `content_text LIKE '%needle%'` scan
#   fts     the same store plus the live 599 MB search_index.db
#           → discovery takes its bm25 branch in four of the five memory verbs
#
# The second state is the one that matters: it IS the maintainer's machine
# today (250,998 indexed messages), and the wave-1 verifier measured 4 of 5
# verbs diverging on it. A gate that only tests `fresh` tests a machine nobody
# owns.
#
# A case may opt out of the shared state with a `@home[:SEED]` suffix on its
# cwd token (wave 8): each implementation then gets its own fresh copy of
# rust/parity/homes/SEED, and the two resulting trees are diffed as part of the
# case. That is how a WRITER (`cfg set`, `clear-cache`) can be gated without one
# run poisoning the next.
#
# Tranche 2 adds one token to that mechanism: a cwd of `home` means "run inside
# the case-local home", and it additionally exports `HOME` there. The installers
# need it — `hooks install --scope project` writes into the *git root of the
# cwd* and `--scope user` into `$HOME/.claude`, so with `repo` as the cwd a row
# would write into this worktree on every run, and with the ambient `$HOME` it
# would write into the maintainer's real `~/.claude`. Neither is acceptable, and
# neither is skipping the proof. The scoping is deliberate: `HOME` is exported
# only for rows that asked for it by naming `home` as their cwd, so the 404-row
# baseline sees no change at all.
#
# Both runs also get stdin from /dev/null. `backup restore` prompts through
# `click.confirm` and `hooks run` reads its payload from stdin — inheriting the
# harness's stdin would make those rows depend on whether ci.sh was started from
# a terminal.
#
# Usage
#   rust/parity-cli.sh                    # both states, the whole matrix
#   rust/parity-cli.sh --state fts        # one state
#   rust/parity-cli.sh --only F-file      # id substring filter
#   rust/parity-cli.sh --build-state      # (re)build the states from live data
#   rust/parity-cli.sh --list             # print the matrix and exit
#
# Exit: 0 when every case is byte-identical, 1 on any divergence, 2 on a setup
# failure (missing venv / missing state). ci.sh's gate 4 calls it with no
# arguments.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
PY_ROOT="${STAX_PARITY_PY_ROOT:-$(cd "$REPO_ROOT/../staxtrace" 2>/dev/null && pwd || true)}"
PY_BIN="${STAX_PARITY_PY_BIN:-$PY_ROOT/.venv/bin/stackunderflow}"
LIVE_DIR="${STAX_PARITY_LIVE_DIR:-$REPO_ROOT/../stackunderflow-data}"
STATE_DIR="${STAX_PARITY_STATE_DIR:-$HERE/.parity-state}"
CASES="${STAX_PARITY_CASES:-$HERE/parity/cases.txt}"
DIFFS="$STATE_DIR/diffs"
RS_BIN="${STAX_PARITY_RS_BIN:-$HERE/target/release/stax}"

# The Rust binary is `stax` (desk ruling 4, 2026-07-31) while the Python CLI
# still prints `stackunderflow` into its usage lines, so an otherwise
# byte-identical parse error differs in exactly that token. Normalising it is
# the ONE substitution this harness performs, applied to the Rust side only,
# scoped to `Usage:`/`Try '…'` lines (a blanket \bstax\b would rewrite store
# content that documents the `stax` alias), and every case that needed it is
# counted and reported — a silent normalisation would be the harness lying to
# the gate.
PROGRAM_NAME_PY="stackunderflow"
PROGRAM_NAME_RS="stax"

WANT_STATES="fresh fts"
ONLY=""
DO_BUILD=0
DO_LIST=0
FORCE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --state) WANT_STATES="$2"; shift 2 ;;
        --state=*) WANT_STATES="${1#*=}"; shift ;;
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --build-state) DO_BUILD=1; shift ;;
        --force) FORCE=1; shift ;;
        --list) DO_LIST=1; shift ;;
        -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
        *) echo "parity-cli: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [ "$WANT_STATES" = "both" ]; then WANT_STATES="fresh fts"; fi

# ── setup ────────────────────────────────────────────────────────────────────

if [ ! -x "$PY_BIN" ]; then
    echo "parity-cli: SETUP FAILURE — no Python CLI at $PY_BIN" >&2
    echo "            (set STAX_PARITY_PY_BIN, or run ci.sh --skip-parity)" >&2
    exit 2
fi

if [ "$DO_BUILD" = 1 ]; then
    py_interp="$PY_ROOT/.venv/bin/python"
    [ -x "$py_interp" ] || py_interp="python3"
    echo "parity-cli: building states from $LIVE_DIR"
    build_args=("$HERE/parity/build_state.py" "$LIVE_DIR" "$STATE_DIR")
    [ "$FORCE" = 1 ] && build_args+=(--force)
    "$py_interp" "${build_args[@]}" || exit 2
    exit 0
fi

if [ ! -x "$RS_BIN" ]; then
    echo "parity-cli: building the release binary (the gate compares shipped bytes)"
    if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
    ( cd "$HERE" && cargo build --release -p stax-cli --quiet ) || exit 2
fi

if [ ! -f "$STATE_DIR/.built" ]; then
    echo "parity-cli: SETUP FAILURE — no states at $STATE_DIR" >&2
    echo "            run: rust/parity-cli.sh --build-state" >&2
    exit 2
fi

# A scratch cwd no project covers — the `_detect_cwd_project_slug` floor.
TMP_CWD="$STATE_DIR/scratch-cwd"
mkdir -p "$TMP_CWD" "$DIFFS"
rm -f "$DIFFS"/*.diff 2>/dev/null

# Determinism: pin everything either implementation could read differently.
# Ollama is pointed at a closed port on purpose — `memory ask`'s vector half
# must be *absent* identically on both sides, not "absent because this box
# happens not to run a daemon".
export LC_ALL=C LANG=C TZ=UTC PYTHONHASHSEED=0 PYTHONIOENCODING=utf-8
export COLUMNS=80 LINES=24 NO_COLOR=1 TERM=dumb CLICOLOR=0
export OLLAMA_URL="http://127.0.0.1:1"
export STACKUNDERFLOW_OLLAMA_URL="http://127.0.0.1:1"
unset STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS
unset STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS
unset STACKUNDERFLOW_EMBED_MODEL
unset OLLAMA_API_KEY STACKUNDERFLOW_OLLAMA_API_KEY
unset STAX_ANCHOR_DB

# The reference must be THIS worktree's Python, not whatever tree the venv's
# editable install points at — they are the same source today only by habit
# (the `.pth` names the maintainer's other worktree, on another branch). Desk
# ruling 1 / DIV-021 made that load-bearing: `resume`'s ORDER BY tiebreaker
# landed in Python HERE, and without this pin the gate would compare this
# branch's Rust against another branch's Python and call the divergence real.
# Same split-brain class as gate 0's "run it AT the commit".
export PYTHONPATH="${STAX_PARITY_PY_PATH:-$REPO_ROOT}${PYTHONPATH:+:$PYTHONPATH}"

CASE_TIMEOUT="${STAX_PARITY_TIMEOUT:-180}"

pass=0; fail=0; skipped=0; normalized=0; accepted_count=0; hook_command_normalized=0
failed_ids=(); accepted_ids=()

resolve_cwd() {
    case "$1" in
        repo) printf '%s' "$REPO_ROOT" ;;
        py)   printf '%s' "${PY_ROOT:-$REPO_ROOT}" ;;
        tmp)  printf '%s' "$TMP_CWD" ;;
        *)    printf '%s' "$1" ;;
    esac
}

# ── case-local homes (wave 8) ────────────────────────────────────────────────
#
# Every row above runs against a SHARED `$STATE_DIR/<state>` home, which is
# correct for read-only verbs and wrong for writers: `cfg set` would leave a
# config.json the next case reads, and `clear-cache` prints a *different number
# of lines* on its second run than its first — so with a shared home the Python
# run (which goes first) would delete the file and the Rust run would then take
# the other branch and "diverge" on a state the harness itself created.
#
# So a cwd token may carry a home spec after an `@`:
#
#   repo@home            a fresh, EMPTY home for this case
#   repo@home:cfg-basic  a fresh home seeded from rust/parity/homes/cfg-basic/
#
# Each implementation gets its OWN copy of the seed, so neither can observe the
# other's writes, and after the run the two homes are diffed recursively — which
# is the actual proof for a config writer: not just that the two printed the
# same line, but that they left the same bytes on disk. A difference in the
# resulting tree fails the case exactly like a stdout difference would.
HOMES="$HERE/parity/homes"

# The installers (`hooks install|uninstall|repair`, `guide install|uninstall`)
# write `<name>.bak.<utc-ts>` before mutating, and the two implementations run
# seconds apart — so the backup's *name* carries a clock the harness itself
# introduced, while its *contents* are the thing under test. This renames the
# stamp to a fixed token in BOTH trees before the diff, which keeps the content
# comparison exact, keeps a missing or extra backup a failure, and is counted
# and reported every run rather than applied silently. Scoped to the one
# filename shape that justified it — nothing else is touched.
backup_stamps_normalized=0
# The same clock reaches STDOUT: `hooks install|uninstall` and `guide install`
# print the path of the backup they wrote ("  backup written: …"), so a row
# whose two runs straddle a second boundary diverges on a clock the HARNESS
# introduced rather than on behaviour. Found exactly that way —
# `T-hooks-uninst-inst` failed at 545 B vs 545 B, `…T221517Z` vs `…T221518Z`,
# which is also why it is a latent flake on every installer row that prints a
# path, not a property of that one case.
#
# Applied to BOTH sides, unlike the program-name substitution: neither clock
# reading is the authoritative one. The stamp FORMAT stays fully under test,
# because this rewrites only text that ALREADY matches the reference's exact
# shape — a port emitting `.bak.2026-08-01T22:15:18Z` does not match, is not
# rewritten, and still fails the diff. Scoped to that one filename shape, and
# counted and reported every run.
stdout_stamps_normalized=0
normalise_backup_stamps() {
    local dir="$1" path base
    while IFS= read -r path; do
        [ -z "$path" ] && continue
        base="$(basename "$path")"
        # `<name>.bak.YYYYmmddTHHMMSSZ` and the `.N` collision suffixes.
        local fixed
        fixed="$(printf '%s' "$base" | sed -E 's/\.bak\.[0-9]{8}T[0-9]{6}Z/.bak.STAMP/')"
        [ "$fixed" = "$base" ] && continue
        mv "$path" "$(dirname "$path")/$fixed" 2>/dev/null && \
            backup_stamps_normalized=$((backup_stamps_normalized + 1))
    done < <(find "$dir" -name '*.bak.*Z' -o -name '*.bak.*Z.[0-9]*' 2>/dev/null)
}

# Rewrite one file's post-split native-program spellings back to the
# reference's, in place. Returns 1 when it changed the file, 0 when it was
# already reference-form — callers count the change. Five exact mappings, one
# per departed surface: the canonical hook command, the stale hint, and the
# guide snippet's three renamed lines. The envelope schema string
# (`stackunderflow.memory/1`) never needed a mapping — it did not change.
normalise_cutover_names() {
    local file="$1"
    sed -e 's/\bstax-hooks run \(stackunderflow-\)/stackunderflow hooks run \1/g' \
        -e 's/run `stax hooks repair`/run `stackunderflow hooks repair`/g' \
        -e 's/`stax memory /`stackunderflow memory /g' \
        -e 's/^## staxtrace — query your past coding sessions$/## StackUnderflow — query your past coding sessions/' \
        -e 's/locally with staxtrace\./locally with StackUnderflow./' \
        "$file" >"$file.cutover"
    if cmp -s "$file" "$file.cutover"; then
        rm -f "$file.cutover"
        return 0
    fi
    mv "$file.cutover" "$file"
    return 1
}

seed_home() {
    local dest="$1" seed="$2"
    mkdir -p "$dest" || return 1
    [ -z "$seed" ] && return 0
    if [ ! -d "$HOMES/$seed" ]; then
        echo "parity-cli: SETUP FAILURE — no seed home $HOMES/$seed" >&2
        return 1
    fi
    cp -a "$HOMES/$seed/." "$dest/" || return 1
    return 0
}

# ── the Rust-only rows ───────────────────────────────────────────────────────
#
# `anchor` and `store` have no Python counterpart (DIV-025 ruled: the verb is `store`; `stackunderflow status` is a
# different command that happens to share the name — see the ledger). They are
# still gated, as self-checking round-trips: a regression in them is caught
# here rather than in a wave that assumes them.
run_rust_only() {
    local id="$1" cwd="$2" home="$3"
    local out rc
    case "$id" in
        rust:anchor-roundtrip)
            local db="$STATE_DIR/anchor-roundtrip.db"
            rm -f "$db"
            out="$(cd "$cwd" && STACKUNDERFLOW_HOME="$home" "$RS_BIN" anchor --db "$db" set parity-probe hello world 2>&1)"; rc=$?
            [ $rc -eq 0 ] || { printf '%s\n' "$out" > "$DIFFS/$id.diff"; return 1; }
            out="$(cd "$cwd" && STACKUNDERFLOW_HOME="$home" "$RS_BIN" anchor --db "$db" get parity-probe 2>&1)"; rc=$?
            [ $rc -eq 0 ] || { printf '%s\n' "$out" > "$DIFFS/$id.diff"; return 1; }
            case "$out" in
                *"hello world"*) ;;
                *) printf 'anchor get did not return the value set:\n%s\n' "$out" > "$DIFFS/$id.diff"; return 1 ;;
            esac
            out="$(cd "$cwd" && STACKUNDERFLOW_HOME="$home" "$RS_BIN" anchor --db "$db" get parity-probe --json 2>&1)"; rc=$?
            [ $rc -eq 0 ] || { printf '%s\n' "$out" > "$DIFFS/$id.diff"; return 1; }
            case "$out" in
                *'"schema"'*) ;;
                *) printf 'anchor get --json is not an envelope:\n%s\n' "$out" > "$DIFFS/$id.diff"; return 1 ;;
            esac
            rm -f "$db"
            return 0
            ;;
        rust:store)
            out="$(cd "$cwd" && STACKUNDERFLOW_HOME="$home" "$RS_BIN" store 2>&1)"; rc=$?
            [ $rc -eq 0 ] || { printf '%s\n' "$out" > "$DIFFS/$id.diff"; return 1; }
            # The store the states are built from always has these two.
            case "$out" in
                *sessions*) ;;
                *) printf 'store did not list the sessions table:\n%s\n' "$out" > "$DIFFS/$id.diff"; return 1 ;;
            esac
            return 0
            ;;
    esac
    printf 'unknown rust-only case id\n' > "$DIFFS/$id.diff"
    return 1
}

# ── one case × one state ─────────────────────────────────────────────────────

run_case() {
    local id="$1" cwd_token="$2" argv="$3" state="$4"
    local home="$STATE_DIR/$state"
    local tag="$state/$id"

    # `TOKEN@home[:SEED]` — a case-local home instead of the shared state.
    local home_spec="" seed=""
    case "$cwd_token" in
        *@*) home_spec="${cwd_token#*@}"; cwd_token="${cwd_token%%@*}" ;;
    esac
    case "$home_spec" in
        home)    seed="" ;;
        home:*)  seed="${home_spec#home:}" ;;
        "")      ;;
        *) echo "parity-cli: case $id has an unknown home spec '$home_spec'" >&2; return 1 ;;
    esac

    # `home` resolves only after the case-local home exists, so its resolution
    # is deferred; every other token resolves (and is existence-checked) now.
    local cwd=""
    if [ "$cwd_token" = "home" ]; then
        if [ -z "$home_spec" ]; then
            echo "parity-cli: case $id uses cwd 'home' without a @home spec" >&2
            fail=$((fail + 1)); failed_ids+=("$tag"); return 1
        fi
    else
        cwd="$(resolve_cwd "$cwd_token")"
        if [ ! -d "$cwd" ]; then
            skipped=$((skipped + 1)); printf '  SKIP  %-28s (no cwd %s)\n' "$tag" "$cwd"; return 0
        fi
    fi

    case "$id" in
        rust:*)
            if run_rust_only "$id" "$cwd" "$home"; then
                pass=$((pass + 1)); return 0
            fi
            fail=$((fail + 1)); failed_ids+=("$tag"); printf '  FAIL  %-28s (rust-only round-trip)\n' "$tag"; return 1
            ;;
    esac

    local work; work="$(mktemp -d)"

    # shellcheck disable=SC2086 — the case file is trusted repo content and its
    # quoting is the point: `eval set --` is how a case says `'cache lookup'`.
    eval "set -- $argv"

    # Both runs see the SAME `$STACKUNDERFLOW_HOME` string. That is not a
    # convenience: `backup list` prints its own directory, so two different home
    # paths would show up as a false divergence in the very rows this mechanism
    # exists for. The isolation comes from re-seeding between the runs and
    # moving each result aside, not from giving them different paths.
    local case_home="$work/home"
    if [ -n "$home_spec" ] && ! seed_home "$case_home" "$seed"; then
        fail=$((fail + 1)); failed_ids+=("$tag")
        printf '  FAIL  %-28s (could not seed the case home)\n' "$tag"
        rm -rf "$work"; return 1
    fi
    local run_home="$home"
    [ -n "$home_spec" ] && run_home="$case_home"
    # `home` as the cwd: run inside the case-local home, and make `$HOME` point
    # at it too, so `Path.home()`-rooted writers stay inside the diffed tree.
    # Passed through `env` rather than as a shell assignment prefix: a prefix has
    # to be a literal word, and an expanded `HOME=…` is parsed as a COMMAND.
    local run_env=()
    if [ "$cwd_token" = "home" ]; then
        cwd="$case_home"
        run_env=(HOME="$case_home")
    fi

    ( cd "$cwd" && STACKUNDERFLOW_HOME="$run_home" env "${run_env[@]}" timeout "$CASE_TIMEOUT" \
        "$PY_BIN" "$@" >"$work/py.out" 2>"$work/py.err" </dev/null )
    local py_rc=$?
    local py_home="$run_home" rs_home="$run_home"
    if [ -n "$home_spec" ]; then
        py_home="$work/after-py"; rs_home="$work/after-rs"
        mv "$case_home" "$py_home"
        if ! seed_home "$case_home" "$seed"; then
            fail=$((fail + 1)); failed_ids+=("$tag")
            printf '  FAIL  %-28s (could not re-seed the case home)\n' "$tag"
            rm -rf "$work"; return 1
        fi
    fi
    ( cd "$cwd" && STACKUNDERFLOW_HOME="$run_home" env "${run_env[@]}" timeout "$CASE_TIMEOUT" \
        "$RS_BIN" "$@" >"$work/rs.raw" 2>"$work/rs.raw.err" </dev/null )
    local rs_rc=$?
    [ -n "$home_spec" ] && mv "$case_home" "$rs_home"

    # Program-name normalisation, Rust side only, counted when it fires.
    # Scoped to the two Click-shaped surfaces that carry the program name
    # (`Usage:` lines and `Try '…'` hint lines) — a blanket \bstax\b
    # substitution rewrites real store CONTENT now that the binary shares
    # its name with the documented `stax` alias (found via a false diff on
    # A-dec-since-budget0, whose snippet quotes the alias README).
    sed -e "/^Usage:/s/\\b$PROGRAM_NAME_RS\\b/$PROGRAM_NAME_PY/g" \
        -e "/^Try '/s/\\b$PROGRAM_NAME_RS\\b/$PROGRAM_NAME_PY/g" \
        "$work/rs.raw" >"$work/rs.out"
    sed -e "/^Usage:/s/\\b$PROGRAM_NAME_RS\\b/$PROGRAM_NAME_PY/g" \
        -e "/^Try '/s/\\b$PROGRAM_NAME_RS\\b/$PROGRAM_NAME_PY/g" \
        "$work/rs.raw.err" >"$work/rs.err"
    if ! cmp -s "$work/rs.raw" "$work/rs.out" || ! cmp -s "$work/rs.raw.err" "$work/rs.err"; then
        normalized=$((normalized + 1))
    fi

    # Cutover-name normalisation, Rust side only, counted when it fires.
    # Since the split the port names the native programs where the reference
    # named the Python entry point — main carries no Python for a settings
    # file or an instruction snippet to invoke. Four exact spellings, each
    # scoped to its surface (templates.rs::canonical_command, the stale hint,
    # guide.rs::GUIDE_BODY); applied to case stdout here and to the written
    # trees the installer cases diff below, through one shared function.
    local hookcmd=0 stream
    for stream in "$work/rs.out" "$work/rs.err"; do
        normalise_cutover_names "$stream" || hookcmd=1
    done
    [ "$hookcmd" = 1 ] && hook_command_normalized=$((hook_command_normalized + 1))

    # Installer backup-stamp normalisation on the printed paths — both sides,
    # counted (see the note by `normalise_backup_stamps`).
    local stamped=0 stream
    for stream in "$work/py.out" "$work/py.err" "$work/rs.out" "$work/rs.err"; do
        [ -f "$stream" ] || continue
        sed -E 's/\.bak\.[0-9]{8}T[0-9]{6}Z/.bak.STAMP/g' "$stream" >"$stream.stamped"
        cmp -s "$stream" "$stream.stamped" || stamped=1
        mv "$stream.stamped" "$stream"
    done
    [ "$stamped" = 1 ] && stdout_stamps_normalized=$((stdout_stamps_normalized + 1))

    local ok=1
    cmp -s "$work/py.out" "$work/rs.out" || ok=0
    cmp -s "$work/py.err" "$work/rs.err" || ok=0
    [ "$py_rc" = "$rs_rc" ] || ok=0

    # The writer proof: the two case-local homes must be byte-identical
    # afterwards. `cfg set` printing the same line while writing a different
    # `config.json` is precisely the divergence a stdout-only diff misses.
    local home_diff=""
    if [ -n "$home_spec" ]; then
        normalise_backup_stamps "$py_home"
        normalise_backup_stamps "$rs_home"
        # The cutover-name normalisation again, on the Rust-side tree: an
        # installer case's settings.json carries the post-split canonical
        # (`stax-hooks run <id>`) and a guide case's CLAUDE.md / AGENTS.md the
        # renamed snippet, where the reference writes the Python spellings.
        # Same mappings as the stdout pass, counted through the same flag.
        local written
        while IFS= read -r written; do
            normalise_cutover_names "$written" || \
                hook_command_normalized=$((hook_command_normalized + 1))
        done < <(find "$rs_home" -type f \( -name 'settings.json*' -o -name 'CLAUDE.md*' -o -name 'AGENTS.md*' \) 2>/dev/null)
        home_diff="$(diff -r "$py_home" "$rs_home" 2>&1)" || ok=0
    fi

    # Maintainer-ACCEPTED divergences (desk ruling 2, 2026-07-31, DIV-010
    # residue): the >u64 `--limit` clamp cases stay named here, every run,
    # by order — they are not silent, and they are not failures.
    local accepted=0
    case "$id" in
        V-dec-limit-huge|V-dec-limit-bad) accepted=1 ;;
    esac

    if [ "$ok" = 1 ]; then
        if [ "$accepted" = 1 ]; then
            printf '  NOTE  %-28s maintainer-accepted case now PASSES — re-examine the ruling\n' "$tag"
        fi
        pass=$((pass + 1))
        rm -rf "$work"
        return 0
    fi

    if [ "$accepted" = 1 ]; then
        accepted_count=$((accepted_count + 1)); accepted_ids+=("$tag")
        printf '  ACPT  %-28s diverges as ruled (DIV-010 >u64 clamp)\n' "$tag"
        rm -rf "$work"
        return 0
    fi

    fail=$((fail + 1)); failed_ids+=("$tag")
    {
        printf '=== %s ===\n' "$tag"
        printf 'argv:  %s\n' "$argv"
        printf 'cwd:   %s\n' "$cwd"
        printf 'home:  python=%s rust=%s\n' "$py_home" "$rs_home"
        printf 'exit:  python=%s rust=%s\n\n' "$py_rc" "$rs_rc"
        if [ -n "$home_diff" ]; then
            printf -- '--- the case-local homes differ ---\n%s\n\n' "$home_diff"
        fi
        printf -- '--- stdout (python) vs (rust) ---\n'
        diff -u "$work/py.out" "$work/rs.out" | head -80
        printf -- '\n--- stderr (python) vs (rust) ---\n'
        diff -u "$work/py.err" "$work/rs.err" | head -40
        printf -- '\n--- sizes ---\n'
        printf 'python stdout %s B, rust stdout %s B\n' \
            "$(wc -c <"$work/py.out")" "$(wc -c <"$work/rs.out")"
    } >"$DIFFS/${state}__${id//[^A-Za-z0-9_.-]/_}.diff"
    printf '  FAIL  %-28s py=%sB/rc%s rs=%sB/rc%s\n' "$tag" \
        "$(wc -c <"$work/py.out")" "$py_rc" "$(wc -c <"$work/rs.out")" "$rs_rc"
    rm -rf "$work"
    return 1
}

# ── the run ──────────────────────────────────────────────────────────────────

if [ "$DO_LIST" = 1 ]; then
    grep -vE '^\s*(#|$)' "$CASES" | sed 's/|/ | /g'
    exit 0
fi

echo "parity-cli: python=$PY_BIN"
echo "            rust=$RS_BIN"
echo "            states=$STATE_DIR [$WANT_STATES]"
[ -n "$ONLY" ] && echo "            filter=$ONLY"

for state in $WANT_STATES; do
    home="$STATE_DIR/$state"
    if [ ! -f "$home/store.db" ]; then
        echo "parity-cli: SETUP FAILURE — no store in $home" >&2
        exit 2
    fi
    # `SearchService.__init__` CREATES search_index.db, so the Python CLI turns
    # the `fresh` state into an "empty index" state on its first run. Both
    # implementations read an empty index as unpopulated, so the outputs agree
    # either way — but resetting keeps the state named honestly and the run
    # order irrelevant.
    if [ "$state" = "fresh" ]; then
        rm -f "$home"/search_index.db "$home"/search_index.db-wal "$home"/search_index.db-shm
    fi
    printf '\n=== state: %s ===\n' "$state"
    while IFS='|' read -r id cwd_token argv; do
        id="$(printf '%s' "$id" | tr -d '[:space:]')"
        cwd_token="$(printf '%s' "$cwd_token" | tr -d '[:space:]')"
        argv="$(printf '%s' "$argv" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
        [ -z "$id" ] && continue
        case "$id" in \#*) continue ;; esac
        [ -z "$argv" ] && continue
        if [ -n "$ONLY" ]; then
            case "$id" in *"$ONLY"*) ;; *) continue ;; esac
        fi
        run_case "$id" "$cwd_token" "$argv" "$state"
    done < <(grep -vE '^\s*#' "$CASES")
done

total=$((pass + fail + accepted_count))
printf '\n=== parity tally ===\n'
printf 'cases: %s   pass: %s   FAIL: %s   accepted: %s   skipped: %s\n' \
    "$total" "$pass" "$fail" "$accepted_count" "$skipped"
printf 'program-name normalisation fired on %s case(s)\n' "$normalized"
printf 'hook-command normalisation (post-split canonical) fired on %s case/file(s)\n' \
    "$hook_command_normalized"
printf 'installer backup-stamp normalisation fired on %s file(s), %s case output(s)\n' \
    "$backup_stamps_normalized" "$stdout_stamps_normalized"
if [ "$accepted_count" -gt 0 ]; then
    printf '\nmaintainer-accepted divergences (desk ruling 2, DIV-010):\n'
    printf '  %s\n' "${accepted_ids[@]}"
fi
if [ "$fail" -gt 0 ]; then
    printf '\nfailing:\n'
    printf '  %s\n' "${failed_ids[@]}"
    printf '\ndiffs: %s\n' "$DIFFS"
    exit 1
fi
printf 'byte-identical on every case.\n'
exit 0

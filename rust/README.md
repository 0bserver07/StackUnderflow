# stax — the Rust port of StackUnderflow

The staxtrace engine: a byte-parity reimplementation of the StackUnderflow
CLI, server, and sidecars. Branch `rust`, local-only. Everything below
describes what is built and proven **today**; the parity evidence lives in
`TASKS-RS.md` (the ledger), `PERF.md` (every number with its command), and
`parity/` (the case matrices).

## Build

Requirements: the pinned toolchain (rustc 1.97.1 via rustup — the distro
cargo 1.75 cannot build edition-2024 crates; `ci.sh` guards this), and the
Python tree beside this directory (three data files are read from it at
build/run time — see "Coexistence").

```bash
cd rust/
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
```

Produces three binaries under `target/release/`:

| binary | what it is |
|---|---|
| `stax` | the CLI — 82 of the Python CLI's 105 commands, byte-identical (858-case harness, 0 FAIL) |
| `stax-server` | the dashboard server (axum) — serves the unmodified React bundle; 763-row endpoint matrix, 0 divergent |
| `stax-hooks` | the hook sidecar — all 9 Claude Code hook entry points at 2–5 ms (Python: 250–400 ms) |

## Use

`stax` is a drop-in for `stackunderflow` on every ported verb — same flags,
same output bytes, same exit codes:

```bash
./target/release/stax memory decisions "cache"     # past decisions on a topic
./target/release/stax memory file <path>           # a file's history
./target/release/stax resume                       # session/resume ids per agent
./target/release/stax status                       # spend summary (today + month)
./target/release/stax store                        # store schema + row counts
./target/release/stax report -p all                # the dashboard-style report
./target/release/stax backup create                # rsync --link-dest incrementals
./target/release/stax sync init                    # age-encrypted cross-device sync
./target/release/stax --help                       # the full tree
```

Server (never on :8095 — that is the Python instance's port):

```bash
./target/release/stax start --port 8096            # boots stax-server, ready-handshake
```

Not yet ported (23 commands, each with a named blocker in TASKS-RS.md):
the `analyze` family, `discovery` maintenance, `doctor`, `etl`, `import`,
`ingest` (needs GitHub network), `memory embed`, `pricing doctor`,
`reindex`, `risk`.

## Coexistence with the Python install

The maintainer's pyproject still owns the installed `stax` entry point;
flipping it to this binary is the wave-10 decommission decision. Until
then, invoke the built binary by path or shell alias. Three runtime/build
couplings to the Python tree are deliberate (drift = build failure, not
divergence): `stackunderflow/data/models.toml` and
`stackunderflow/store/migrations/*.sql` are compiled in via `include_str!`;
`adapters/capabilities.json` and the React bundle under
`stackunderflow/static/react/` are read at runtime. Decommissioning Python
therefore means shipping those files with the binaries — the plan is in
`docs/specs/decommission-report.md` §4.

## Verify

```bash
./ci.sh            # the full battery: gates 0–7 (clean-checkout build, fmt,
                   # clippy -D warnings, workspace tests, CLI byte-parity,
                   # ingest parity, endpoint byte-parity)
```

Standalone differs (each documents its own invocation): `hooks-parity.sh`,
`sync-parity.sh`, `backup-differ.sh`, `schema-differ.sh`, `init-differ.sh`,
`reports-clock-differ.sh`, `project-set-differ.sh`, `endpoint-parity.sh`.

## The browser demo

`demo/index.html` — drop a `store.db` into the page and query it with the
same compiled SQL the CLI runs; no network requests, enforced by CSP and
asserted by `demo` smoke tests. See wave 9 in the ledger.

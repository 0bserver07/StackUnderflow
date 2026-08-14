---
title: Installation
description: How to install staxtrace and open the dashboard.
---

:::note[The engine is Rust now]
staxtrace's engine is the Rust workspace under `rust/`. Build it from source
(Rust 1.89+) — prebuilt binaries are coming to GitHub Releases. The Python
instructions below cover the previous implementation, which lives on the
`python-legacy` branch in maintenance mode, reads the same store, and whose
old PyPI packages have been removed — install it from this repo if you need it.
:::

## Install the Rust engine

```bash
git clone https://github.com/0bserver07/staxtrace
cd staxtrace/rust && cargo build --release
cargo install --path crates/stax-cli --path crates/stax-server --path crates/stax-hooks
stax hooks install
stax init
```

:::caution[Install all three binaries]
The build produces **three** binaries and the product needs all of them:

| binary | what it does |
| --- | --- |
| `stax` | the command you type |
| `stax-server` | the dashboard |
| `stax-hooks` | the injection fast path |

`stax` finds the other two *next to its own executable*. Install only `stax` and
`stax start` fails with `No such file or directory` — that is it looking for
`stax-server`, not a missing store or config.

Earlier revisions of these docs suggested symlinking `target/release/stax` onto
your `PATH`. Don't: it installs one third of the product and pins your `PATH` to
a build directory that the next `cargo clean` or branch switch empties. The
binaries carry every data file they read, so `cargo install` — or copying all
three into one directory — works from anywhere.
:::

`stax hooks install` is a separate step from installing the binaries, and it is
what makes staxtrace proactive: it registers the lifecycle hooks that surface
prior session context, and cross-machine messages from `stax msg send`, into a
live agent turn. Skip it and the CLI still answers every query you type by
hand — but nothing arrives on its own, and an inbox message is never delivered.
The command is idempotent, backs the settings file up first, and only ever
touches its own entries. Verify with `stax hooks status`.

## Requirements

- **Python 3.10 or newer**
- An existing `~/.claude/` directory from having used [Claude Code](https://claude.ai/code). Adapters for more coding agents are on the way.

## Install the Python implementation (python-legacy)

The old PyPI packages have been removed — install from this repo's
`python-legacy` branch:

```bash
git clone --branch python-legacy https://github.com/0bserver07/staxtrace
cd staxtrace && pip install .
```

:::caution[This shadows the Rust binary]
The legacy package installs **two** console scripts — `stackunderflow` and a
`stax` alias — and the alias lands on your `PATH` over the native Rust `stax`.
If you use both, re-run the `cargo install` above after any Python
(re)install, and confirm with `stax --version` (the native binary answers
`stax 0.0.0`, the alias answers `stackunderflow, version 0.9.x`).
:::

## Launch the dashboard

```bash
stax init
```

This:

1. Ingests every session under `~/.claude/projects/` into a local SQLite store at `~/.stackunderflow/store.db`.
2. Starts a FastAPI server at `http://127.0.0.1:8081` (or the next free port).
3. Opens the dashboard in your default browser.

Use `Ctrl+C` to stop.

## Common first-run commands

```bash
stax status          # one-liner: today and this month
stax today           # today's usage per project
stax month           # this month's usage per project
stax report -p 7days # custom date-ranged report
stax --help          # everything else
```

If port 8081 is taken, configure a different one:

```bash
stax cfg set port 8099
stax init
```

## Where things live

| Path | Purpose |
|---|---|
| `~/.stackunderflow/store.db` | SQLite session store (can be several GB once populated) |
| `~/.stackunderflow/config.json` | Your persistent settings (port, filters, etc.) |
| `~/.stackunderflow/cache/pricing.json` | Cached model pricing from LiteLLM |
| `~/.claude/` | Read-only source data — staxtrace never writes here |

To start over from scratch, delete `~/.stackunderflow/store.db` and run `stax reindex`.

## Upgrade

```bash
git pull && cd rust && cargo build --release
cargo install --path crates/stax-cli --path crates/stax-server --path crates/stax-hooks
```

(For the Python implementation: `git pull` on the `python-legacy` checkout —
the install is from the repo, there is no package to upgrade.) The ingest
pipeline is incremental, so re-running `stax init` after an upgrade only
processes new or changed session files.

## Install from source

See the [Development guide](/staxtrace/dev-guide/) for source setup, which additionally requires Node 18+ to build the React dashboard.

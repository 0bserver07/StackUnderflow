---
title: Installation
description: How to install StackUnderflow and open the dashboard.
---

## Requirements

- **Python 3.10 or newer**
- An existing `~/.claude/` directory from having used [Claude Code](https://claude.ai/code). Adapters for more coding agents are on the way.

## Install from PyPI

```bash
pip install stackunderflow
```

Or with `pipx` if you prefer isolated CLI installs:

```bash
pipx install stackunderflow
```

## Launch the dashboard

```bash
stackunderflow init
```

This:

1. Ingests every session under `~/.claude/projects/` into a local SQLite store at `~/.stackunderflow/store.db`.
2. Starts a FastAPI server at `http://127.0.0.1:8081` (or the next free port).
3. Opens the dashboard in your default browser.

Use `Ctrl+C` to stop.

## Common first-run commands

```bash
stackunderflow status          # one-liner: today and this month
stackunderflow today           # today's usage per project
stackunderflow month           # this month's usage per project
stackunderflow report -p 7days # custom date-ranged report
stackunderflow --help          # everything else
```

If port 8081 is taken, configure a different one:

```bash
stackunderflow cfg set port 8099
stackunderflow init
```

## Where things live

| Path | Purpose |
|---|---|
| `~/.stackunderflow/store.db` | SQLite session store (can be several GB once populated) |
| `~/.stackunderflow/config.json` | Your persistent settings (port, filters, etc.) |
| `~/.stackunderflow/cache/pricing.json` | Cached model pricing from LiteLLM |
| `~/.claude/` | Read-only source data — StackUnderflow never writes here |

To start over from scratch, delete `~/.stackunderflow/store.db` and run `stackunderflow reindex`.

## Upgrade

```bash
pip install --upgrade stackunderflow
```

The ingest pipeline is incremental, so re-running `stackunderflow init` after an upgrade only processes new or changed session files.

## Install from source

See the [Development guide](/StackUnderflow/dev-guide/) for source setup, which additionally requires Node 18+ to build the React dashboard.

# StackUnderflow CLI Reference

StackUnderflow ships a single `stackunderflow` binary that covers dashboard launch, usage reports,
data export, config management, and session backups. All persistent state lives under
`~/.stackunderflow/` (config at `~/.stackunderflow/config.json`, session data at
`~/.stackunderflow/store.db`). Every command accepts `--help` for a quick reminder.

---

## Command Overview

```
# Dashboard
stackunderflow init [--port N] [--host H] [--no-browser] [--clear-cache]
stackunderflow start [-p N] [-H H] [--headless] [--fresh]
stackunderflow reindex
stackunderflow clear-cache [PROJECT]

# MCP server (also exposed as `stackunderflow-mcp` console script)
stackunderflow mcp

# Reports
stackunderflow status [--format text|json]
stackunderflow today [--format text|json] [--project P] [--exclude P]
stackunderflow month [--format text|json] [--project P] [--exclude P]
stackunderflow report [-p PERIOD] [--format text|json] [--project P] [--exclude P] [--provider PROV]
stackunderflow export -f csv|json -o PATH [-p today|week|month|all] [--provider X] [--project P] [--exclude P] [--force]
stackunderflow optimize [-p PERIOD] [--format text|json] [--project P] [--exclude P]

# Config  (legacy: config show/set/unset still works as hidden aliases for cfg ls/set/rm)
stackunderflow cfg ls [--json]
stackunderflow cfg set KEY VALUE
stackunderflow cfg rm KEY
stackunderflow cfg model-alias set FROM TO
stackunderflow cfg model-alias rm FROM
stackunderflow cfg model-alias ls [--json]

# Backup
stackunderflow backup create [--label TEXT] [--keep N]
stackunderflow backup list
stackunderflow backup restore NAME [--dry-run]
stackunderflow backup auto [--enable|--disable]
```

---

## Dashboard Commands

### `stackunderflow start`

Launch the StackUnderflow dashboard.

```
Usage: stackunderflow start [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --port` | INTEGER | from config | Server port |
| `-H, --host` | TEXT | from config | Bind address |
| `--headless` | flag | false | Don't open the browser |
| `--fresh` | flag | false | Clear disk cache before starting |

**Examples:**

```
$ stackunderflow start
  StackUnderflow is live at http://127.0.0.1:8081
  Ctrl+C to stop

$ stackunderflow start -p 9000 --headless
  StackUnderflow is live at http://127.0.0.1:9000
  Ctrl+C to stop

$ stackunderflow start --fresh
  cache cleared: /Users/you/.stackunderflow/cache
  StackUnderflow is live at http://127.0.0.1:8081
  Ctrl+C to stop
```

---

### `stackunderflow init`

Start the dashboard (alias for `start`). This is the primary user-facing command.
Flag names differ slightly from `start` for convenience.

```
Usage: stackunderflow init [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--port` | INTEGER | from config | Server port |
| `--host` | TEXT | from config | Bind address |
| `--no-browser` | flag | false | Don't open the browser (maps to `--headless`) |
| `--clear-cache` | flag | false | Clear disk cache first (maps to `--fresh`) |

**Examples:**

```
$ stackunderflow init
$ stackunderflow init --port 9000 --no-browser
$ stackunderflow init --clear-cache
```

---

### `stackunderflow reindex`

Rebuild the session store from scratch. Reads all registered adapter sources and
re-ingests them into `~/.stackunderflow/store.db`. Use this after a schema migration
or if the store gets corrupted.

```
Usage: stackunderflow reindex [OPTIONS]
```

No options beyond `--help`.

**Example:**

```
$ stackunderflow reindex
Reindexing into /Users/you/.stackunderflow/store.db
Done: {'sessions': 412, 'messages': 58203}
```

---

### `stackunderflow clear-cache`

Wipe the on-disk Cursor parse cache (`~/.stackunderflow/cache/cursor-results.json`)
and print guidance on clearing the rest. The Cursor parse cache is a
fingerprint-keyed snapshot of the parsed `state.vscdb` records; deleting it
forces the next ingest to re-read SQLite from scratch (slow but always
correct). The in-memory cache is always cleared on restart; pass `--fresh`
to `start` to also wipe the broader disk cache.

```
Usage: stackunderflow clear-cache [OPTIONS] [PROJECT]
```

| Argument | Required | Description |
|---|---|---|
| `PROJECT` | no | (reserved, currently unused) |

**Example:**

```
$ stackunderflow clear-cache
  cursor parse cache cleared.
  in-memory cache is cleared on restart.
  use `stackunderflow start --fresh` to also wipe the disk cache.
```

> To actually wipe the disk cache: `stackunderflow start --fresh`

---

## MCP Server

### `stackunderflow mcp`

Run the MCP (Model Context Protocol) server over stdio. Equivalent to the
standalone `stackunderflow-mcp` console script — both are wired to the same
entry point. Use this if you prefer one binary; use `stackunderflow-mcp` if
the client config you're pasting expects a single-word command.

```
Usage: stackunderflow mcp [OPTIONS]
```

The server exposes one tool, `session_query`, that any MCP client (Claude
Desktop, Claude Code, Cursor) can call to read local Claude-Code-format
session logs across `~/.claude*` directories. Stateless — does not read or
write StackUnderflow's SQLite store.

**Example Claude Desktop config:**

```json
{
  "mcpServers": {
    "stackunderflow": {
      "command": "stackunderflow-mcp"
    }
  }
}
```

See [`docs/mcp.md`](mcp.md) for the full tool reference, supported agent
roots, architectural rationale, and known limitations.

---

## Report Commands

### `stackunderflow status`

Compact one-liner showing today's and this month's cost and message counts.
Equivalent to running `today` and `month` together and condensing to a single line.

```
Usage: stackunderflow status [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow status
today: $34.61 (558 msg) | month: $558.65 (22681 msg)

$ stackunderflow status --format json
{
  "today": { ... },
  "month": { ... }
}
```

> See also: `today` and `month` for full per-project tables.

---

### `stackunderflow today`

Today's usage broken down by project.

```
Usage: stackunderflow today [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Example:**

```
$ stackunderflow today
StackUnderflow — today
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                       ┃   Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-my-api                         │ $15.21 │      116 │        1 │
│ -Users-you-dev-my-app                         │  $2.95 │      125 │        1 │
└───────────────────────────────────────────────┴────────┴──────────┴──────────┘
Total: $18.16  241 messages  2 sessions

$ stackunderflow today --project my-api --format json
```

---

### `stackunderflow month`

This month's usage broken down by project.

```
Usage: stackunderflow month [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Example:**

```
$ stackunderflow month
StackUnderflow — this month
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                      ┃    Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-StackUnderflow                │ $138.56 │    5,665 │       10 │
│ -Users-you-dev-my-api                        │  $91.91 │    2,939 │        3 │
└──────────────────────────────────────────────┴─────────┴──────────┴──────────┘
Total: $230.47  8,604 messages  13 sessions

$ stackunderflow month --exclude StackUnderflow
```

---

### `stackunderflow report`

Dashboard-style summary over a configurable date range.

```
Usage: stackunderflow report [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `7days` | Period: `today`, `7days`, `30days`, `month`, `all` |
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |
| `--provider` | `all\|claude\|codex\|cursor\|opencode\|pi\|copilot` | `all` | Provider filter (only `claude` and `all` supported today) |

Valid period strings: `today`, `7days`, `30days`, `month`, `all`. Any other value exits with
code 1 and prints `Unknown period`.

**Examples:**

```
$ stackunderflow report
StackUnderflow — last 7 days
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                      ┃    Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-StackUnderflow                │ $138.56 │    5,665 │       10 │
│ -Users-you-dev-chimera                       │  $91.91 │    2,939 │        3 │
└──────────────────────────────────────────────┴─────────┴──────────┴──────────┘
Total: $453.88  14,782 messages  48 sessions

$ stackunderflow report -p 30days --project StackUnderflow
$ stackunderflow report -p all --format json
$ stackunderflow report -p today --exclude sandbox
```

---

### `stackunderflow export`

Export aggregated, cross-project usage data to a file. Both `--format`
and `--output` are required. Designed for spreadsheets, BI tools, and
downstream automation.

```
Usage: stackunderflow export [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-f, --format` | `csv\|json` | (required) | Output format |
| `-o, --output` | PATH | (required) | File path to write |
| `-p, --period` | `today\|week\|month\|all` | (multi-period rollup) | Single window. Omit for today + 7 days + 30 days in one file. |
| `--provider` | TEXT | (all) | Filter by provider (e.g. `claude`, `codex`, `cursor`) |
| `--project` | TEXT | (all) | Include only this project slug (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project slug (repeatable) |
| `--force` | flag | false | Overwrite the output file if it already exists |

**CSV layout.** Each period section starts with a `# period: <label>`
comment row, followed by the daily-rows header
(`date, provider, project, cost_usd, calls, sessions, input_tokens,
output_tokens, cache_read_tokens, cache_write_tokens`) and the rows.
A blank line separates each period from the activity-breakdown section
(`# activity — <label>`, then `activity, calls, share_pct`). With no
`--period`, three pairs of sections are emitted (today / last 7 days /
last 30 days) in the same file.

**JSON layout.** With `--period`, the file is one period dict with
`label`, `since`, `until`, `totals`, `daily`, `projects`, `models`,
`activities`, `tools`, `mcp`, `shell`. Without `--period`, a top-level
`{schema, generated, filters, today, last_7d, last_30d}` envelope wraps
three of those dicts so a single file is enough for short / medium /
long windows side-by-side.

**File-write safety.** The command refuses to overwrite an existing
file unless `--force` is set, refuses to follow symlinks at the output
path, and writes atomically via a `.tmp` file that is renamed into
place. Parent directories are created if missing.

**Examples:**

```
$ stackunderflow export --format csv --output ~/usage-week.csv --period week
  wrote /Users/you/usage-week.csv

$ stackunderflow export -f json -o ~/usage-rollup.json
  wrote /Users/you/usage-rollup.json

$ stackunderflow export -f csv -o ~/claude-only.csv --provider claude --period month

$ stackunderflow export -f csv -o ~/big.csv -p all --exclude sandbox --force

$ jq '.last_30d.projects[] | select(.cost_usd > 10)' < ~/usage-rollup.json
```

The same data is available over HTTP at `GET /api/export` with the
matching query parameters (`format`, `period`, `provider`, `project`,
`exclude`) — used by the dashboard's "Download" button.

---

### `stackunderflow optimize`

Find wasted spend: sessions where the assistant had to retry repeatedly (looped Q&A pairs).

```
Usage: stackunderflow optimize [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `30days` | Period: `today`, `7days`, `30days`, `month`, `all` |
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Examples:**

```
$ stackunderflow optimize --period 7days
No looped Q&A pairs found in last 7 days.

$ stackunderflow optimize --period 30days
Waste report — last 30 days

  my-api: 3 looped pair(s)
    - How do I fix the auth middleware?
    - Why does the test keep failing?

$ stackunderflow optimize --period all --format json
```

---

## Config Commands

### `stackunderflow cfg ls`

Show all settings with their sources (`default`, `file`, or `env`).

```
Usage: stackunderflow cfg ls [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--json` | flag | false | JSON output instead of table |

**Examples:**

```
$ stackunderflow cfg ls
Settings:
  auto_browser                        False           [file]
  host                                127.0.0.1       [default]
  log_level                           INFO            [default]
  max_date_range_days                 30              [default]
  messages_initial_load               500             [default]
  port                                8095            [file]

$ stackunderflow cfg ls --json
{
  "port": 8095,
  "host": "127.0.0.1",
  "auto_browser": false,
  "max_date_range_days": 30,
  "messages_initial_load": 500,
  "log_level": "INFO"
}
```

> Legacy alias: `stackunderflow config show [--json]`

---

### `stackunderflow cfg set`

Write a key-value pair to the config file (`~/.stackunderflow/config.json`).

```
Usage: stackunderflow cfg set [OPTIONS] KEY VALUE
```

No options beyond `--help`.

**Examples:**

```
$ stackunderflow cfg set port 9000
  port = 9000

$ stackunderflow cfg set auto_browser false
  auto_browser = False

$ stackunderflow cfg set log_level DEBUG
  log_level = DEBUG
```

Valid keys: `port`, `host`, `auto_browser`, `max_date_range_days`,
`messages_initial_load`, `log_level`, `currency`. Passing an unknown key exits with an error.

> Legacy alias: `stackunderflow config set KEY VALUE`

---

### `stackunderflow cfg rm`

Remove a key from the config file, reverting it to its built-in default.

```
Usage: stackunderflow cfg rm [OPTIONS] KEY
```

No options beyond `--help`.

**Examples:**

```
$ stackunderflow cfg rm port
  port removed

$ stackunderflow cfg rm auto_browser
  auto_browser removed
```

> Legacy alias: `stackunderflow config unset KEY`

---

### `stackunderflow cfg model-alias`

Manage **model aliases** — a map from a proxy-rewritten model id to a
canonical id our pricing tables know about. Use this when sessions go
through OpenRouter, Replicate, LiteLLM, or an internal company gateway
that rewrites model names.

**Why you'd reach for this.** Suppose your sessions emit
`"model": "openrouter/claude-opus"` but our rate tables only know
`claude-opus-4-6`. Without an alias, `compute_cost()` falls into the
fallback rates (or even returns $0 once stricter resolution lands), and
spend on those sessions is silently misreported. Adding the alias
`openrouter/claude-opus → claude-opus-4-6` patches the gap so the
dashboard shows the real number.

```
Usage: stackunderflow cfg model-alias set FROM TO
       stackunderflow cfg model-alias rm  FROM
       stackunderflow cfg model-alias ls  [--json]
```

| Argument | Required | Description |
|---|---|---|
| `FROM` | yes (set / rm) | The proxy-rewritten id as it appears in your session logs |
| `TO`   | yes (set)      | The canonical id (must match a key our pricers recognise) |

| Option | Type | Default | Description |
|---|---|---|---|
| `--json` | flag | false | (`ls` only) JSON output instead of table |

**Resolution semantics.** Aliases are consulted **before** the
provider-specific canonicalize / identify logic in `compute_cost()`.
Resolution is single-step — no recursive chasing — so a chain like
`a → b → c` returns `b`, not `c`. Aliasing to an unknown canonical id
falls through to the existing fallback behaviour rather than looping.

**Examples:**

```
$ stackunderflow cfg model-alias set openrouter/claude-opus claude-opus-4-6
  openrouter/claude-opus -> claude-opus-4-6

$ stackunderflow cfg model-alias set litellm/sonnet claude-sonnet-4-6
  litellm/sonnet -> claude-sonnet-4-6

$ stackunderflow cfg model-alias ls
Model aliases:
  litellm/sonnet           ->  claude-sonnet-4-6
  openrouter/claude-opus   ->  claude-opus-4-6

$ stackunderflow cfg model-alias ls --json
{
  "litellm/sonnet": "claude-sonnet-4-6",
  "openrouter/claude-opus": "claude-opus-4-6"
}

$ stackunderflow cfg model-alias rm litellm/sonnet
  litellm/sonnet removed
```

**Worked end-to-end example.**

```
$ stackunderflow cfg model-alias set my-proxy claude-opus-4-6
  my-proxy -> claude-opus-4-6

$ python -c "from stackunderflow.infra.costs import compute_cost; \
print(compute_cost({'input': 1000, 'output': 1000}, 'my-proxy')['total_cost'])"
0.09
```

The alias map is stored under the `model_aliases` key in
`~/.stackunderflow/config.json`. Generic `cfg set model_aliases ...` is
intentionally rejected — use this dedicated subcommand instead.

---

## Backup Commands

### `stackunderflow backup create`

Create an incremental backup of all `~/.claude/` data. Backs up sessions, file history,
plans, tasks, todos, settings, shell snapshots, and prompt history. Excludes debug logs
and plugin binaries to save space. Uses hard links for efficiency — unchanged files cost
zero additional disk space.

```
Usage: stackunderflow backup create [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--label` | TEXT | (none) | Optional label appended to the backup directory name |
| `--keep` | INTEGER (>=1) | 10 | Max backups to retain; oldest are pruned automatically |

Backups are stored in `~/.stackunderflow/backups/` as timestamped directories
(`YYYYMMDD-HHMMSS[-label]`).

**Examples:**

```
$ stackunderflow backup create
  Backing up ~/.claude → /Users/you/.stackunderflow/backups/20260419-143209
  (excluding: debug, plugins, cache, statsig...)
  Done: 2884 files (1102 JSONL), 3216.6 MB

$ stackunderflow backup create --label pre-upgrade --keep 5
  Backing up ~/.claude → /Users/you/.stackunderflow/backups/20260419-143209-pre-upgrade
  (excluding: debug, plugins, cache, statsig...)
  Done: 2884 files (1102 JSONL), 3216.6 MB
```

---

### `stackunderflow backup list`

List all existing backups with their file counts and sizes.

```
Usage: stackunderflow backup list [OPTIONS]
```

No options beyond `--help`.

**Example:**

```
$ stackunderflow backup list
  7 backup(s) in /Users/you/.stackunderflow/backups

  20260409-153720-full                      (2743 files, 3018.2 MB)
  20260410-111823-test                      (2804 files, 3066.6 MB)
  20260414-175009-pre-upgrade               (2819 files, 3094.0 MB)
  20260419-143209-pre-upgrade               (2884 files, 3216.6 MB)
```

---

### `stackunderflow backup restore`

Restore `~/.claude/` from a named backup. Prompts for confirmation before overwriting.

```
Usage: stackunderflow backup restore [OPTIONS] NAME
```

| Argument | Required | Description |
|---|---|---|
| `NAME` | yes | Backup directory name as shown by `backup list` |

| Option | Type | Default | Description |
|---|---|---|---|
| `--dry-run` | flag | false | Show what would be restored without making any changes |

**Examples:**

```
$ stackunderflow backup restore 20260409-153720-full --dry-run
  Would restore 2743 files from /Users/you/.stackunderflow/backups/20260409-153720-full
  → /Users/you/.claude

$ stackunderflow backup restore 20260409-153720-full
  This will overwrite files in /Users/you/.claude. Continue? [y/N]: y
  Restoring 2743 files from ... → /Users/you/.claude
  Restore complete.
```

---

### `stackunderflow backup auto`

Set up or remove daily automatic backups. On macOS, installs a launchd plist
(`~/Library/LaunchAgents/com.stackunderflow.backup.plist`) that runs at 3:00 AM.
On Linux, prints the cron line to add manually via `crontab -e`.

```
Usage: stackunderflow backup auto [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--enable / --disable` | flag | `--enable` | Enable or disable daily backups |

**Examples:**

```
$ stackunderflow backup auto --enable
  Daily backup enabled (3:00 AM). Keeps last 10.
  Plist: /Users/you/Library/LaunchAgents/com.stackunderflow.backup.plist

$ stackunderflow backup auto --disable
  Automatic backups disabled.
```

---

## Config Keys Reference

| Key | Type | Default | Description |
|---|---|---|---|
| `port` | int | `8081` | HTTP port the dashboard server binds to |
| `host` | str | `127.0.0.1` | Address the server binds to |
| `auto_browser` | bool | `true` | Open the browser automatically on `start`/`init` |
| `max_date_range_days` | int | `30` | Maximum days allowed in a dashboard date range query |
| `messages_initial_load` | int | `500` | Number of messages loaded on initial dashboard view |
| `log_level` | str | `INFO` | Python logging level (`DEBUG`, `INFO`, `WARNING`, `ERROR`) |
| `currency` | str | `USD` | Display currency for cost figures — any 3-letter ISO 4217 code |
| `model_aliases` | dict[str,str] | `{}` | Proxy-rewritten model id → canonical id (manage via `cfg model-alias`) |

**Example — set, verify, then reset a key:**

```
$ stackunderflow cfg set port 9000
  port = 9000
$ stackunderflow cfg ls
Settings:
  ...
  port                                9000            [file]
  ...
$ stackunderflow cfg rm port
  port removed
```

---

## Environment Variables

Every config key can be overridden by an environment variable. The variable name is the
second argument to the `_Opt` descriptor in `stackunderflow/settings.py` (shown below).
Environment variables take precedence over the config file.

| Config key | Env var | Example |
|---|---|---|
| `port` | `PORT` | `PORT=9000 stackunderflow start` |
| `host` | `HOST` | `HOST=0.0.0.0 stackunderflow start` |
| `auto_browser` | `AUTO_BROWSER` | `AUTO_BROWSER=false stackunderflow start` |
| `max_date_range_days` | `MAX_DATE_RANGE_DAYS` | `MAX_DATE_RANGE_DAYS=90 stackunderflow start` |
| `messages_initial_load` | `MESSAGES_INITIAL_LOAD` | `MESSAGES_INITIAL_LOAD=1000 stackunderflow start` |
| `log_level` | `LOG_LEVEL` | `LOG_LEVEL=DEBUG stackunderflow start` |
| `currency` | `STACKUNDERFLOW_CURRENCY` | `STACKUNDERFLOW_CURRENCY=GBP stackunderflow start` |

Boolean env vars accept `1`, `true`, `yes`, `on` (case-insensitive) as truthy values;
anything else is treated as false.

**Resolution order (highest to lowest):**
1. Environment variable
2. Config file (`~/.stackunderflow/config.json`)
3. Built-in default

---

## Currency

Cost figures default to USD. To display them in another currency set the `currency`
key to any 3-letter ISO 4217 code (`stackunderflow cfg set currency GBP` or
`STACKUNDERFLOW_CURRENCY=EUR`). Validation only checks the format — runtime resolves
the rate via the public Frankfurter API (ECB FX data, no auth) and caches it for
24h at `~/.stackunderflow/cache/exchange-rate.json`. Cost computation stays in USD
internally; conversion happens at the API boundary, so the cached rate-card and the
displayed numbers are independent. If the FX fetch fails and the cache is empty or
stale-and-uncached, responses fall back to USD with `rate_from_usd=1.0` rather than
crash. Every API endpoint that returns dollar figures now also returns a
`currency: {code, symbol, rate_from_usd}` block at the top level so the frontend
can render symbols and labels without a second round-trip.

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| non-zero | Error |

Invalid period strings (e.g. `stackunderflow report -p yesterday`) exit with code 1
and print `Unknown period`. Invalid config keys passed to `cfg set` exit with an error
message listing the valid keys.

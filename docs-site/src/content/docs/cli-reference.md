---
title: CLI reference
description: Every stax command, flag, and subcommand.
---

# staxtrace CLI Reference

staxtrace ships a single `stackunderflow` binary that covers dashboard launch, usage reports,
data export, config management, and session backups. All persistent state lives under
`~/.stackunderflow/` (config at `~/.stackunderflow/config.json`, session data at
`~/.stackunderflow/store.db`). Every command accepts `--help` for a quick reminder.

---

## Command Overview

```
# Dashboard
stax init [--port N] [--host H] [--no-browser] [--clear-cache]
stax start [-p N] [-H H] [--headless] [--fresh]
stax reindex
stax clear-cache [PROJECT]

# Reports
stax status [--format text|json]
stax today [--format text|json] [--project P] [--exclude P]
stax month [--format text|json] [--project P] [--exclude P]
stax report [-p PERIOD] [--format text|json] [--project P] [--exclude P] [--provider PROV]
stax export [-p PERIOD] [-f csv|json] [--project P] [--exclude P]
stax optimize [-p PERIOD] [--format text|json] [--project P] [--exclude P]

# Config  (legacy: config show/set/unset still works as hidden aliases for cfg ls/set/rm)
stax cfg ls [--json]
stax cfg set KEY VALUE
stax cfg rm KEY

# Backup
stax backup create [--label TEXT] [--keep N]
stax backup list
stax backup restore NAME [--dry-run]
stax backup auto [--enable|--disable]
```

---

## Dashboard Commands

### `stax start`

Launch the staxtrace dashboard.

```
Usage: stax start [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --port` | INTEGER | from config | Server port |
| `-H, --host` | TEXT | from config | Bind address |
| `--headless` | flag | false | Don't open the browser |
| `--fresh` | flag | false | Clear disk cache before starting |

**Examples:**

```
$ stax start
  staxtrace is live at http://127.0.0.1:8081
  Ctrl+C to stop

$ stax start -p 9000 --headless
  staxtrace is live at http://127.0.0.1:9000
  Ctrl+C to stop

$ stax start --fresh
  cache cleared: /Users/you/.stackunderflow/cache
  staxtrace is live at http://127.0.0.1:8081
  Ctrl+C to stop
```

---

### `stax init`

Start the dashboard (alias for `start`). This is the primary user-facing command.
Flag names differ slightly from `start` for convenience.

```
Usage: stax init [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--port` | INTEGER | from config | Server port |
| `--host` | TEXT | from config | Bind address |
| `--no-browser` | flag | false | Don't open the browser (maps to `--headless`) |
| `--clear-cache` | flag | false | Clear disk cache first (maps to `--fresh`) |

**Examples:**

```
$ stax init
$ stax init --port 9000 --no-browser
$ stax init --clear-cache
```

---

### `stax reindex`

Rebuild the session store from scratch. Reads all registered adapter sources and
re-ingests them into `~/.stackunderflow/store.db`. Use this after a schema migration
or if the store gets corrupted.

```
Usage: stax reindex [OPTIONS]
```

No options beyond `--help`.

**Example:**

```
$ stax reindex
Reindexing into /Users/you/.stackunderflow/store.db
Done: {'sessions': 412, 'messages': 58203}
```

---

### `stax clear-cache`

Print guidance on clearing the in-memory and disk caches. The in-memory cache is
always cleared on restart; pass `--fresh` to `start` to also wipe the disk cache.

```
Usage: stax clear-cache [OPTIONS] [PROJECT]
```

| Argument | Required | Description |
|---|---|---|
| `PROJECT` | no | (reserved, currently unused) |

**Example:**

```
$ stax clear-cache
  in-memory cache is cleared on restart.
  use `stax start --fresh` to also wipe the disk cache.
```

> To actually wipe the disk cache: `stax start --fresh`

---

## Report Commands

### `stax status`

Compact one-liner showing today's and this month's cost and message counts.
Equivalent to running `today` and `month` together and condensing to a single line.

```
Usage: stax status [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stax status
today: $34.61 (558 msg) | month: $558.65 (22681 msg)

$ stax status --format json
{
  "today": { ... },
  "month": { ... }
}
```

> See also: `today` and `month` for full per-project tables.

---

### `stax today`

Today's usage broken down by project.

```
Usage: stax today [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Example:**

```
$ stax today
staxtrace — today
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                       ┃   Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-my-api                         │ $15.21 │      116 │        1 │
│ -Users-you-dev-my-app                         │  $2.95 │      125 │        1 │
└───────────────────────────────────────────────┴────────┴──────────┴──────────┘
Total: $18.16  241 messages  2 sessions

$ stax today --project my-api --format json
```

---

### `stax month`

This month's usage broken down by project.

```
Usage: stax month [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Example:**

```
$ stax month
staxtrace — this month
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                      ┃    Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-staxtrace                │ $138.56 │    5,665 │       10 │
│ -Users-you-dev-my-api                        │  $91.91 │    2,939 │        3 │
└──────────────────────────────────────────────┴─────────┴──────────┴──────────┘
Total: $230.47  8,604 messages  13 sessions

$ stax month --exclude staxtrace
```

---

### `stax report`

Dashboard-style summary over a configurable date range.

```
Usage: stax report [OPTIONS]
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
$ stax report
staxtrace — last 7 days
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                      ┃    Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-you-dev-staxtrace                │ $138.56 │    5,665 │       10 │
│ -Users-you-dev-chimera                       │  $91.91 │    2,939 │        3 │
└──────────────────────────────────────────────┴─────────┴──────────┴──────────┘
Total: $453.88  14,782 messages  48 sessions

$ stax report -p 30days --project staxtrace
$ stax report -p all --format json
$ stax report -p today --exclude sandbox
```

---

### `stax export`

Export aggregated data as CSV or JSON. Useful for spreadsheets or downstream tooling.
`export --format json` is equivalent to `report --format json`.

```
Usage: stax export [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `30days` | Period: `today`, `7days`, `30days`, `month`, `all` |
| `-f, --format` | `csv\|json` | csv | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Examples:**

```
$ stax export --period today --format csv
project,cost,messages,sessions
-Users-you-dev-my-api,15.21,116,1
-Users-you-dev-staxtrace,2.95,125,1

$ stax export --period today --format json
{
  "total_cost": 34.61,
  "total_messages": 558,
  ...
}

$ stax export -p 30days -f csv > usage.csv
$ stax export -p all -f json | jq '.projects[] | select(.cost > 10)'
```

---

### `stax optimize`

Find wasted spend: sessions where the assistant had to retry repeatedly (looped Q&A pairs).

```
Usage: stax optimize [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `30days` | Period: `today`, `7days`, `30days`, `month`, `all` |
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |

**Examples:**

```
$ stax optimize --period 7days
No looped Q&A pairs found in last 7 days.

$ stax optimize --period 30days
Waste report — last 30 days

  my-api: 3 looped pair(s)
    - How do I fix the auth middleware?
    - Why does the test keep failing?

$ stax optimize --period all --format json
```

---

## Config Commands

### `stax cfg ls`

Show all settings with their sources (`default`, `file`, or `env`).

```
Usage: stax cfg ls [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--json` | flag | false | JSON output instead of table |

**Examples:**

```
$ stax cfg ls
Settings:
  auto_browser                        False           [file]
  host                                127.0.0.1       [default]
  log_level                           INFO            [default]
  max_date_range_days                 30              [default]
  messages_initial_load               500             [default]
  port                                8095            [file]

$ stax cfg ls --json
{
  "port": 8095,
  "host": "127.0.0.1",
  "auto_browser": false,
  "max_date_range_days": 30,
  "messages_initial_load": 500,
  "log_level": "INFO"
}
```

> Legacy alias: `stax config show [--json]`

---

### `stax cfg set`

Write a key-value pair to the config file (`~/.stackunderflow/config.json`).

```
Usage: stax cfg set [OPTIONS] KEY VALUE
```

No options beyond `--help`.

**Examples:**

```
$ stax cfg set port 9000
  port = 9000

$ stax cfg set auto_browser false
  auto_browser = False

$ stax cfg set log_level DEBUG
  log_level = DEBUG
```

Valid keys: `port`, `host`, `auto_browser`, `max_date_range_days`,
`messages_initial_load`, `log_level`. Passing an unknown key exits with an error.

> Legacy alias: `stax config set KEY VALUE`

---

### `stax cfg rm`

Remove a key from the config file, reverting it to its built-in default.

```
Usage: stax cfg rm [OPTIONS] KEY
```

No options beyond `--help`.

**Examples:**

```
$ stax cfg rm port
  port removed

$ stax cfg rm auto_browser
  auto_browser removed
```

> Legacy alias: `stax config unset KEY`

---

## Backup Commands

### `stax backup create`

Create an incremental backup of all `~/.claude/` data. Backs up sessions, file history,
plans, tasks, todos, settings, shell snapshots, and prompt history. Excludes debug logs
and plugin binaries to save space. Uses hard links for efficiency — unchanged files cost
zero additional disk space.

```
Usage: stax backup create [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--label` | TEXT | (none) | Optional label appended to the backup directory name |
| `--keep` | INTEGER (>=1) | 10 | Max backups to retain; oldest are pruned automatically |

Backups are stored in `~/.stackunderflow/backups/` as timestamped directories
(`YYYYMMDD-HHMMSS[-label]`).

**Examples:**

```
$ stax backup create
  Backing up ~/.claude → /Users/you/.stackunderflow/backups/20260419-143209
  (excluding: debug, plugins, cache, statsig...)
  Done: 2884 files (1102 JSONL), 3216.6 MB

$ stax backup create --label pre-upgrade --keep 5
  Backing up ~/.claude → /Users/you/.stackunderflow/backups/20260419-143209-pre-upgrade
  (excluding: debug, plugins, cache, statsig...)
  Done: 2884 files (1102 JSONL), 3216.6 MB
```

---

### `stax backup list`

List all existing backups with their file counts and sizes.

```
Usage: stax backup list [OPTIONS]
```

No options beyond `--help`.

**Example:**

```
$ stax backup list
  7 backup(s) in /Users/you/.stackunderflow/backups

  20260409-153720-full                      (2743 files, 3018.2 MB)
  20260410-111823-test                      (2804 files, 3066.6 MB)
  20260414-175009-pre-upgrade               (2819 files, 3094.0 MB)
  20260419-143209-pre-upgrade               (2884 files, 3216.6 MB)
```

---

### `stax backup restore`

Restore `~/.claude/` from a named backup. Prompts for confirmation before overwriting.

```
Usage: stax backup restore [OPTIONS] NAME
```

| Argument | Required | Description |
|---|---|---|
| `NAME` | yes | Backup directory name as shown by `backup list` |

| Option | Type | Default | Description |
|---|---|---|---|
| `--dry-run` | flag | false | Show what would be restored without making any changes |

**Examples:**

```
$ stax backup restore 20260409-153720-full --dry-run
  Would restore 2743 files from /Users/you/.stackunderflow/backups/20260409-153720-full
  → /Users/you/.claude

$ stax backup restore 20260409-153720-full
  This will overwrite files in /Users/you/.claude. Continue? [y/N]: y
  Restoring 2743 files from ... → /Users/you/.claude
  Restore complete.
```

---

### `stax backup auto`

Set up or remove daily automatic backups. On macOS, installs a launchd plist
(`~/Library/LaunchAgents/com.stackunderflow.backup.plist`) that runs at 3:00 AM.
On Linux, prints the cron line to add manually via `crontab -e`.

```
Usage: stax backup auto [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--enable / --disable` | flag | `--enable` | Enable or disable daily backups |

**Examples:**

```
$ stax backup auto --enable
  Daily backup enabled (3:00 AM). Keeps last 10.
  Plist: /Users/you/Library/LaunchAgents/com.stackunderflow.backup.plist

$ stax backup auto --disable
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

**Example — set, verify, then reset a key:**

```
$ stax cfg set port 9000
  port = 9000
$ stax cfg ls
Settings:
  ...
  port                                9000            [file]
  ...
$ stax cfg rm port
  port removed
```

---

## Environment Variables

Every config key can be overridden by an environment variable. The variable name is the
second argument to the `_Opt` descriptor in `stackunderflow/settings.py` (shown below).
Environment variables take precedence over the config file.

| Config key | Env var | Example |
|---|---|---|
| `port` | `PORT` | `PORT=9000 stax start` |
| `host` | `HOST` | `HOST=0.0.0.0 stax start` |
| `auto_browser` | `AUTO_BROWSER` | `AUTO_BROWSER=false stax start` |
| `max_date_range_days` | `MAX_DATE_RANGE_DAYS` | `MAX_DATE_RANGE_DAYS=90 stax start` |
| `messages_initial_load` | `MESSAGES_INITIAL_LOAD` | `MESSAGES_INITIAL_LOAD=1000 stax start` |
| `log_level` | `LOG_LEVEL` | `LOG_LEVEL=DEBUG stax start` |

Boolean env vars accept `1`, `true`, `yes`, `on` (case-insensitive) as truthy values;
anything else is treated as false.

**Resolution order (highest to lowest):**
1. Environment variable
2. Config file (`~/.stackunderflow/config.json`)
3. Built-in default

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| non-zero | Error |

Invalid period strings (e.g. `stax report -p yesterday`) exit with code 1
and print `Unknown period`. Invalid config keys passed to `cfg set` exit with an error
message listing the valid keys.

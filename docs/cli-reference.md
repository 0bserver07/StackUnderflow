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
stackunderflow compare [-p today|week|month|all] [--provider X] [--project P] [--format text|json]
stackunderflow yield [-p PERIOD] [--format text|json] [--project SLUG]
stackunderflow context-budget [--project DIR] [--global] [--format text|json]

# Config  (legacy: config show/set/unset still works as hidden aliases for cfg ls/set/rm)
stackunderflow cfg ls [--json]
stackunderflow cfg set KEY VALUE
stackunderflow cfg rm KEY
stackunderflow cfg model-alias set FROM TO
stackunderflow cfg model-alias rm FROM
stackunderflow cfg model-alias ls [--json]

# Plan budgets
stackunderflow plan show [--format text|json]
stackunderflow plan set NAME [--monthly-usd N] [--reset-day D]
stackunderflow plan reset

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

### `stackunderflow compare`

Side-by-side per-model comparison over a time window — answers "is it worth using Opus for this kind of work?" by surfacing one-shot rate, retry rate, cache hit rate, and unit economics ($/call, $/session) per model.

```
Usage: stackunderflow compare [OPTIONS]
### `stackunderflow yield`

Yield analysis — correlate AI sessions with the git commit history of their `cwd`.

For each session in the window, the command resolves the session's `cwd` to a
git repo, runs `git log` over the 24h after the session started, and
classifies the result:

| Class        | Meaning                                                                 |
|--------------|-------------------------------------------------------------------------|
| `productive` | A commit landed within 24h and is still reachable from `HEAD`.          |
| `reverted`   | A commit landed but was later reverted (by `git revert` or by being wiped from `HEAD` via reset / force push). |
| `abandoned`  | No commit landed within the window.                                     |
| `no_repo`    | The session's `cwd` is missing or isn't a git repository.               |

> **Heuristic warning.** This correlates by **time**, not by content. A commit
> inside the 24h window is credited to the session even if it was about
> something else. Treat the breakdown as a smoke signal, not a verdict.
> Sessions and commits don't have to share a topic to get matched up.

```
Usage: stackunderflow yield [OPTIONS]
### `stackunderflow context-budget`

Estimate the per-session "context tax" — the tokens every Claude Code
turn pays before the user types: system prompt + registered MCP servers
+ available skills + agent definitions + memory files (project
`CLAUDE.md`, global `~/.claude/CLAUDE.md`).

```
Usage: stackunderflow context-budget [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | `today\|week\|month\|all` | `month` | Window over which to compare |
| `--provider` | TEXT | (all) | Filter by provider id (`claude`, `codex`, `cursor`, …) |
| `--project` | TEXT | (all) | Restrict to this project slug (repeatable) |
| `--format` | `text\|json` | text | Output format |

**Metrics:**

- **1-shot %** — fraction of sessions where the user asked once, the assistant answered once, and that was it (heuristic: exactly 1 user + 1 assistant message).
- **Retry rate** — `(assistant_messages / sessions) - 1`, i.e. the average number of *extra* assistant turns per session beyond the first answer.
- **Cache %** — `cache_read / (cache_read + cache_create)`, the prompt-cache hit rate; high is good (most prompt context was reused).
- **$/call** — `total_cost / calls` (per assistant message).
- **$/session** — `total_cost / sessions` (sessions are attributed to whichever model dominated the session — most assistant messages wins, ties broken alphabetically).
- **Total $** — sum of `compute_cost` over every assistant message in the window.
| `--project` | PATH | cwd | Project directory to inspect |
| `--global` | flag | false | Estimate the global budget only (`~/.claude`); ignore project files |
| `--format` | `text\|json` | text | Output format |

**Estimation heuristic — explicit and approximate.**

| Source | Cost |
|---|---|
| System prompt | Fixed `DEFAULT_SYSTEM_PROMPT_TOKENS=3000` (Claude Code default, public scratch count) |
| `CLAUDE.md` files | `len(content) // 4` — 1 token ≈ 4 characters of English |
| MCP server | `MCP_BASE_TOKENS=200` + `MCP_PER_TOOL_TOKENS=50` × declared tools, **or** `MCP_UNKNOWN_TOOLS_FALLBACK=200` flat when tool counts aren't statically known (the common case) |
| Skill (`SKILL.md`) | `len(content) // 4` |
| Subagent (`*.md`) | `len(content) // 4` |
| Cost projection | Total tokens × `$3/M` (current Sonnet input rate) per session, × 100 sessions/month |

**Underestimates code-heavy or non-Latin content; the full heuristic
string is echoed in every output payload (`heuristic` field in JSON).
Useful for spotting bloat — not for billing.**

A budget over 20k tokens triggers a yellow warning in the text output
and a `context_budget_bloat` finding (severity: medium) when consumed
by the optimize report.

**Examples:**

```
$ stackunderflow compare
                              Compare — month
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━┳━━━━━━━━━┳━━━━━━━┳━━━━━━━━┳━━━━━━━━━┳━━━━━━━━━━━┳━━━━━━━━┓
┃ Model                      ┃ Sessions ┃ Calls ┃ 1-shot% ┃ Retry ┃ Cache% ┃ $/call  ┃ $/session ┃ Total$ ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━╇━━━━━━━━━╇━━━━━━━╇━━━━━━━━╇━━━━━━━━━╇━━━━━━━━━━━╇━━━━━━━━┩
│ claude-opus-4-6            │       12 │   480 │   16.7% │ 39.00 │  92.4% │ $0.0210 │     $0.84 │ $10.07 │
│ claude-sonnet-4-6          │       38 │ 1,240 │   34.2% │ 31.63 │  88.1% │ $0.0034 │     $0.11 │  $4.21 │
│ gpt-5                      │        4 │    18 │   75.0% │  3.50 │   0.0% │ $0.0040 │     $0.02 │  $0.07 │
└────────────────────────────┴──────────┴───────┴─────────┴───────┴────────┴─────────┴───────────┴────────┘

$ stackunderflow compare --period week --provider claude --format json
{
  "period": "week",
  "models": [
    {
      "model": "claude-opus-4-6",
      "provider": "claude",
      "sessions": 3,
      "calls": 124,
      "one_shot_pct": 0.0,
      "retry_rate": 40.33,
      "cache_hit_rate": 0.94,
      "cost_per_call": 0.018,
      "cost_per_session": 0.74,
      "total_cost": 2.21,
      "total_tokens": 4_120_000
    }
  ],
  "generated": 1746125443.117
}
```

The same data is available via `GET /api/compare` — see `docs/api-reference.md`.
| `-p, --period` | TEXT | `month` | Period: `today`, `week`, `month`, `all`, `7days`, `30days` |
| `--project` | TEXT | (all) | Filter by project slug (repeatable) |
| `--format` | `text\|json` | text | Output format |

**Examples:**

```
$ stackunderflow yield -p week
Yield analysis — period: week
  productive:   13  ($724.77)
  reverted:      0  ($0.00)
  abandoned:     8  ($1080.25)
  no_repo:       3  ($0.92)
  total:        24  ($1805.94)

Top sessions by cost:
  CLASS            COST  PROJECT                       SESSION
  abandoned    $ 266.39  -Users-yadkonrad-dev-dev-yea  910a9d68-...
  productive   $ 247.88  -Users-yadkonrad-dev-dev-yea  ada0010e-...
  ...

  note: yield is correlated by time, not by content — a commit within 24h is credited to the session even if unrelated.

$ stackunderflow yield -p month --format json
```

The same data is available over HTTP at `GET /api/yield` with the matching
query parameters (`period`, `project`).

**Limits:**

- Sessions whose `cwd` lives on a path you've since deleted, renamed, or
  moved outside its original git work tree are reported as `no_repo`.
- The 24h window is fixed in v1. A multi-session day in one repo will share
  the same follow-up commit attribution across every session that ran first.
- Each git invocation has a 5-second timeout; a hung repo (e.g. NFS lock)
  falls through to `no_repo` rather than stalling the report.
$ stackunderflow context-budget
Context budget (per-session estimate)
  heuristic: len(text) // 4; per-MCP-server 200 + 50/tool

  system_prompt                     3,000 tok   (fixed)
  memory:project_CLAUDE.md            512 tok   /path/to/project/CLAUDE.md
  memory:global_CLAUDE.md             876 tok   /Users/you/.claude/CLAUDE.md
  mcp:filesystem                      400 tok   /Users/you/.claude.json
  mcp:tavily                          400 tok   /Users/you/.claude.json
  skill:anti-slop-guide               204 tok   /Users/you/.claude/skills/anti-slop-guide/SKILL.md
  ...

  total: 8,142 tokens
  cost per session: $0.0244
  estimated monthly cost: $2.44

$ stackunderflow context-budget --global --format json
$ stackunderflow context-budget --project ~/code/my-app
```

The same data is available over HTTP at `GET /api/context-budget` with
a `project=<slug>` query parameter (omit for the global budget).

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

## Plan Budget Commands

Track monthly AI spend against a known plan (Claude Pro, Claude Max, Cursor Pro,
Cursor Max, or a custom amount). Status banding tells you whether you're on track:

| pct of budget | status |
|---|---|
| `< 80%` | `ok` |
| `80% – 100%` | `warn` |
| `> 100%` | `over` |

The plan is stored in three settings keys (`plan_name`, `plan_monthly_usd`,
`plan_reset_day`) but managed through this command — `cfg set plan_name ...`
is intentionally rejected because the three keys have inter-key invariants.

The `Projected` figure is a **simple linear** extrapolation
(`used + daily_burn × days_left`). It does not weight weekends, project
ramps, or week-of-month seasonality — read it as a directional signal,
not a forecast.

The cost rollup reuses the same engine as `stackunderflow month`
(`reports.aggregate.build_report`), so the `Used` number always matches
what the dashboard's monthly spend shows.

### `stackunderflow plan show`

Print the active plan and current usage against budget.

```
Usage: stackunderflow plan show [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow plan show
Plan:          claude-pro
Budget:        $20.00 / month  (resets day 1)
Period:        2026-05-01 → 2026-05-31  (day 12 of 31)
Used:          $12.50  (62.5% of budget)
Remaining:     $7.50
Projected:     $32.29  (linear, today's burn rate)
Status:        ok

$ stackunderflow plan show --format json
{
  "plan": {"name": "claude-pro", "monthly_usd": 20.0, "reset_day": 1},
  "usage": {
    "used": 12.5,
    "budget": 20.0,
    "remaining": 7.5,
    "pct": 62.5,
    "projected_month_end": 32.29,
    "status": "ok",
    "period_start": "2026-05-01",
    "period_end": "2026-05-31",
    "days_so_far": 12,
    "days_in_period": 31
  }
}
```

If no plan is set:

```
$ stackunderflow plan show
No plan set. Run: stackunderflow plan set claude-pro
```

The same payload is exposed over HTTP at `GET /api/plan`. With no plan
configured, the route returns `{"plan": null, "usage": null}`.

---

### `stackunderflow plan set`

Set the active plan. Preset names accept an optional `--monthly-usd` to
override the listed amount (useful if you're grandfathered into an
older price); `custom` requires `--monthly-usd`.

```
Usage: stackunderflow plan set NAME [OPTIONS]
```

| Argument | Required | Description |
|---|---|---|
| `NAME` | yes | One of `claude-pro`, `claude-max`, `cursor-pro`, `cursor-max`, `custom` |

| Option | Type | Default | Description |
|---|---|---|---|
| `--monthly-usd` | FLOAT | (preset amount) | Required for `custom`; overrides the preset amount otherwise |
| `--reset-day` | INTEGER (1–31) | 1 | Day-of-month the billing window rolls over |

Preset amounts:

| Preset | Monthly USD |
|---|---|
| `claude-pro` | 20 |
| `claude-max` | 200 |
| `cursor-pro` | 20 |
| `cursor-max` | 40 |
| `custom` | `--monthly-usd` (required) |

**`--reset-day` semantics.** A reset day greater than the current month's
length clamps to the last day of the month — so `--reset-day 31` lands on
Feb 28 (or 29 in leap years), then rolls back to 31 on March.

**Examples:**

```
$ stackunderflow plan set claude-pro
  plan = claude-pro  ($20.00/month, resets day 1)

$ stackunderflow plan set claude-max --reset-day 15
  plan = claude-max  ($200.00/month, resets day 15)

$ stackunderflow plan set custom --monthly-usd 75
  plan = custom  ($75.00/month, resets day 1)

$ stackunderflow plan set claude-pro --monthly-usd 18
  plan = claude-pro  ($18.00/month, resets day 1)   # grandfathered price
```

---

### `stackunderflow plan reset`

Clear the active plan. After this, `plan show` reports "No plan set" and
`/api/plan` returns `{"plan": null, "usage": null}`.

```
Usage: stackunderflow plan reset
```

**Example:**

```
$ stackunderflow plan reset
  plan cleared
```

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
| `plan_name` | str \| null | `null` | Active plan preset name (manage via `plan set`) |
| `plan_monthly_usd` | float \| null | `null` | Monthly budget in USD (manage via `plan set`) |
| `plan_reset_day` | int | `1` | Day-of-month the budget resets (manage via `plan set`) |

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

## Cost Computation

Costs are computed from per-message token usage via the
`stackunderflow.infra.costs.compute_cost(tokens, model, provider, *, speed)`
shim, which routes through provider-specific pricers in
`stackunderflow.infra.providers/`. The Anthropic pricer recognises the
priority/fast tier flag set by the Claude adapter from
`message.usage.service_tier`:

| Field value | `Record.speed` | Pricing impact |
|---|---|---|
| `"priority"` | `"fast"` | Opus models: 6× input + 6× output rate, cache rates unchanged. Sonnet/Haiku: 1× (unchanged). |
| `"standard"` | `"standard"` | Standard rate card. |
| `"batch"` | `"standard"` | Batch tier is cheaper (not faster); we conservatively price it at standard until separate batch rates are wired in. |
| `null` / missing | `"standard"` | Pre-tier records and non-Claude adapters default here. |

Unknown model ids (which fall back to the Sonnet-3.5 rate card) are
priced at 1× even when `speed="fast"` so a misclassified record never
gets accidentally over-charged. Aggregator collectors group tokens by
`(model, speed)` so a session that mixes standard and fast records gets
each subset priced at its correct rate. The store schema does not yet
carry the speed flag — SQLite-backed stat queries
(`store/queries.get_project_stats`) report standard rates until a
follow-up migration lands.

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| non-zero | Error |

Invalid period strings (e.g. `stackunderflow report -p yesterday`) exit with code 1
and print `Unknown period`. Invalid config keys passed to `cfg set` exit with an error
message listing the valid keys.

# StackUnderflow CLI Reference

The `stackunderflow` command covers dashboard launch, usage reports, data export,
config management, session backups, and the agent-facing `memory` namespace. All
persistent state lives under `~/.stackunderflow/` (config at
`~/.stackunderflow/config.json`, session data at `~/.stackunderflow/store.db`). Every
command accepts `--help` for a quick reminder.

---

## Command Overview

```
# Dashboard
stackunderflow init [--port N] [--host H] [--no-browser] [--clear-cache] [--install-skills] [--skills-dest DIR] [--skills-force]
stackunderflow start [-p N] [-H H] [--headless] [--fresh] [--no-watcher] [--no-lock]
stackunderflow reindex
stackunderflow clear-cache [PROJECT]

# Reports  (all eight also accept --ingest / --no-auto-ingest — see "Reading freshness")
stackunderflow status [--format text|json]
stackunderflow today [--format text|json] [--project P] [--exclude P]
stackunderflow month [--format text|json] [--project P] [--exclude P]
stackunderflow report [-p PERIOD] [--format text|json] [--project P] [--exclude P] [--provider PROV]
stackunderflow export -f csv|json -o PATH [-p today|week|month|all] [--provider X] [--project P] [--exclude P] [--force]
stackunderflow optimize [-p PERIOD] [--format text|json] [--project P] [--exclude P]
stackunderflow compare [-p today|week|month|all] [--provider X] [--project P] [--format text|json]
stackunderflow yield [-p PERIOD] [--format text|json] [--project SLUG]
stackunderflow context-budget [--project DIR] [--global] [--format text|json]

# Memory — the agent-facing namespace (stable JSON envelope; see "Memory Commands")
stackunderflow memory decisions QUERY  [--project SLUG] [--since X] [--limit N] [--context-budget N] [--format text|json] [--json]
stackunderflow memory file PATH        [--project SLUG] [--since X] [--limit N] [--context-budget N] [--format text|json] [--json]
stackunderflow memory worked ACTION    [--project SLUG] [--since X] [--limit N] [--context-budget N] [--format text|json] [--json]
stackunderflow memory sessions [PATH]  [--project SLUG] [--since X] [--limit N] [--context-budget N] [--format text|json] [--json]
stackunderflow memory ask QUESTION     [--project SLUG] [--since X] [--limit N] [--context-budget N] [--format text|json] [--json]

# Discovery — lower-level self-referential queries (the `memory` namespace wraps these)
stackunderflow find-sessions-in-path PATH [--since X] [--limit N] [--provider P] [--format text|json] [--context-budget N]
stackunderflow find-sessions-touching-file FILE [--limit N] [--mode read|write|any] [--format text|json] [--context-budget N]
stackunderflow search-past-decisions QUERY [--project P] [--since X] [--limit N] [--use-embeddings] [--embed-model M] [--format text|json] [--context-budget N]
stackunderflow find-sessions-where-action-worked ACTION [--project P] [--file F] [--since X] [--limit N] [--min-confidence F] [-v] [--format text|json]
stackunderflow find-failure-modes-for-file FILE [--since X] [--limit N] [--min-confidence F] [-v] [--format text|json]
stackunderflow risk file PATH [--since X] [--format text|json]

# Auto-generated skills (mined from the local store — see docs/skills.md)
stackunderflow skills generate [--project P] [--projects A,B] [--scope project|user] [--min-occurrences N] [--kind K] [--window W] [--out DIR] [--dry-run] [--format text|json]
stackunderflow skills list [--scope project|user] [--out DIR] [--format text|json]
stackunderflow skills clean [--scope project|user] [--out DIR] [--older-than X] [--dry-run] [--yes]

# Recommendations (mined from the local store — read-only, never auto-applied)
stackunderflow recommend skills [--project P] [--threshold N] [--window-days N] [--no-cache] [--format text|json]
stackunderflow recommend mode --prompt TEXT [--current-model M] [--no-cache] [--format text|json]

# Discovery citation-feedback telemetry
stackunderflow discovery telemetry [--command C] [--session S] [--limit N] [--format text|json]
stackunderflow discovery demote-uncited [--dry-run] [--min-loads N] [--min-age-days N] [--format text|json]

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
stackunderflow plan thresholds show [--format text|json]
stackunderflow plan thresholds set N [N ...]
stackunderflow plan thresholds reset

# ETL pipeline
stackunderflow etl backfill [--force]
stackunderflow etl status [--format text|json]

# Hooks (opt-in Claude Code lifecycle hooks — see docs/hooks.md)
stackunderflow hooks install   [--scope project|user] [--dry-run] [--capture-content]
stackunderflow hooks uninstall [--scope project|user]
stackunderflow hooks status    [--scope project|user] [--format text|json]
stackunderflow hooks repair    [--scope project|user|all] [--dry-run]
stackunderflow hooks run <hook-id> [--capture-content]   # internal — invoked by Claude Code

# Backup
stackunderflow backup create [--label TEXT] [--keep N]
stackunderflow backup list
stackunderflow backup restore NAME [--dry-run]
stackunderflow backup auto [--enable|--disable]

# PR / CI ingest (opt-in — REST backfill + webhook receiver)
stackunderflow ingest github --repo OWNER/REPO [--token T] [--state all|open|closed] [--max-pages N] [--no-ci] [--format text|json]
stackunderflow ingest webhook serve [--port N] [--host H]
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
| `--no-watcher` | flag | false | Disable the ETL filesystem watcher (sets `STACKUNDERFLOW_DISABLE_WATCHER=1`); headless / debugging |
| `--no-lock` | flag | false | Skip the singleton watcher lock at `~/.stackunderflow/server.lock` (sets `STACKUNDERFLOW_DISABLE_LOCK=1`). Headless / test only — two instances running watchers against the same store race on ingest and mart refresh |

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

`--no-watcher` starts the server without the ETL filesystem watcher;
dashboard data then refreshes only on a manual `reindex` or restart.
`--no-lock` skips the singleton watcher lock — a headless / test escape
hatch, not a way to run two servers against one store (their watchers
race). Both print the same two-line banner as a normal start.

> **Network exposure.** Binding to any host other than `127.0.0.1`,
> `localhost`, or `::1` prints a yellow warning to stderr: the API has
> no authentication, so a non-loopback bind serves session data,
> tokens, and cost figures to anyone who can reach that interface.
> The server still starts — the warning does not block it.

> **Filesystem watcher.** By default `start` spawns a daemon thread
> that watches every registered adapter's source paths (for the four
> default-on adapters: `~/.claude/projects`, `~/.codex/sessions`, the
> Cursor `state.vscdb`, and the Cline task store under VS Code's
> `globalStorage`). On any change it ingests the new bytes and refreshes
> the marts. Pass `--no-watcher` (or set `STACKUNDERFLOW_DISABLE_WATCHER=1`)
> to skip the spawn and rely on the periodic-ingest path instead.

> **Restart after upgrades.** `stackunderflow start` is a long-lived
> process that loads the application code into memory once at boot.
> New releases (`pip install -U stackunderflow`) and any locally
> installed code changes only take effect after the server restarts —
> stop with Ctrl+C and re-run `stackunderflow start`. Routes added in a
> new version will return `404` until then.

---

### `stackunderflow init`

Start the dashboard. A backward-compatible alias for `start` with
slightly different flag names. With `--install-skills`, it also copies
the three shipped Claude Code skills into place before the server boots.

```
Usage: stackunderflow init [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--port` | INTEGER | from config | Server port |
| `--host` | TEXT | from config | Bind address |
| `--no-browser` | flag | false | Don't open the browser (maps to `start --headless`) |
| `--clear-cache` | flag | false | Clear disk cache first (maps to `start --fresh`) |
| `--install-skills` | flag | false | Copy the three shipped Claude Code skills (`check-prior-work`, `find-related-sessions`, `recall-past-decisions`) into the skills destination before starting. Idempotent: byte-identical files are skipped silently |
| `--skills-dest` | PATH | `~/.claude/skills/` | Destination directory for `--install-skills` |
| `--skills-force` | flag | false | With `--install-skills`, overwrite destination `SKILL.md` files that differ from the shipped copy. Default behaviour preserves local edits — a modified destination is skipped with a warning |

**Skill install.** `--install-skills` runs first, then the dashboard
starts as usual. A missing destination is created; one whose bytes
match the shipped copy is reported as already current; one that differs
is skipped with a warning unless `--skills-force` is set. See
[`docs/skills.md`](skills.md).

**Examples:**

```
$ stackunderflow init
$ stackunderflow init --port 9000 --no-browser
$ stackunderflow init --clear-cache
$ stackunderflow init --install-skills
  + installed skill: check-prior-work → /Users/you/.claude/skills/check-prior-work/SKILL.md
  + installed skill: find-related-sessions → /Users/you/.claude/skills/find-related-sessions/SKILL.md
  + installed skill: recall-past-decisions → /Users/you/.claude/skills/recall-past-decisions/SKILL.md
```

---

### `stackunderflow reindex`

Re-ingest every registered adapter source into `~/.stackunderflow/store.db`,
applying the current schema first. Ingest is incremental — each source
file is read from its stored watermark and rows land via `INSERT OR
IGNORE` — so re-running is safe and cheap. Run it after an upgrade (a
new release may add adapters or schema) or to force a catch-up when
`start` and its watcher are not running.

```
Usage: stackunderflow reindex [OPTIONS]
```

No options beyond `--help`.

**Example:**

```
$ stackunderflow reindex
Reindexing into /Users/you/.stackunderflow/store.db
Done: {'claude': 58203, 'codex': 1171, 'cursor': 1035, 'cline': 860}
```

The `Done:` line is the per-provider count of messages ingested on this
pass (`run_ingest` returns a `{provider: messages_added}` dict).

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

## Report Commands

### Reading freshness (`--ingest` / `--auto-ingest`)

The eight read-only data commands listed in this section
(`status`, `today`, `month`, `report`, `compare`, `yield`,
`optimize`, `export`) all read from the SQLite store at
`~/.stackunderflow/store.db`. That store is normally kept fresh by
the filesystem watcher that `stackunderflow start` spawns. When
`start` is not running, the store reflects whatever the last
watcher snapshot wrote — which can be hours or days stale.

Two flags control how much the read commands try to refresh the
store before they run:

| Flag | Default | Behaviour |
|---|---|---|
| `--ingest` | off | Force a fresh ingest + incremental backfill pass before the command's query. Use when `stackunderflow start` is not running and you need authoritative numbers right now. |
| `--auto-ingest / --no-auto-ingest` | `--auto-ingest` (on) | Refresh automatically when the store's newest event is older than 6 hours. A one-line `[stale data — ingesting...]` notice goes to stderr. Disable with `--no-auto-ingest` (e.g. inside scripts that don't want surprise latency). |

`--ingest` overrides `--no-auto-ingest`: an explicit force always
runs the pass. The refresh runs **synchronously** in the CLI's
lifetime — it blocks the command until the pass completes.

The refresh path is identical to the one
`stackunderflow start` runs at boot: enumerate every registered
adapter, ingest new bytes via the per-file watermark, then call
`backfill(conn, force=False)` to materialise any newly-inserted
messages into `usage_events` and refresh the mart watermarks.

When the `start` server is running in another terminal it owns the
watcher lock. The CLI ingest path runs anyway — it shares the same
ETL transactions and `INSERT OR IGNORE` dedup, so concurrent passes
do not corrupt the store, though the second pass will see most rows
as already-ingested.

---

### `stackunderflow status`

Compact one-liner showing today's and this month's cost and message counts.
Equivalent to running `today` and `month` together and condensing to a single line.

```
Usage: stackunderflow status [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

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
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

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
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

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
| `--provider` | `all\|claude\|codex\|cursor\|opencode\|pi\|copilot` | `all` | Provider filter. Accepted for forward-compatibility but not yet applied — the report aggregates every provider regardless of this value |
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

Valid period strings: `today`, `7days`, `30days`, `month`, `all`. Any other value
exits with code 1 and prints `Error: Unknown period '<value>'. Valid: today, 7days,
30days, month, all`.

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
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

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

Surface wasted spend in two blocks. **Q&A loops** lists projects where
the assistant retried the same question repeatedly. **Structural
patterns** reports waste detected from filesystem state and tool-call
history — seven detectors: bloated `CLAUDE.md`, unused MCP servers,
ghost agents, exploration-only sessions, files re-read excessively,
cache thrash, and oversized bash output.

```
Usage: stackunderflow optimize [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `30days` | Period: `today`, `7days`, `30days`, `month`, `all` |
| `--format` | `text\|json` | text | Output format |
| `--project` | TEXT | (all) | Include only this project dir name (repeatable) |
| `--exclude` | TEXT | (none) | Exclude this project dir name (repeatable) |
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

**Examples:**

```
$ stackunderflow optimize --period 7days
No waste or structural patterns found in last 7 days.

$ stackunderflow optimize --period 30days
Waste report — last 30 days

Q&A loops:
  my-api: 3 looped pair(s)
    - How do I fix the auth middleware?
    - Why does the test keep failing?

Structural patterns:
  [MEDIUM] bloated_claude_md: 2 bloated CLAUDE.md file(s)
      2 CLAUDE.md file(s) exceed 5,000 tokens and are loaded into every session's context.
      ~7,400 wasted tokens
      fix: Trim CLAUDE.md to the bare essentials — move long-form notes to project-local docs and reference them on demand.

$ stackunderflow optimize --period all --format json
```

JSON output is `{"scope": "<label>", "waste": [...], "patterns": [...]}`.
Each `patterns` entry carries `pattern_id`, `severity`, `title`,
`description`, `estimated_waste_tokens`, `suggested_fix`, and a
`details` payload. `estimated_waste_tokens` is `null` for detectors
that can't put a token figure on the waste.

---

### `stackunderflow compare`

Side-by-side per-model comparison over a time window — answers "is it worth using Opus for this kind of work?" by surfacing one-shot rate, retry rate, cache hit rate, and unit economics ($/call, $/session) per model.

```
Usage: stackunderflow compare [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | `today\|week\|month\|all` | `month` | Window over which to compare |
| `--provider` | TEXT | (all) | Filter by provider id (`claude`, `codex`, `cursor`, …) |
| `--project` | TEXT | (all) | Restrict to this project slug (repeatable) |
| `--format` | `text\|json` | text | Output format |
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

**Metrics:**

- **1-shot %** — fraction of sessions where the user asked once, the assistant answered once, and that was it (heuristic: exactly 1 user + 1 assistant message).
- **Retry rate** — `(assistant_messages / sessions) - 1`, i.e. the average number of *extra* assistant turns per session beyond the first answer.
- **Cache %** — `cache_read / (cache_read + cache_create)`, the prompt-cache hit rate; high is good (most prompt context was reused).
- **$/call** — `total_cost / calls` (per assistant message).
- **$/session** — `total_cost / sessions` (sessions are attributed to whichever model dominated the session — most assistant messages wins, ties broken alphabetically).
- **Total $** — sum of `compute_cost` over every assistant message in the window.

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
  "generated": 1746125443.117,
  "models": [
    {
      "cache_hit_rate": 0.94,
      "calls": 124,
      "cost_per_call": 0.018,
      "cost_per_session": 0.74,
      "model": "claude-opus-4-6",
      "one_shot_pct": 0.0,
      "provider": "claude",
      "retry_rate": 40.33,
      "sessions": 3,
      "total_cost": 2.21,
      "total_tokens": 4120000
    }
  ],
  "period": "week"
}
```

The payload is `json.dumps(..., indent=2, sort_keys=True)`, so keys are
alphabetical. The same data is available via `GET /api/compare` — see
[`docs/api-reference.md`](api-reference.md).

---

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
```

| Option | Type | Default | Description |
|---|---|---|---|
| `-p, --period` | TEXT | `month` | Period: `today`, `week`, `month`, `all`, `7days`, `30days` |
| `--project` | TEXT | (all) | Filter by project slug (repeatable) |
| `--format` | `text\|json` | text | Output format |
| `--ingest` | flag | off | Force a fresh ingest pass — see [Reading freshness](#reading-freshness---ingest----auto-ingest) above |
| `--auto-ingest / --no-auto-ingest` | flag | on | Auto-refresh when the store is > 6 h stale |

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

---

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

Each row's `[source]` tag is `default` (built-in), `file` (set in
`config.json`), or `env` (an environment variable currently overrides it).
The text table is sorted alphabetically; the JSON keeps declaration order.

**Examples (fresh install — every key at its built-in default):**

```
$ stackunderflow cfg ls
Settings:
  auto_browser                        True            [default]
  auto_reindex_on_ingest              True            [default]
  currency                            USD             [default]
  discovery_budget_tokens             2000            [default]
  discovery_rank_weights              0.5,0.2,0.3     [default]
  host                                127.0.0.1       [default]
  log_level                           INFO            [default]
  max_date_range_days                 30              [default]
  messages_initial_load               500             [default]
  model_aliases                       {}              [default]
  plan_alert_thresholds               [50, 75, 90]    [default]
  plan_monthly_usd                    None            [default]
  plan_name                           None            [default]
  plan_reset_day                      1               [default]
  port                                8081            [default]

$ stackunderflow cfg ls --json
{
  "port": 8081,
  "host": "127.0.0.1",
  "auto_browser": true,
  "max_date_range_days": 30,
  "messages_initial_load": 500,
  "log_level": "INFO",
  "auto_reindex_on_ingest": true,
  "currency": "USD",
  "model_aliases": {},
  "plan_name": null,
  "plan_monthly_usd": null,
  "plan_reset_day": 1,
  "plan_alert_thresholds": [
    50,
    75,
    90
  ],
  "discovery_budget_tokens": 2000,
  "discovery_rank_weights": "0.5,0.2,0.3"
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
`messages_initial_load`, `log_level`, `auto_reindex_on_ingest`, `currency`,
`discovery_budget_tokens`, `discovery_rank_weights` (full list in
[Config Keys Reference](#config-keys-reference) below). Passing an unknown key
exits with an error. Structured settings (`model_aliases`, the `plan_*` group)
are rejected here — use their dedicated subcommands (`cfg model-alias`, `plan
set`).

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

The `Projected` figure comes from the burn projector: once the period
has at least three non-zero daily samples it uses a weighted 7-day
average (recent days dominate, decay 0.85/day), falling back to a linear
running mean below that. Read it as a directional signal, not a forecast
— it does not model weekends, project ramps, or week-of-month
seasonality. The **Projection method** note under `plan show` covers the
switch in detail.

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
Projected:     $32.29  (weighted-7d, $1.04/day burn)
Days to limit: ~7 days at current burn
Status:        ok
Alert:         Crossed 50% of plan budget

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
  },
  "projection": {
    "projected_month_end_usd": 32.29,
    "projection_method": "weighted-7d",
    "daily_burn_usd": 1.04,
    "days_to_limit": 7,
    "thresholds": [50, 75, 90],
    "crossed_threshold": 50,
    "alert": "Crossed 50% of plan budget"
  }
}
```

If no plan is set:

```
$ stackunderflow plan show
No plan set. Run: stackunderflow plan set claude-pro
```

The same payload is exposed over HTTP at `GET /api/plan`. With no plan
configured, the route returns `{"plan": null, "usage": null, "projection": null}`.

**Projection method.** Burn projector v2 auto-picks `weighted-7d` once the
period has accumulated at least 3 non-zero daily samples; the weighted
average decays at 0.85/day so recent activity dominates and weekends fade.
Below that threshold (or when the last 7 days are all zero against an
otherwise non-empty period — the stale-store case), the method falls back
to `linear` (running mean over the period to date) and the response
surfaces `projection_method: "linear"` so the cause is visible.

**Alert thresholds.** Default to `50 / 75 / 90` percent of budget; override
via `stackunderflow plan thresholds set N N N`. The CLI / route / UI
surface a banner when the most-severe threshold crossed bumps up; a
separate "projected to overrun" banner supersedes the threshold notice
when the forecast exceeds the budget before the period ends.

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

Clear the active plan. Afterwards `plan show` prints `No plan set. Run:
stackunderflow plan set claude-pro`, and `plan show --format json`
returns `{"plan": null, "usage": null}`.

```
Usage: stackunderflow plan reset
```

**Example:**

```
$ stackunderflow plan reset
  plan cleared
```

---

### `stackunderflow plan thresholds {show, set, reset}`

Manage the burn-projector alert thresholds — the percentages of the plan
budget at which the CLI / `/api/plan` / UI surface a banner. Defaults to
`50 / 75 / 90`. Stored as a single settings key (`plan_alert_thresholds`)
and managed exclusively through this subcommand — `cfg set` rejects the
key with a hint pointing here.

```
Usage: stackunderflow plan thresholds {show, set, reset}
```

| Subcommand | Args | Description |
|---|---|---|
| `show` | `[--format text\|json]` | Print the active thresholds (default text) |
| `set` | `N [N ...]` | Set thresholds (positional integers in `[1, 200]`) — deduped and sorted before write |
| `reset` | (none) | Restore the built-in default `50 75 90` |

**Examples:**

```
$ stackunderflow plan thresholds show
  thresholds = 50%, 75%, 90%

$ stackunderflow plan thresholds set 60 80 95
  thresholds = 60%, 80%, 95%

$ stackunderflow plan thresholds set 90 50 50 75
  thresholds = 50%, 75%, 90%   # deduped + sorted

$ stackunderflow plan thresholds reset
  thresholds = 50%, 75%, 90%  (default)
```

---

## ETL Commands

The ETL pipeline turns raw `messages` rows into a `usage_events` fact table and
eight indexed marts (`daily_mart`, `session_mart`, `project_mart`,
`provider_day_mart`, `model_day_mart`, `tool_mart`, `command_mart`,
`message_tool_mart`). The watcher keeps everything fresh in the background; these
commands let you do a one-shot backfill and check pipeline health from the command
line.

### `stackunderflow etl backfill`

Convert all existing `messages` rows into `usage_events`, then refresh
every mart from the new watermark. The default run is incremental:
messages already converted on a prior run are skipped via the
`uniq_events_msg` UNIQUE index. When `tqdm` is installed a progress bar
is shown; otherwise the orchestrator logs a line every 10K events.

```
Usage: stackunderflow etl backfill [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--force` | flag | false | Wipe `usage_events` + `mart_watermark`, rebuild every mart from scratch, then run the normalize pass fresh — useful after a normalizer change or model-rate update |

**Example:**

```
$ stackunderflow etl backfill

Backfill complete.
  events inserted:            247,278
  events skipped (duplicate): 0
  marts refreshed:
    command          247,278 events
    daily            247,278 events
    message_tool     247,278 events
    model_day        247,278 events
    project          247,278 events
    provider_day     247,278 events
    session          247,278 events
    tool             247,278 events
  duration:                   4.812s
```

All eight marts appear here because `backfill` refreshes every
registered mart. `etl status` (below) reports only five of them.

### `stackunderflow etl status`

One-line health check for the ETL pipeline: watcher state, mart watermarks vs
the max event id, per-provider event counts, and an overall `health` enum
(`live` / `syncing` / `stale` / `error`). Same payload as `GET /api/etl/status`.
Reads the store directly, so it works without a running server.

```
Usage: stackunderflow etl status [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--format` | `text\|json` | text | Output format |

**Health rules** (evaluated worst-first — lag is `max_event_id` minus the
laggiest mart watermark, threshold 100 events):

- `error` — lag is over 100 events **and** the watcher reports `running=false` (we are behind, and nothing is catching up).
- `stale` — lag is over 100 events but the watcher is not known-stopped (catch-up may still happen).
- `syncing` — lag is above 0 but a refresh ran in the last 10 seconds (pipeline is actively catching up).
- `live` — zero lag, lag within the 100-event threshold, or no events at all.

A recently-failed backfill also forces `error` for the ~30 s its result
slot is retained, regardless of mart lag.

**Example:**

```
$ stackunderflow etl status
ETL pipeline — live (last refresh 7s ago)

  Events:        228,311 total (228,311 max id)
                 by provider: claude=225,245 codex=1,171 cursor=1,035 cline=860
                 by cost source: rate_card=226,000 estimated=2,311

  Marts:
                 daily=4,521 rows          (watermark 228,311, fresh)
                 session=1,106 rows        (watermark 228,311, fresh)
                 project=188 rows          (watermark 228,311, fresh)
                 provider_day=312 rows     (watermark 228,311, fresh)
                 model_day=482 rows        (watermark 228,311, fresh)

  Watcher:       running
                 last cycle: 12 events processed
                 lock held by PID 48213
```

`etl status` surfaces five marts — `daily`, `session`, `project`,
`provider_day`, `model_day` (the Wave 2B set). The other three registered
marts (`tool`, `command`, `message_tool`) are refreshed by `backfill` and
the watcher but not shown here.

The `last cycle:` line appears only when the watcher handle reports a
last-cycle count, and `lock held by PID N` only when a watcher lock file
is present and readable — each is a separate indented line, not a suffix
on the `Watcher:` line.

**JSON output.** The payload is `json.dumps(..., indent=2, sort_keys=True)`,
so keys are alphabetical and every nested object is fully expanded; the
inner objects are shown compact here only to keep the example short.

```
$ stackunderflow etl status --format json
{
  "current_job": null,
  "events": {
    "by_cost_source": {"estimated": 2311, "rate_card": 226000},
    "by_provider": {"claude": 225245, "cline": 860, "codex": 1171, "cursor": 1035},
    "max_id": 228311,
    "total": 228311
  },
  "health": "live",
  "lag_seconds": 0,
  "last_job": null,
  "marts": {
    "daily":        {"last_refresh_ts": "2026-05-06T08:24:11+00:00", "row_count": 4521, "watermark": 228311},
    "model_day":    {"last_refresh_ts": "2026-05-06T08:24:11+00:00", "row_count": 482,  "watermark": 228311},
    "project":      {"last_refresh_ts": "2026-05-06T08:24:11+00:00", "row_count": 188,  "watermark": 228311},
    "provider_day": {"last_refresh_ts": "2026-05-06T08:24:11+00:00", "row_count": 312,  "watermark": 228311},
    "session":      {"last_refresh_ts": "2026-05-06T08:24:11+00:00", "row_count": 1106, "watermark": 228311}
  },
  "watcher": {
    "enabled": true,
    "events_in_last_cycle": 12,
    "last_refresh_ts": "2026-05-06T08:24:11+00:00",
    "lock_held_by": 48213,
    "running": true,
    "seconds_since_refresh": 7
  }
}
```

`marts` carries the same five marts as the text view. `current_job` and
`last_job` reflect the in-process backfill slots — both `null` when no
backfill ran recently. `lag_seconds` is a misnomer kept for compatibility:
it counts *events* behind, not seconds.

When the CLI is run with no live server (the typical case for `etl status`),
`watcher.running` reports `"unknown"` — the assembler cannot introspect a
daemon thread that lives in another process.

---

## Memory Commands

`stackunderflow memory` is the agent-facing namespace — the single set of
commands a coding agent learns to ask the local store *"have I decided this
before, what broke on this file, what worked last time"*. Each subcommand
wraps an existing discovery query (see [Discovery Commands](#discovery-commands)
below); `memory` adds a uniform option set and, with `--json`, a stable
output envelope built for splicing into a context window.

### Shared options

Every `memory` subcommand carries the same six options:

| Option | Default | Description |
|---|---|---|
| `--format text\|json` | `text` | `text` for humans, `json` for agents |
| `--json` | — | Shortcut for `--format json` |
| `--project SLUG` | cwd → slug | Scope to one project. Default: the current directory's project when StackUnderflow recognises it, otherwise all projects |
| `--since X` | (all time) | Time lower bound — `7d` / `1w` / `1m` / `24h` or an ISO date/datetime |
| `--limit N` | `20` | Hard cap on the number of results |
| `--context-budget N` | env or `2000` | Token budget — results are ranked and packed greedily until ~N estimated tokens are used; `0` disables |

`--project` defaulting to the current directory is what makes these ergonomic
inside a repo: `stackunderflow memory file src/foo.py` Just Works without
naming the project.

### The JSON envelope (`--format json`)

With `--json` (or `--format json`), every subcommand emits one envelope:

```json
{
  "schema": "stackunderflow.memory/1",
  "command": "decisions",
  "query": { "text": "retry logic", "project": "-Users-you-dev-app",
             "since": null, "limit": 20 },
  "results": [ { "...": "command-specific result shape" } ],
  "result_count": 7,
  "token_estimate": 1840,
  "budget": 2000,
  "truncated": false
}
```

| Field | Meaning |
|---|---|
| `schema` | `stackunderflow.memory/<N>`; the integer bumps only on a breaking change |
| `command` | The subcommand that produced the envelope |
| `query` | The resolved inputs, echoed back (`project` is the *resolved* slug) |
| `results` | The rows — discovery result dicts; the shape is per-command |
| `result_count` | `len(results)` |
| `token_estimate` | chars/4 estimate of `results` — what the payload costs to splice in |
| `budget` | The `--context-budget` that was enforced (`0` = no budget) |
| `truncated` | `true` when the budget dropped at least one ranked row |

Contract guarantees: the envelope is **stable and versioned**, **deterministic**
(same store + same query → byte-identical JSON), and **token-bounded**. In
`--format json`, stdout is pure JSON with nothing on stderr; a non-zero exit
means stdout is an `{"error": "..."}` envelope instead. `memory file` adds a
`risk` block alongside the eight core fields; `memory ask` adds a `note`.

### `stackunderflow memory decisions`

"Did I decide something about this before?" — substring-searches `QUERY`
across past message content, newest first, each result with a snippet. Wraps
`search_past_decisions`.

```
Usage: stackunderflow memory decisions [OPTIONS] QUERY
```

**Example:**

```
$ stackunderflow memory decisions "watcher flag"
Past decisions matching 'watcher flag'  (2 session(s))
  ...

$ stackunderflow memory decisions "retry logic" --json
{
  "schema": "stackunderflow.memory/1",
  "command": "decisions",
  ...
}
```

### `stackunderflow memory file`

"What do I know about this file?" — merges three file-scoped discovery calls
into one report: known failure modes, every session that touched the file, and
a risk summary. `PATH` is resolved against the current directory. Wraps
`find_failure_modes_for_file` + `find_sessions_touching_file` + `risk file`.

```
Usage: stackunderflow memory file [OPTIONS] PATH
```

Each `results[]` row carries a `kind` of `"failure_mode"` or `"touched"`; the
envelope's extra `risk` block carries `total_sessions`, `reverted`, `failed`,
`worked`, and `recent_session_ids`. `--since` filters the failure-mode and risk
portions — the touching-sessions query has no time bound.

```
$ stackunderflow memory file stackunderflow/routes/cost.py --json
```

### `stackunderflow memory worked`

"What worked last time I tried this?" — sessions where `ACTION` was performed
and the next user turn confirmed success. Wraps
`find_sessions_where_action_worked`.

```
Usage: stackunderflow memory worked [OPTIONS] ACTION
```

### `stackunderflow memory sessions`

"Which past sessions ran here?" — with no `PATH`, lists sessions for the current
directory's project; give a directory to scope to that tree, or a file to list
only the sessions that touched it. An explicit `--project SLUG` overrides
`PATH`. Wraps `find_sessions_in_path` / `find_sessions_touching_file` — the file
form has no time bound, so `--since` applies to the path form only.

```
Usage: stackunderflow memory sessions [OPTIONS] [PATH]
```

### `stackunderflow memory ask`

A natural-language question of the local store. **v1 is a keyword search:**
`ask` runs the same query as `memory decisions` on `QUESTION` and labels the
result `ask`, with a `note` saying so. The local-LLM meta-agent — which needs a
running Ollama, so an agent caller cannot rely on it — is deliberately
deferred; until it lands, `memory decisions` with specific terms is the precise
tool.

```
Usage: stackunderflow memory ask [OPTIONS] QUESTION
```

---

## Discovery Commands

Self-referential queries over the local store: "what's happened in this project /
this file / about this decision?" — designed for a coding agent to call before
starting non-trivial work, so it can reuse prior context instead of re-deriving it.
These are the lower-level commands the `memory` namespace wraps; they
read the store directly (no running server needed), accept `--format text|json`
(the JSON shape is stable), and resolve `~` / relative paths before matching.

`--since` everywhere accepts a relative spec (`7d`, `1w`, `1m`, `24h`) or an
ISO-8601 date/datetime. An unparseable value exits with a `--since` parameter
error.

### Token budgeting (`--context-budget`)

`find-sessions-in-path`, `find-sessions-touching-file`, and `search-past-decisions`
are *budget-aware*: within the `--limit` hard cap, results are ranked
(recency + cost + relevance — weights tunable via
`STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS`) and packed greedily until roughly
`--context-budget` estimated tokens (chars/4 heuristic) are used, so an agent
calling them doesn't get an unprioritised dump into a tight context window.

| Aspect | Behaviour |
|---|---|
| Default | `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` (env) or `2000` |
| Disable | pass `--context-budget 0` — then `--limit` is the only cap |
| Text output | when rows were dropped, a tail marker reports how many more matched |
| JSON output | always carries `_budget_used_tokens` / `_budget_max_tokens`; adds `_truncated: true` + `_more_available: <count>` when rows were dropped |

### `stackunderflow find-sessions-in-path`

List sessions whose project root is `PATH` or any ancestor of `PATH` (ancestor-only
— projects rooted *below* `PATH` don't match). Useful when an agent is working in
`/a/b/c` and wants to know what's happened in the project rooted at `/a/b`.

```
Usage: stackunderflow find-sessions-in-path [OPTIONS] PATH
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--since` | TEXT | (all time) | Only sessions whose last activity is newer than this (`7d`/`1w`/`1m`/`24h`/ISO) |
| `--limit` | INTEGER | `20` | Max sessions to return (hard cap) |
| `--provider` | TEXT | (all) | Filter by provider slug (`claude`, `codex`, `cursor`, …) |
| `--context-budget` | INTEGER | env or `2000` | Token budget for the output; `0` disables |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow find-sessions-in-path "$(pwd)" --since 30d --limit 5
Sessions in path /Users/you/dev/my-app  (3 session(s))

  [claude] abc123def456…  2026-05-10T14:22:01  msgs=128  $1.4210
      -Users-you-dev-my-app  /Users/you/dev/my-app
  ...

$ stackunderflow find-sessions-in-path . --format json --limit 5 --context-budget 1500
```

### `stackunderflow find-sessions-touching-file`

List sessions where `FILE` shows up in tool calls (Read / Edit / Write /
MultiEdit / NotebookEdit, Bash redirects, …) or message text.

```
Usage: stackunderflow find-sessions-touching-file [OPTIONS] FILE
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--limit` | INTEGER | `20` | Max sessions to return (hard cap) |
| `--mode` | `read\|write\|any` | `any` | `read` = Read-style accesses; `write` = Edit/Write/MultiEdit/NotebookEdit mutations; `any` = any mention (tools or freeform) |
| `--context-budget` | INTEGER | env or `2000` | Token budget for the output; `0` disables |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow find-sessions-touching-file stackunderflow/store/queries.py --mode write
Sessions touching stackunderflow/store/queries.py  (mode=write)  (2 session(s))
  ...

$ stackunderflow find-sessions-touching-file ~/code/app/main.py --format json --limit 5
```

### `stackunderflow search-past-decisions`

Substring-search `QUERY` across past message content (and tool-call arguments);
return matching sessions, each with a short snippet for context.

```
Usage: stackunderflow search-past-decisions [OPTIONS] QUERY
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--project` | TEXT | (all) | Filter by project slug (e.g. `-Users-you-dev-foo`) |
| `--since` | TEXT | (all time) | Filter to messages newer than this (`7d`/`1w`/`1m`/`24h`/ISO) |
| `--limit` | INTEGER | `20` | Max sessions to return (hard cap) |
| `--context-budget` | INTEGER | env or `2000` | Token budget for the output; `0` disables |
| `--use-embeddings` | flag | false | Re-rank substring matches by local sentence-transformers cosine similarity (requires `pip install stackunderflow[embeddings]`) |
| `--embed-model` | TEXT | env or `all-MiniLM-L6-v2` | Override the sentence-transformers model id; ignored without `--use-embeddings` |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow search-past-decisions "watchfiles inotify" --limit 10
Past decisions matching 'watchfiles inotify'  (1 session(s))

  [claude] def456abc789…  2026-04-25T09:11:33  msgs=64  $0.8800
      -Users-you-dev-app  /Users/you/dev/app
      … "watchfiles is Rust-backed and cross-platform; raw inotify would mean a separate macOS path…"

$ stackunderflow search-past-decisions "auth middleware" --project -Users-you-dev-api --format json
```

**Semantic search (opt-in).** Plain substring matching misses phrases that
were worded differently in the transcript. Install
`pip install stackunderflow[embeddings]` (adds sentence-transformers) and
pass `--use-embeddings` to re-rank the substring-matched candidate set
by cosine similarity against the query. The substring filter still runs
first — `--use-embeddings` only re-orders the set, never widens it. JSON
output gains an `embedding_score` in `[0, 1]`; text output appends
`cos=X.XX` to each session headline. Model defaults to
`sentence-transformers/all-MiniLM-L6-v2` (90 MB, 384-dim) and is loaded
lazily on the first call; override via the `STACKUNDERFLOW_EMBED_MODEL`
env var or `--embed-model`. Per-message vectors are cached in
`discovery_embeddings` (added by migration v014) so repeat queries
against the same candidate set are a SELECT, not a recompute.

```
$ pip install 'stackunderflow[embeddings]'
$ stackunderflow search-past-decisions "watcher behind a flag" --use-embeddings
Past decisions matching 'watcher behind a flag'  (2 session(s))

  [claude] def456abc789…  2026-04-25T09:11:33  msgs=64  $0.8800  cos=0.87
      -Users-you-dev-app  /Users/you/dev/app
      … "we decided to ship the watcher behind a flag for the rollout…"

  [claude] aaa111bbb222…  2026-04-19T15:02:11  msgs=22  $0.1200  cos=0.51
      -Users-you-dev-app  /Users/you/dev/app
      … "the file watcher behind the etl service occasionally drops events…"
```

### `stackunderflow find-sessions-where-action-worked`

List sessions where `ACTION` was performed *and the next user turn confirmed it
worked* (an explicit "thanks"/"that worked", or no revert and no complaint before
the session ended). `ACTION` is matched as a case-insensitive substring against
tool calls and message text — a tool name (`Edit`), a file fragment (`cost.py`),
or a phrase (`add caching`). The positive-signal counterpart to
`find-failure-modes-for-file`. Not budget-aware — `--limit` is the only cap.

```
Usage: stackunderflow find-sessions-where-action-worked [OPTIONS] ACTION
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--project` | TEXT | (all) | Filter by project slug |
| `--file` | TEXT | (none) | Narrow to sessions that also touched this file |
| `--since` | TEXT | (all time) | Only sessions whose matching activity is newer than this (`7d`/`1w`/`1m`/`24h`/ISO) |
| `--limit` | INTEGER | `20` | Max sessions to return |
| `--min-confidence` | FLOAT | `0.5` | Minimum outcome confidence in `[0.0, 1.0]`. Explicit-phrase confirmations clear the default; lower-confidence "silence ⇒ worked" rows do not. Pass `0.0` for the legacy "anything that didn't break is a success" behaviour |
| `-v, --verbose` | flag | false | Append `outcome_confidence` to each text row (JSON always carries it) |
| `--format` | `text\|json` | text | Output format |

Each result carries the inferred `outcome` (`"worked"`), `outcome_evidence` (the
message that established it), `outcome_msg_id`, and `outcome_confidence`. The text
output prints a `→ worked: <evidence>` line under each session; with `-v` it also
shows the confidence score.

**Example:**

```
$ stackunderflow find-sessions-where-action-worked "add caching" --file stackunderflow/infra/cache.py
Sessions where 'add caching' worked  (1 session(s))

  [claude] aaa111bbb222…  2026-05-08T16:40:02  msgs=92  $1.1000
      -Users-you-dev-app  /Users/you/dev/app
      → worked: user replied "perfect, that's exactly it" two turns later
```

### `stackunderflow find-failure-modes-for-file`

List sessions where editing `FILE` led to a follow-up correction — the user
reporting it broke, the agent reverting it (`git revert` / `git reset --hard` /
`git checkout --`), or a complaint — each with the triggering message as evidence.
The negative-signal counterpart to `find-sessions-where-action-worked`. Not
budget-aware.

```
Usage: stackunderflow find-failure-modes-for-file [OPTIONS] FILE
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--since` | TEXT | (all time) | Only sessions whose edit is newer than this (`7d`/`1w`/`1m`/`24h`/ISO) |
| `--limit` | INTEGER | `20` | Max sessions to return |
| `--min-confidence` | FLOAT | `0.5` | Minimum outcome confidence in `[0.0, 1.0]`. Explicit-phrase complaints (0.8) and agent revert tool calls (0.5) clear the default; pass `0.0` to include lower-confidence inferences |
| `-v, --verbose` | flag | false | Append `outcome_confidence` to each text row (JSON always carries it) |
| `--format` | `text\|json` | text | Output format |

Each result carries `outcome` (`"failed"` or `"reverted"`), `outcome_evidence`,
`outcome_msg_id`, and `outcome_confidence`.

**Example:**

```
$ stackunderflow find-failure-modes-for-file stackunderflow/routes/cost.py --since 90d
Failure modes for stackunderflow/routes/cost.py  (1 session(s))

  [claude] ccc333ddd444…  2026-04-29T11:02:55  msgs=140  $2.0100
      -Users-you-dev-app  /Users/you/dev/app
      → reverted: agent ran `git checkout -- stackunderflow/routes/cost.py` after the test broke
```

---

## Risk Command

### `stackunderflow risk file`

Risk summary for a file before you edit it: how many distinct past
sessions that touched it ended `reverted`, `failed`, or `worked`. A
read-only aggregator over the same outcome heuristic that powers
`find-failure-modes-for-file` — no extra schema, the counts are computed
from `messages` / `sessions` rows on each call. Also surfaced, merged with
failure modes and touching sessions, by
[`stackunderflow memory file`](#stackunderflow-memory-file).

```
Usage: stackunderflow risk file [OPTIONS] PATH
```

| Argument | Required | Description |
|---|---|---|
| `PATH` | yes | File path to assess — `~` and relative paths are resolved to an absolute path before matching |

| Option | Type | Default | Description |
|---|---|---|---|
| `--since` | TEXT | (all time) | Only sessions whose activity is newer than this (`7d`/`1w`/`1m`/`24h`/ISO) |
| `--format` | `text\|json` | text | Output format |

`recent_session_ids` lists up to five of the most recent failure-mode
sessions (`reverted` ∪ `failed`) for the file — read them with
`session_query` to see what went wrong.

**Example:**

```
$ stackunderflow risk file stackunderflow/routes/cost.py --since 90d
File risk for /Users/you/dev/app/stackunderflow/routes/cost.py
  since: 90d

  total sessions touching the file: 14
  reverted:                         2
  failed:                           1
  worked:                           9

  recent failure-mode sessions:
    - ccc333dd-1a2b-4c3d-...
    - 91b2f0a1-9e8d-4f7c-...
```

JSON output is `{path, since, total_sessions, reverted, failed, worked,
recent_session_ids}`.

---

## Skills Commands

Mine the local store for project-specific workflow patterns ("always run `pytest`
after editing", "never `pkill`", a flag combo this project favours, a path the
user keeps steering edits away from) and emit Claude Code `SKILL.md` files. These
are derived from ground truth (what actually happened across your sessions) — never
from `CLAUDE.md`. Pure-regex synthesis, no network. Full background in
[`docs/skills.md`](skills.md).

**Guardrails (deliberate):** project-scoped by default — there is no
`--all-projects` mode; cross-project mining is opt-in via `--scope user` with an
explicit `--project`/`--projects` allowlist. Generated skills land under
`<project>/.claude/skills/auto-*/` (or `~/.claude/skills/` with `--scope user`) —
never into the package, never released. Idempotent (`.bak` written before any
overwrite); never clobbers a hand-authored skill.

### `stackunderflow skills generate`

```
Usage: stackunderflow skills generate [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--project` | TEXT | (cwd's project) | Project slug to mine. Default for `--scope project`: the project the current directory belongs to |
| `--projects` | TEXT | (none) | Comma-separated slugs for cross-project mining (required for `--scope user` when `--project` isn't given) |
| `--scope` | `project\|user` | `project` | `project` → `./.claude/skills/` ; `user` → `~/.claude/skills/` (global; requires explicit `--project`/`--projects`) |
| `--min-occurrences` | INTEGER (≥1) | `5` | A pattern must appear in this many distinct sessions |
| `--kind` | choice (repeatable) | (all) | Restrict to these pattern kinds: `avoids-X`, `never-touches-paths`, `canonical-test-command`, `always-runs-X-after-Y`, `uses-tool-flag-combo` |
| `--window` | TEXT | `90d` | Only consider sessions newer than this (`90d`/`1w`/ISO; `all` or empty for no bound) |
| `--out` | PATH | (per `--scope`) | Output directory |
| `--dry-run` | flag | false | Show what would be generated; write nothing |
| `--format` | `text\|json` | text | Output format |

**Examples:**

```
$ stackunderflow skills generate
Generated 2 skill(s) under /Users/you/dev/app/.claude/skills:
  [created] auto-canonical-test-command  (.../auto-canonical-test-command/SKILL.md)
  [updated] auto-never-touches-store-db  (.../auto-never-touches-store-db/SKILL.md)
    · auto-canonical-test-command: canonical-test-command, 14 sessions
    · auto-never-touches-store-db: never-touches-paths, 9 sessions

$ stackunderflow skills generate --dry-run --format json
$ stackunderflow skills generate --project -Users-you-dev-foo --min-occurrences 8 --window 60d
$ stackunderflow skills generate --scope user --projects app,api,infra
```

### `stackunderflow skills list`

```
Usage: stackunderflow skills list [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--scope` | `project\|user` | `project` | Where to look: `./.claude/skills/` or `~/.claude/skills/` |
| `--out` | PATH | (per `--scope`) | Skills directory to inspect |
| `--format` | `text\|json` | text | Output format |

Lists only the auto-generated skills (directory prefix `auto-`, frontmatter
`auto_generated: true`).

### `stackunderflow skills clean`

```
Usage: stackunderflow skills clean [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--scope` | `project\|user` | `project` | Where to clean: `./.claude/skills/` or `~/.claude/skills/` |
| `--out` | PATH | (per `--scope`) | Skills directory to clean |
| `--older-than` | TEXT | (all) | Only remove skills generated before this (`30d`/`2w`/ISO) |
| `--dry-run` | flag | false | Show what would be removed; delete nothing |
| `-y, --yes` | flag | false | Actually delete. Without this, `clean` only previews |

Only ever removes `auto-*` directories marked `auto_generated: true` — never a
hand-authored skill. Without `--yes` it prints the would-be deletions and exits.

```
$ stackunderflow skills clean --older-than 30d        # preview
$ stackunderflow skills clean --older-than 30d --yes  # delete
```

---

## Recommendation Commands

Proactive suggestions mined from your local session store. Both
subcommands are read-only — they never write a file or change a setting;
acting on a recommendation is always a separate, explicit step.

### `stackunderflow recommend skills`

List workflow patterns you've re-run by hand often enough to be worth a
skill and don't already have one for. Reads `messages` plus the skills
already on disk; each row carries an `accept_command` you can paste to
generate that skill. The proactive counterpart to `skills generate`.

```
Usage: stackunderflow recommend skills [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--project` | TEXT | (cwd's project) | Project slug to scan. Defaults to the project the current directory belongs to |
| `--threshold` | INTEGER (≥1) | `5` | A pattern must appear in this many distinct sessions |
| `--window-days` | INTEGER (≥1) | `30` | Lookback window in days |
| `--no-cache` | flag | false | Bypass the recommendation cache and re-mine |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow recommend skills
Found 2 skill recommendation(s) for -Users-you-dev-app:
  • run-pytest-after-edit  [always-runs-X-after-Y]  occurrences=18
      You ran `pytest` right after editing a source file in 18 sessions.
      accept: stackunderflow skills generate --project -Users-you-dev-app --pattern always_runs_pytest_after_edit
  • avoid-pkill  [avoids-X]  occurrences=7
      You consistently steered away from `pkill` across 7 sessions.
      accept: stackunderflow skills generate --project -Users-you-dev-app --pattern avoids_pkill
```

The `--pattern` selector in each `accept` command is forward-compatible —
`skills generate` does not consume it yet, so accepting a recommendation
today means re-running `skills generate` for the whole project. If the
current directory can't be mapped to a project, the command exits with a
usage error; pass `--project` explicitly.

### `stackunderflow recommend mode`

Pattern-match a task prompt against your own past sessions and suggest
the cheapest model whose similar sessions had the lowest cost. Heuristic
v1 — read it as a routing nudge, not a hard model picker. A `confidence`
of `0.00` means there isn't enough similar history to have an opinion.
Nothing leaves the machine.

```
Usage: stackunderflow recommend mode [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--prompt` | TEXT | (required) | The task prompt to score (quote it) |
| `--current-model` | TEXT | (none) | The model you'd otherwise route to; drives the cost delta |
| `--no-cache` | flag | false | Skip the 24 h cache and recompute from history |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow recommend mode --prompt "rename a local variable" --current-model claude-opus-4-6
Recommended model:  claude-sonnet-4-6
Current model:      claude-opus-4-6
Confidence:         0.82
Estimated savings:  $0.1840/session
Similar sessions:   23
Why:                23 similar past tasks finished on Sonnet at a median $0.02
```

---

## Discovery Telemetry Commands

A citation-feedback loop on discovery: the three budget-aware discovery commands
passively record which sessions they surface (`loaded_count`), and a later
session-detail lookup records which surfaced sessions actually get used
(`cited_count`). `cite_rate = cited_count / loaded_count` feeds the discovery
ranking so sessions agents actually use climb and uncited noise sinks. Telemetry
is local-only (session ids + counters, no transcript content) and the passive
recording is gated behind `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (default on; set to
`0` to disable).

### `stackunderflow discovery telemetry`

Show the telemetry table: loaded/cited counters + cite-rate per session, sorted
most-recently-surfaced first.

```
Usage: stackunderflow discovery telemetry [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--command` | TEXT | (all) | Filter to one discovery command (`find_sessions_in_path`, `find_sessions_touching_file`, `search_past_decisions`) |
| `--session` | TEXT | (all) | Filter to one session id |
| `--limit` | INTEGER | `50` | Max rows to show; ≤ 0 means no limit |
| `--format` | `text\|json` | text | Output format |

### `stackunderflow discovery demote-uncited`

Flag sessions surfaced N+ times over M+ days that were never cited. Demoted
sessions drop out of default discovery ranking (their cite-rate term is zeroed)
but stay reachable via direct lookup.

```
Usage: stackunderflow discovery demote-uncited [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--dry-run` | flag | false | List candidates without flagging them |
| `--min-loads` | INTEGER | `20` | Minimum times surfaced |
| `--min-age-days` | INTEGER | `7` | Minimum age (days since first surfaced) |
| `--format` | `text\|json` | text | Output format |

---

## Backup Commands

Long-form discussion of the snapshot directory layout, the rsync
`--link-dest` hard-link contract, restore semantics, and the macOS
launchd auto-backup integration lives in
[docs/backup.md](backup.md).

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

## PR & CI Ingest Commands

Two opt-in surfaces for pulling pull-request and CI data into the local
store. Neither runs unless you invoke it; tokens and secrets come from
the environment, never the database. This `ingest` command group is
separate from the `--ingest` flag on the read commands, which refreshes
session data.

### `stackunderflow ingest github`

Backfill a repository's pull requests and workflow runs over the GitHub
REST API into the local store — a one-shot catch-up for history from
before the webhook receiver was turned on.

```
Usage: stackunderflow ingest github [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--repo` | `OWNER/REPO` | (required) | The GitHub repository slug, e.g. `octocat/hello-world` |
| `--token` | TEXT | (from env) | GitHub PAT. Falls back to `$STACKUNDERFLOW_GITHUB_TOKEN`, then `$GITHUB_TOKEN`. Public repos work without one but hit the 60-requests/hour anonymous limit |
| `--state` | `all\|open\|closed` | `all` | PR state filter passed to the GitHub API |
| `--max-pages` | INTEGER (1–50) | `10` | Maximum pages of 100 to fetch per endpoint (PRs and CI) |
| `--no-ci` | flag | false | Skip the workflow-runs fetch — a quick PR-only refresh |
| `--format` | `text\|json` | text | Output format |

**Example:**

```
$ stackunderflow ingest github --repo 0bserver07/StackUnderflow --max-pages 5
Backfill complete for 0bserver07/StackUnderflow
  PRs:  inserted=42  updated=3   pages=2
  CI:   inserted=118 updated=0   pages=5
  duration: 6.30s
```

A request that exhausts the GitHub rate limit exits with an error.

### `stackunderflow ingest webhook serve`

Run a small FastAPI receiver that serves the
`/api/webhooks/{github,gitlab,ci}` endpoints on a dedicated port,
separate from the dashboard. Each endpoint verifies a signature and
returns `503` until its secret is set, so the receiver never accepts an
anonymous payload.

```
Usage: stackunderflow ingest webhook serve [OPTIONS]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `--port` | INTEGER | `8096` | Port to bind the receiver on |
| `--host` | TEXT | `127.0.0.1` | Bind address. Loopback by default; set `0.0.0.0` only when tunnelling from a public webhook URL |

The endpoints check these secrets (HMAC-SHA256 for GitHub and CI, token
comparison for GitLab):

| Endpoint | Secret env var |
|---|---|
| `/api/webhooks/github` | `STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET` |
| `/api/webhooks/gitlab` | `STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET` |
| `/api/webhooks/ci` | `STACKUNDERFLOW_CI_WEBHOOK_SECRET` |

**Example:**

```
$ STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET=... stackunderflow ingest webhook serve
  configured receivers: github
Webhook receiver listening on http://127.0.0.1:8096/api/webhooks/
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
| `auto_reindex_on_ingest` | bool | `true` | Re-aggregate dashboard data automatically after a background ingest cycle |
| `currency` | str | `USD` | Display currency for cost figures — any 3-letter ISO 4217 code |
| `model_aliases` | dict[str,str] | `{}` | Proxy-rewritten model id → canonical id (manage via `cfg model-alias`) |
| `plan_name` | str \| null | `null` | Active plan preset name (manage via `plan set`) |
| `plan_monthly_usd` | float \| null | `null` | Monthly budget in USD (manage via `plan set`) |
| `plan_reset_day` | int | `1` | Day-of-month the budget resets (manage via `plan set`) |
| `plan_alert_thresholds` | list[int] | `[50, 75, 90]` | Burn-projector alert thresholds, percent of budget (manage via `plan thresholds set`) |
| `discovery_budget_tokens` | int | `2000` | Default `--context-budget` for the `memory` namespace and the budget-aware discovery commands |
| `discovery_rank_weights` | str | `0.5,0.2,0.3` | Discovery-ranking weights `recency,cost,relevance` (malformed → default) |

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
| `auto_reindex_on_ingest` | `AUTO_REINDEX_ON_INGEST` | `AUTO_REINDEX_ON_INGEST=false stackunderflow start` |
| `currency` | `STACKUNDERFLOW_CURRENCY` | `STACKUNDERFLOW_CURRENCY=GBP stackunderflow start` |
| `discovery_budget_tokens` | `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` | `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS=4000 stackunderflow memory decisions "retry logic"` |
| `discovery_rank_weights` | `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS` | `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS=0.6,0.1,0.3 stackunderflow find-sessions-in-path .` |

Boolean env vars accept `1`, `true`, `yes`, `on` (case-insensitive) as truthy values;
anything else is treated as false.

**Resolution order (highest to lowest):**
1. Environment variable
2. Config file (`~/.stackunderflow/config.json`)
3. Built-in default

### Standalone environment variables (not config-key-backed)

These aren't in `~/.stackunderflow/config.json` — they're read directly at the call
site:

| Env var | Default | Effect |
|---|---|---|
| `STACKUNDERFLOW_DISABLE_WATCHER` | unset | Truthy = skip spawning the ETL filesystem watcher (same as `start --no-watcher`) |
| `STACKUNDERFLOW_DISABLE_LOCK` | unset | Truthy = skip the watcher single-instance lock at `~/.stackunderflow/server.lock` (same as `start --no-lock`) |
| `STACKUNDERFLOW_DISCOVERY_TELEMETRY` | on | Set to `0` / `false` to disable the passive discovery citation-feedback recording |
| `STACKUNDERFLOW_BETA_<NAME>` | unset | Truthy = enable a beta-flagged provider adapter (e.g. `STACKUNDERFLOW_BETA_GEMINI=1`) |
| `STACKUNDERFLOW_EMBED_MODEL` | `sentence-transformers/all-MiniLM-L6-v2` | sentence-transformers model id for `search-past-decisions --use-embeddings`; the `--embed-model` flag overrides it |
| `STACKUNDERFLOW_GITHUB_TOKEN` / `GITHUB_TOKEN` | unset | GitHub PAT for `ingest github`. The `--token` flag overrides; `STACKUNDERFLOW_GITHUB_TOKEN` is consulted before `GITHUB_TOKEN` |
| `STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET` | unset | HMAC-SHA256 secret for the `/api/webhooks/github` receiver; unset → that endpoint returns 503 |
| `STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET` | unset | Token-comparison secret for `/api/webhooks/gitlab`; unset → 503 |
| `STACKUNDERFLOW_CI_WEBHOOK_SECRET` | unset | HMAC-SHA256 secret for `/api/webhooks/ci`; unset → 503 |

The `discovery_rank_weights` value is `recency,cost,relevance` (three comma-separated
floats); a malformed value or the wrong component count falls back to the default
(`0.5,0.2,0.3`).

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

## Hook Commands

Opt-in integration with Claude Code's lifecycle hooks (`PostToolUse`,
`UserPromptSubmit`, `Stop`, `PreCompact`). Nothing is installed unless you run
`hooks install`. Full details — what's captured, the privacy model, the
performance characteristics, the Windows status — in [`docs/hooks.md`](hooks.md).

### `stackunderflow hooks install`

```
stackunderflow hooks install [--scope project|user] [--dry-run] [--capture-content]
```

Merges StackUnderflow's hook entries into a `settings.json` —
`<git-root>/.claude/settings.json` for `--scope project` (default), or
`~/.claude/settings.json` for `--scope user`. Idempotent and convergent: a
re-run lands on exactly the config the current flags describe (a stale entry or
a changed `--capture-content` choice is *replaced*, never duplicated). Writes
`settings.json.bak.<utc-ts>` before any mutation (not on a no-op, not under
`--dry-run`). **Never touches another tool's hooks.** `--dry-run` prints the
would-be `hooks` block and writes nothing. `--capture-content` makes the
installed hook commands store full payloads (prompt text, tool output) instead
of the conservative sanitised-metadata default.

### `stackunderflow hooks uninstall`

```
stackunderflow hooks uninstall [--scope project|user]
```

Removes only the entries StackUnderflow recognises as its own. Never deletes the
file, never touches other entries. Backs up first iff the file actually changes.

### `stackunderflow hooks status`

```
stackunderflow hooks status [--scope project|user] [--format text|json]
```

Shows which StackUnderflow hooks are installed in which scope (default: both),
whether any are *stale* (recognisably ours but not in the portable form), and how
many non-StackUnderflow hook entries the file carries.

### `stackunderflow hooks repair`

```
stackunderflow hooks repair [--scope project|user|all] [--dry-run]
```

Rewrites stale StackUnderflow hook commands (e.g. a hardcoded path left over from
a moved venv) back to the portable `stackunderflow hooks run <id>` form,
preserving `--capture-content`. Changes nothing else; backs up each file first;
`--dry-run` reports without writing. `--scope all` walks `$HOME` for every
project's `.claude/settings.json` — bounded (depth ≤ 8), never follows symlinks,
prunes `node_modules`/`.git`/`.npm`/`.cache`/`.nvm` and other heavy trees. Only
ever runs when you invoke it.

### `stackunderflow hooks run <hook-id>`

Internal — Claude Code invokes this; reads the hook payload as JSON on stdin and
records a `captured_events` row when warranted. Always exits `0` (it must never
disrupt Claude Code). Not for direct use.

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | A command-level error (`click.ClickException`) — e.g. an unknown `--period` passed to `report` or `optimize` |
| `2` | A usage error (bad parameter / `click.UsageError`) — e.g. an invalid `cfg set` key, or a `--period` outside the fixed choice list on `compare` / `yield` / `export` |

An unknown period given to `report` or `optimize` exits `1` and prints
`Error: Unknown period '<value>'. Valid: today, 7days, 30days, month, all`.
The same bad period on `compare`, `yield`, or `export` is caught by Click's
choice validation instead and exits `2`. An unknown key passed to `cfg set`
exits `2` with a message listing the valid keys.

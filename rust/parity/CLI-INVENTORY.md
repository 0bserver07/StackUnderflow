# The wave-8 CLI inventory — every verb, flag, default and output shape

**Generated, not written.** Every row is extracted from the live
`click.Command` objects by walking `cmd.params` / `cmd.commands`; no
decorator is paraphrased. Regenerate from the rust worktree root with:

```
PYTHONPATH=$PWD ../StackUnderflow/.venv/bin/python \
    rust/parity/tools/cli_inventory.py rust/parity/CLI-INVENTORY.md
```

Reference: Click **8.4.2**, CPython **3.12.13**, `stackunderflow/cli.py` at **6484** lines.

## 1. Counts

* **105** nodes — **23** groups (including the root `stackunderflow` group) and **82** leaf commands.
* **276** declared parameters. Click's own `--help` (and the root's `--version`) are added at `get_params()` time, so `--help` is not counted here — it exists on all 105 nodes.
* **1** hidden node(s): `config` — reachable, absent from every listing.

### 1.1 By port status

| status | nodes |
| --- | ---: |
| PORTED | 75 |
| PARTIAL | 1 |
| UNPORTED | 29 |
| **total** | **105** |

### 1.1a Verified against the shipped binary — DIV-346's guard

The `STATUS` map above is the file's one piece of judgment, and judgment about a moving fact goes stale silently. Every regeneration now asks `stax <path> --help` for its exit code and reports the disagreement here, so this section is a **check**, not a claim. This run: ran against `rust/target/release/stax`.

**PRESENT IN THE BINARY, CALLED UNPORTED** — the map is behind the code:

* `benchmark`
* `benchmark recommend`
* `benchmark show`
* `compare`
* `context-replay`
* `discovery`
* `discovery demote-uncited`
* `discovery telemetry`
* `etl`
* `etl backfill`
* `etl status`
* `export`
* `ingest`
* `ingest webhook`
* `ingest webhook serve`
* `optimize`
* `pricing`
* `pricing doctor`
* `worktrees`
* `worktrees attribute`
* `worktrees list`

### 1.2 By wave assignment

| wave | nodes | leaf commands | groups |
| --- | ---: | ---: | ---: |
| `W1` | 12 | 11 | 1 |
| `W7` | 6 | 4 | 2 |
| `W8-T1` | 18 | 13 | 5 |
| `W8-T2` | 31 | 24 | 7 |
| `W8-T3` | 25 | 21 | 4 |
| `W8-T4` | 10 | 7 | 3 |
| `W8-T6` | 3 | 2 | 1 |
| **total** | **105** | **82** | **23** |

| wave tag | meaning |
| --- | --- |
| `W1` | wave 1 — landed, gated every run by `rust/parity-cli.sh` |
| `W8-T1` | tranche 1 — read-only + config verbs, writers on case-local homes |
| `W8-T2` | tranche 2 — writers: rsync / launchd / network / installers (argv-differ pattern) |
| `W8-T3` | tranche 3 — the spend + reports family (`services::{aggregate,export,optimize,…}`) |
| `W8-T4` | tranche 4 — skills / docs / recommend (`skill_synth.py` 1256 ln, `embedded_docs.py` 306 ln) |
| `W7` | wave 7 — server boot and long-running processes (`start`, `init`, webhook serve) |

An RS item id marked `*` did **not** exist in `rust/TASKS-RS.md` when this inventory was first generated — building the inventory is what found the gap. See §4.

## 2. The master table

| # | path | kind | status | wave | RS item | params | summary |
| ---: | --- | --- | --- | --- | --- | ---: | --- |
| 0 | `(root)` | group | PARTIAL | `W8-T1` | — | 1 | StackUnderflow — a local-first knowledge base for your AI coding sessions. |
| 1 | `analyze` | group | UNPORTED | `W8-T2` | RS-8-106* | 0 | Per-session static-analysis pass — complexity / lint / type-completeness deltas. |
| 2 | `analyze backfill` | command | UNPORTED | `W8-T2` | RS-8-107* | 4 | Analyze every recent session lacking ``static_analysis_findings`` rows. |
| 3 | `analyze quality` | command | UNPORTED | `W8-T3` | RS-8-028 | 4 | Grade session quality using a local Ollama model. |
| 4 | `analyze session` | command | UNPORTED | `W8-T3` | RS-8-029 | 3 | Run analyzers on every file SESSION_ID touched; persist findings. |
| 5 | `backup` | group | PORTED | `W8-T1` | RS-8-090* | 0 | Back up and restore session data from every registered coding agent. |
| 6 | `backup auto` | command | PORTED | `W8-T2` | RS-8-095*, RS-8-080 | 1 | Set up or remove daily automatic backups via launchd (macOS) or cron. |
| 7 | `backup create` | command | PORTED | `W8-T2` | RS-8-093* | 3 | Create an incremental backup of every agent's session data. |
| 8 | `backup list` | command | PORTED | `W8-T1` | RS-8-091* | 0 | List existing backups. |
| 9 | `backup restore` | command | PORTED | `W8-T2` | RS-8-094* | 2 | Restore ~/.claude/ from a backup. |
| 10 | `backup verify` | command | PORTED | `W8-T1` | RS-8-092* | 1 | Verify a backup contains all critical artifacts. |
| 11 | `benchmark` | group | UNPORTED | `W8-T3` | RS-8-014 | 0 | Which model wins for the kind of work you actually do. |
| 12 | `benchmark recommend` | command | UNPORTED | `W8-T3` | RS-8-030, RS-5-004 | 7 | Outcome-aware model pick for a described task. |
| 13 | `benchmark show` | command | UNPORTED | `W8-T3` | RS-8-031, RS-5-004 | 6 | Leaderboard + per-stratum honesty for the current scope. |
| 14 | `cfg` | group | PORTED | `W8-T1` | RS-8-015 | 0 | View or change persistent settings. |
| 15 | `cfg ls` | command | PORTED | `W8-T1` | RS-8-032 | 1 | Show all settings with their sources. |
| 16 | `cfg model-alias` | group | PORTED | `W8-T1` | RS-8-016 | 0 | Manage model aliases (proxy → canonical model id). |
| 17 | `cfg model-alias ls` | command | PORTED | `W8-T1` | RS-8-033 | 1 | List all configured model aliases. |
| 18 | `cfg model-alias rm` | command | PORTED | `W8-T1` | RS-8-034 | 1 | Remove SOURCE from the alias map. |
| 19 | `cfg model-alias set` | command | PORTED | `W8-T1` | RS-8-035 | 2 | Map SOURCE (proxy id) → TARGET (canonical id) for cost lookup. |
| 20 | `cfg rm` | command | PORTED | `W8-T1` | RS-8-036 | 1 | Remove KEY from the config file. |
| 21 | `cfg set` | command | PORTED | `W8-T1` | RS-8-037 | 2 | Write KEY=VALUE to the config file. |
| 22 | `clear-cache` | command | PORTED | `W8-T1` | RS-8-038 | 1 | Clear cached data. |
| 23 | `compare` | command | UNPORTED | `W8-T3` | RS-8-039 | 6 | Compare per-model metrics side-by-side over a window. |
| 24 | `config` | group · hidden | PORTED | `W8-T1` | RS-8-017 | 0 |  |
| 25 | `config set` | command | PORTED | `W8-T1` | RS-8-040 | 2 |  |
| 26 | `config show` | command | PORTED | `W8-T1` | RS-8-041 | 1 |  |
| 27 | `config unset` | command | PORTED | `W8-T1` | RS-8-042 | 1 |  |
| 28 | `context-budget` | command | PORTED | `W8-T3` | RS-8-043 | 3 | Estimate the per-session context tax (system prompt + MCP + skills + memory). |
| 29 | `context-replay` | command | UNPORTED | `W8-T3` | RS-8-044 | 7 | Reconstruct what the model "saw" in SESSION_ID up to a --at seq. |
| 30 | `discovery` | group | UNPORTED | `W8-T2` | RS-8-018 | 0 | Inspect / maintain the discovery citation-feedback telemetry. |
| 31 | `discovery demote-uncited` | command | UNPORTED | `W8-T2` | RS-8-045 | 4 | Flag sessions surfaced N+ times over M+ days that were never cited. |
| 32 | `discovery telemetry` | command | UNPORTED | `W8-T2` | RS-8-046 | 4 | Show discovery telemetry: loaded/cited counters + cite-rate per session. |
| 33 | `docs` | group | PORTED | `W8-T4` | RS-8-019 | 0 | Read StackUnderflow's own docs, offline from the installed package. |
| 34 | `docs list` | command | PORTED | `W8-T4` | RS-8-047, RS-8-002 | 2 | List available documentation topics. |
| 35 | `docs show` | command | PORTED | `W8-T4` | RS-8-048, RS-8-002 | 2 | Print an embedded documentation topic. |
| 36 | `doctor` | command | PORTED | `W8-T6` | RS-8-049 | 2 | Read-only health + delivery check of the local store. |
| 37 | `etl` | group | UNPORTED | `W8-T2` | RS-8-103* | 0 | Run the ETL pipeline (raw messages → events → marts). |
| 38 | `etl backfill` | command | UNPORTED | `W8-T2` | RS-8-104* | 1 | Convert all existing messages into usage_events, then refresh marts. |
| 39 | `etl status` | command | UNPORTED | `W8-T2` | RS-8-105* | 1 | Show ETL pipeline health: watcher, marts, events, lag. |
| 40 | `export` | command | UNPORTED | `W8-T3` | RS-8-050, RS-5-005 | 9 | Export aggregated usage data to a CSV or JSON file. |
| 41 | `find-failure-modes-for-file` | command | PORTED | `W1` | RS-1-010 | 6 | List sessions where editing FILE led to a follow-up correction. |
| 42 | `find-sessions-in-path` | command | PORTED | `W1` | RS-1-011 | 6 | List sessions whose project root is PATH or any ancestor of PATH. |
| 43 | `find-sessions-touching-file` | command | PORTED | `W1` | RS-1-012 | 5 | List sessions where FILE shows up in tool calls or message text. |
| 44 | `find-sessions-where-action-worked` | command | PORTED | `W1` | RS-1-013 | 8 | List sessions where ACTION was performed and the next user turn confirmed it worked. |
| 45 | `guide` | group | PORTED | `W8-T2` | RS-8-020 | 0 | Manage the StackUnderflow agent-discovery snippet in CLAUDE.md / AGENTS.md. |
| 46 | `guide install` | command | PORTED | `W8-T2` | RS-8-051, RS-8-001 | 2 | Write the agent-discovery snippet into the instruction file(s) (idempotent, backs up first). |
| 47 | `guide status` | command | PORTED | `W8-T2` | RS-8-052 | 2 | Show where the StackUnderflow guide snippet is installed. |
| 48 | `guide uninstall` | command | PORTED | `W8-T2` | RS-8-053 | 1 | Remove the StackUnderflow guide snippet (only our marked block; never the file). |
| 49 | `hooks` | group | PORTED | `W8-T2` | RS-8-021 | 0 | Manage opt-in Claude Code lifecycle hooks (hybrid capture). |
| 50 | `hooks install` | command | PORTED | `W8-T2` | RS-8-054, RS-8-004 | 4 | Register the StackUnderflow hooks in a settings.json (idempotent, backs up first). |
| 51 | `hooks repair` | command | PORTED | `W8-T2` | RS-8-055, RS-8-005 | 2 | Rewrite stale StackUnderflow hook commands to the portable form (changes nothing else). |
| 52 | `hooks run` | command | PORTED | `W8-T2` | RS-8-056 | 2 | Internal — invoked by Claude Code. |
| 53 | `hooks status` | command | PORTED | `W8-T2` | RS-8-057 | 2 | Show which StackUnderflow hooks are installed, where, and whether any are stale. |
| 54 | `hooks uninstall` | command | PORTED | `W8-T2` | RS-8-058 | 1 | Remove the StackUnderflow hooks (only ours; never the file or other tools' hooks). |
| 55 | `import` | command | UNPORTED | `W8-T2` | RS-8-101* | 2 | Import external agent history via a user-supplied export command. |
| 56 | `ingest` | group | UNPORTED | `W7` | RS-8-110* | 0 | Pull PR / CI data into the local store (REST backfill + webhook receiver). |
| 57 | `ingest github` | command | UNPORTED | `W7` | RS-8-111*, RS-5-020 | 6 | Backfill GitHub PRs + workflow runs for REPO into the local store. |
| 58 | `ingest webhook` | group | UNPORTED | `W7` | RS-8-112* | 0 | Run the opt-in webhook receiver (PR + CI events). |
| 59 | `ingest webhook serve` | command | UNPORTED | `W7` | RS-8-113* | 2 | Serve the /api/webhooks/* endpoints on a dedicated port. |
| 60 | `init` | command | PORTED | `W7` | RS-8-059 | 7 | Start the dashboard (alias for ``start``). |
| 61 | `memory` | group | PORTED | `W1` | RS-1-001 | 0 | Ask the local store what past sessions already know. |
| 62 | `memory ask` | command | PORTED | `W1` | RS-1-014 | 7 | Ask a natural-language question of the local store. |
| 63 | `memory decisions` | command | PORTED | `W1` | RS-1-001 | 7 | Search past decisions — "did I decide something about this before?" |
| 64 | `memory embed` | command | UNPORTED | `W8-T2` | RS-8-088* | 1 | Backfill vector embeddings for your existing indexed messages. |
| 65 | `memory file` | command | PORTED | `W1` | RS-1-001 | 7 | Everything known about a file — "what do I know about this file?" |
| 66 | `memory sessions` | command | PORTED | `W1` | RS-1-001 | 7 | List past sessions that touched here — "which sessions ran here?" |
| 67 | `memory worked` | command | PORTED | `W1` | RS-1-001 | 7 | Find where an action worked — "what worked last time I tried this?" |
| 68 | `month` | command | PORTED | `W8-T3` | RS-8-060, RS-5-002 | 5 | This month's usage. |
| 69 | `optimize` | command | UNPORTED | `W8-T3` | RS-8-061, RS-5-007 | 6 | Find wasted spend: looped Q&A pairs plus seven structural waste patterns. |
| 70 | `plan` | group | PORTED | `W8-T3` | RS-8-022 | 0 | Manage and inspect a monthly plan budget (Claude Pro, Cursor Pro, custom). |
| 71 | `plan reset` | command | PORTED | `W8-T3` | RS-8-062 | 0 | Clear the active plan. |
| 72 | `plan set` | command | PORTED | `W8-T3` | RS-8-063 | 3 | Set the active plan. |
| 73 | `plan show` | command | PORTED | `W8-T3` | RS-8-064 | 1 | Show the active plan, current usage against budget, and burn projection. |
| 74 | `plan thresholds` | group | PORTED | `W8-T3` | RS-8-023 | 0 | Configure burn-projector alert thresholds (default 50% / 75% / 90%). |
| 75 | `plan thresholds reset` | command | PORTED | `W8-T3` | RS-8-065 | 0 | Restore the default thresholds (50% / 75% / 90%). |
| 76 | `plan thresholds set` | command | PORTED | `W8-T3` | RS-8-066 | 1 | Set the alert thresholds (positional integers in [1, 200]). |
| 77 | `plan thresholds show` | command | PORTED | `W8-T3` | RS-8-067 | 1 | Show the active alert thresholds. |
| 78 | `pricing` | group | UNPORTED | `W8-T2` | RS-8-108* | 0 | Inspect model pricing health (read-only). |
| 79 | `pricing doctor` | command | UNPORTED | `W8-T2` | RS-8-109* | 4 | Report pricing health: unpriced models, stale rates, unknown cost rows. |
| 80 | `recommend` | group | PORTED | `W8-T4` | RS-8-024 | 0 | Proactive recommendations mined from your local session store. |
| 81 | `recommend mode` | command | PORTED | `W8-T4` | RS-8-068 | 4 | Recommend the cheapest model that fits this task. |
| 82 | `recommend skills` | command | PORTED | `W8-T4` | RS-8-069, RS-8-012 | 5 | List patterns you've manually re-run that could become auto-skills. |
| 83 | `reindex` | command | UNPORTED | `W8-T2` | RS-8-102* | 0 | Rebuild the session store from scratch. |
| 84 | `report` | command | PORTED | `W8-T3` | RS-8-070, RS-5-002 | 7 | Dashboard-style summary over a date range. |
| 85 | `resume` | command | PORTED | `W1` | RS-1-002 | 4 | Session/resume ids for every coding agent under PATH (default: cwd). |
| 86 | `risk` | group | PORTED | `W8-T6` | RS-8-025 | 0 | Surface "this file has caused N reverts in M days" before editing it. |
| 87 | `risk file` | command | PORTED | `W8-T6` | RS-8-071, RS-8-011 | 3 | Risk summary for PATH: how many sessions reverted / failed / worked. |
| 88 | `search-past-decisions` | command | PORTED | `W1` | RS-1-020 | 8 | Substring-search QUERY across past message content; return matching sessions. |
| 89 | `skills` | group | PORTED | `W8-T4` | RS-8-026 | 0 | Generate / list / clean project-specific Claude Code skills. |
| 90 | `skills clean` | command | PORTED | `W8-T4` | RS-8-072, RS-8-013 | 5 | Remove auto-generated skills (never touches hand-authored ones). |
| 91 | `skills generate` | command | PORTED | `W8-T4` | RS-8-073, RS-8-013 | 9 | Mine session patterns and emit auto-generated SKILL.md files. |
| 92 | `skills list` | command | PORTED | `W8-T4` | RS-8-074, RS-8-013 | 3 | List the auto-generated skills present in the skills directory. |
| 93 | `start` | command | PORTED | `W7` | RS-8-075 | 7 | Launch the StackUnderflow dashboard. |
| 94 | `status` | command | PORTED | `W8-T1` | RS-8-089* | 3 | Compact one-liner: today + month cost and message counts. |
| 95 | `sync` | group | PORTED | `W8-T2` | RS-8-096* | 0 | Encrypted, bring-your-own-bucket backup of your analytics aggregates (opt-in). |
| 96 | `sync init` | command | PORTED | `W8-T2` | RS-8-097* | 3 | Generate this device's encryption key and record the bucket destination. |
| 97 | `sync pull` | command | PORTED | `W8-T2` | RS-8-099* | 1 | Fetch and merge every OTHER device's encrypted aggregates from your bucket. |
| 98 | `sync push` | command | PORTED | `W8-T2` | RS-8-098* | 0 | Encrypt and upload changed aggregate shards to your bucket. |
| 99 | `sync status` | command | PORTED | `W8-T2` | RS-8-100* | 1 | Show sync configuration and how many shards are pending upload (local only). |
| 100 | `today` | command | PORTED | `W8-T3` | RS-8-076, RS-5-002 | 5 | Today's usage. |
| 101 | `worktrees` | group | UNPORTED | `W8-T3` | RS-8-027 | 0 | Inspect git worktrees: owner project, cost, prune safety (read-only). |
| 102 | `worktrees attribute` | command | UNPORTED | `W8-T3` | RS-8-077 | 0 | Attribute worktree session fragments to their parent projects. |
| 103 | `worktrees list` | command | UNPORTED | `W8-T3` | RS-8-078 | 2 | List known worktrees with a verdict: ACTIVE, MERGED_SAFE_TO_PRUNE, or HAS_UNIQUE_WORK. |
| 104 | `yield` | command | PORTED | `W8-T3` | RS-8-079 | 5 | Yield analysis: productive vs reverted vs abandoned sessions. |

## 3. Per-node parameter detail

Columns: the literal option strings (`secondary_opts` after the `\|` for `--x/--no-x` pairs), the parameter kind, Click's resolved type, the `default` exactly as `repr()` prints it, the modifiers Click records (`required` / `nargs` / `multiple` / `flag` / `hidden` / a `dest` that differs from the option spelling), and the verbatim `help` string.

### `(root)` — PARTIAL · W8-T1 · —

> StackUnderflow — a local-first knowledge base for your AI coding sessions.

Subcommands: `analyze`, `backup`, `benchmark`, `cfg`, `clear-cache`, `compare`, `config`, `context-budget`, `context-replay`, `discovery`, `docs`, `doctor`, `etl`, `export`, `find-failure-modes-for-file`, `find-sessions-in-path`, `find-sessions-touching-file`, `find-sessions-where-action-worked`, `guide`, `hooks`, `import`, `ingest`, `init`, `memory`, `month`, `optimize`, `plan`, `pricing`, `recommend`, `reindex`, `report`, `resume`, `risk`, `search-past-decisions`, `skills`, `start`, `status`, `sync`, `today`, `worktrees`, `yield`

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--version` | opt | boolean | `False` | flag | Show the version and exit. |

### `analyze` — UNPORTED · W8-T2 · RS-8-106*

> Per-session static-analysis pass — complexity / lint / type-completeness deltas.
> 
>     Reconstructs each session's pre/post file states (Playback v2),
>     runs per-language analyzers (Python / TypeScript / Go), and writes
>     findings to ``static_analysis_findings``. The results power
>     outcome attribution v2 ("session X reduced complexity by 20%")
>     and the comparative benchmark surface.
> 
>     Optional dependencies (``pip install 'stackunderflow[analysis]'``
>     for ``radon`` + ``mypy``; ``tsc`` / ``eslint`` / ``go`` /
>     ``gocyclo`` need to be on PATH for those languages). Missing
>     tools produce a warning per language and skip cleanly — never
>     a hard failure.

Subcommands: `backfill`, `quality`, `session`

*No declared parameters.*

### `analyze backfill` — UNPORTED · W8-T2 · RS-8-107*

> Analyze every recent session lacking ``static_analysis_findings`` rows.
> 
>     Idempotent — sessions that already have findings are skipped
>     (the candidate query filters them out via
>     ``NOT EXISTS (SELECT 1 FROM static_analysis_findings ...)``).
>     Each worker opens its own connection so the analyzers can run in
>     parallel without sqlite cross-thread issues.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--since` | opt | text | `'30d'` | — | Only sessions whose last activity is newer than this. '7d', '1w', '1m', '24h', or an ISO date. |
| `-N / --limit` | opt | int range(1..inf) | `None` | dest=limit | Cap on candidates analyzed (default: no cap). |
| `--concurrency` | opt | int range(1..16) | `None` | — | Worker count (default: min(4, cpu_count); analyzers fork shell processes). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `analyze quality` — UNPORTED · W8-T3 · RS-8-028

> Grade session quality using a local Ollama model.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `SESSION_ID` | arg | text | `Sentinel.UNSET` | — |  |
| `--all` | opt | boolean | `False` | flag, dest=all_flag | Grade all sessions that have not been graded yet. |
| `--force` | opt | boolean | `False` | flag | Force re-grading even if cached grade exists. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `analyze session` — UNPORTED · W8-T3 · RS-8-029

> Run analyzers on every file SESSION_ID touched; persist findings.
> 
>     Idempotent — re-running overwrites prior rows for the same
>     (session, file, metric). The session's pre/post snapshots come
>     from Playback v2; analyzer subprocess calls are time-capped at
>     60s per file.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `SESSION_ID` | arg | text | `Sentinel.UNSET` | required |  |
| `--language` | opt | choice[python, typescript, go] | `Sentinel.UNSET` | multiple, dest=languages | Restrict to these languages (repeatable). Default: all supported. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `backup` — PORTED · W8-T1 · RS-8-090*

> Back up and restore session data from every registered coding agent.

Subcommands: `auto`, `create`, `list`, `restore`, `verify`

*No declared parameters.*

### `backup auto` — PORTED · W8-T2 · RS-8-095*, RS-8-080

> Set up or remove daily automatic backups via launchd (macOS) or cron.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--enable \\| --disable` | opt | boolean | `True` | flag | Enable or disable daily backups |

### `backup create` — PORTED · W8-T2 · RS-8-093*

> Create an incremental backup of every agent's session data.
> 
>     ``~/.claude`` (sessions, file history, plans, tasks, todos, settings,
>     shell snapshots, prompt history) mirrors at the backup root, exactly as
>     before, so existing restores keep working. Every OTHER registered
>     adapter's source roots — self-declared by each adapter via
>     ``source_roots()`` / ``watch_paths()``, never listed here — copy under
>     ``sources/<adapter>/`` with a ``sources/manifest.json`` mapping each
>     subdir back to its original absolute path. Excludes debug logs and
>     plugin binaries to save space.
> 
>     Uses hard links for efficiency — unchanged files cost zero disk. Files
>     that vanish or partly copy because an agent is writing to the tree right
>     now (rsync 24 / 23) are reported, not fatal — a live machine must still be
>     able to finish a backup.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--label` | opt | text | `None` | — | Optional label for the backup |
| `--keep` | opt | int range(1..inf) | `10` | — | Max backups to retain (oldest pruned) |
| `--to` | opt | text | `None` | dest=to_url | Also replicate the finished backup to ssh://[user@]host[:port]/abs/path. One-way whole-artifact copy — for peer sync of aggregates use `stackunderflow sync` instead. |

### `backup list` — PORTED · W8-T1 · RS-8-091*

> List existing backups.

*No declared parameters.*

### `backup restore` — PORTED · W8-T2 · RS-8-094*

> Restore ~/.claude/ from a backup.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `NAME` | arg | text | `Sentinel.UNSET` | required |  |
| `--dry-run` | opt | boolean | `False` | flag | Show what would be restored without doing it |

### `backup verify` — PORTED · W8-T1 · RS-8-092*

> Verify a backup contains all critical artifacts.
> 
>     Checks that the backup holds every file needed for a full restore —
>     store.db plus the search / Q&A / tags sidecars. The SQLite store alone is
>     not the complete source of truth, so a store-only backup silently loses
>     search, Q&A, and tags. Exits non-zero if the backup is missing or
>     incomplete, so wrapper scripts can detect it.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--name` | opt | text | `None` | — | Backup to verify (default: latest) |

### `benchmark` — UNPORTED · W8-T3 · RS-8-014

> Which model wins for the kind of work you actually do.
> 
>     An observational benchmark over your own history — a natural experiment you
>     already ran, not live replay. Every verdict carries n, coverage, confidence
>     intervals and a ``confidence`` label, and says "insufficient evidence"
>     rather than guess. Run any subcommand with ``--json`` for the stable,
>     token-bounded agent-output envelope.

Subcommands: `recommend`, `show`

*No declared parameters.*

### `benchmark recommend` — UNPORTED · W8-T3 · RS-8-030, RS-5-004

> Outcome-aware model pick for a described task.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--intent` | opt | text | `Sentinel.UNSET` | required | Task intent: build/fix/explore/refactor/test/ops. |
| `--size` | opt | text | `None` | — | Task size band: tiny/small/med/large. |
| `--language` | opt | text | `None` | — | Dominant language hint (e.g. python). |
| `--project` | opt | text | `None` | — | Project slug/path to scope to. |
| `--context-budget` | opt | integer | `None` | — | Token budget for --json output. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |

### `benchmark show` — UNPORTED · W8-T3 · RS-8-031, RS-5-004

> Leaderboard + per-stratum honesty for the current scope.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--period` | opt | text | `'all'` | — | today \| week \| month \| all |
| `--project` | opt | text | `None` | — | Project slug/path to scope to. Default: whole store. |
| `--intent` | opt | text | `None` | — | Filter to one intent stratum (build/fix/explore/refactor/test/ops). |
| `--context-budget` | opt | integer | `None` | — | Token budget for --json output (strata are packed to fit). |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |

### `cfg` — PORTED · W8-T1 · RS-8-015

> View or change persistent settings.

Subcommands: `ls`, `model-alias`, `rm`, `set`

*No declared parameters.*

### `cfg ls` — PORTED · W8-T1 · RS-8-032

> Show all settings with their sources.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json | JSON output |

### `cfg model-alias` — PORTED · W8-T1 · RS-8-016

> Manage model aliases (proxy → canonical model id).

Subcommands: `ls`, `rm`, `set`

*No declared parameters.*

### `cfg model-alias ls` — PORTED · W8-T1 · RS-8-033

> List all configured model aliases.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json | JSON output |

### `cfg model-alias rm` — PORTED · W8-T1 · RS-8-034

> Remove SOURCE from the alias map.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `SOURCE` | arg | text | `Sentinel.UNSET` | required |  |

### `cfg model-alias set` — PORTED · W8-T1 · RS-8-035

> Map SOURCE (proxy id) → TARGET (canonical id) for cost lookup.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `SOURCE` | arg | text | `Sentinel.UNSET` | required |  |
| `TARGET` | arg | text | `Sentinel.UNSET` | required |  |

### `cfg rm` — PORTED · W8-T1 · RS-8-036

> Remove KEY from the config file.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `KEY` | arg | text | `Sentinel.UNSET` | required |  |

### `cfg set` — PORTED · W8-T1 · RS-8-037

> Write KEY=VALUE to the config file.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `KEY` | arg | text | `Sentinel.UNSET` | required |  |
| `VALUE` | arg | text | `Sentinel.UNSET` | required |  |

### `clear-cache` — PORTED · W8-T1 · RS-8-038

> Clear cached data.  Use ``start --fresh`` for a clean boot.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PROJECT` | arg | text | `Sentinel.UNSET` | — |  |

### `compare` — UNPORTED · W8-T3 · RS-8-039

> Compare per-model metrics side-by-side over a window.
> 
>     Renders one row per model with sessions, calls, one-shot %, retry
>     rate, cache hit %, $/call, $/session, and total $.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-p / --period` | opt | choice[today, week, month, all] | `'month'` | dest=period | Window over which to compare (default: month). |
| `--provider` | opt | text | `None` | — | Filter by provider id (e.g. claude, codex, cursor). |
| `--project` | opt | text | `Sentinel.UNSET` | multiple | Restrict to this project slug (repeatable). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `config` — PORTED · W8-T1 · RS-8-017

Subcommands: `set`, `show`, `unset`

*No declared parameters.*

### `config set` — PORTED · W8-T1 · RS-8-040

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `KEY` | arg | text | `Sentinel.UNSET` | required |  |
| `VALUE` | arg | text | `Sentinel.UNSET` | required |  |

### `config show` — PORTED · W8-T1 · RS-8-041

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json |  |

### `config unset` — PORTED · W8-T1 · RS-8-042

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `KEY` | arg | text | `Sentinel.UNSET` | required |  |

### `context-budget` — PORTED · W8-T3 · RS-8-043

> Estimate the per-session context tax (system prompt + MCP + skills + memory).
> 
>     Inspects the visible config files (CLAUDE.md, ~/.claude.json mcpServers,
>     ~/.claude/skills/, agents) and produces a token / cost estimate. The
>     ``len(text) // 4`` heuristic is approximate — useful for spotting bloat,
>     not for billing.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--project` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | dest=project_dir | Project directory (default: cwd) |
| `--global` | opt | boolean | `False` | flag, dest=use_global | Estimate the global budget only (~/.claude); ignore project files. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format |

### `context-replay` — UNPORTED · W8-T3 · RS-8-044

> Reconstruct what the model "saw" in SESSION_ID up to a --at seq.
> 
>     Returns the ordered message sequence (role, preview, tool calls, per-turn
>     token estimate) with a running token total, so you can watch the context
>     grow. Read-only and advisory: an unknown session yields an empty result,
>     never an error. MVP semantics = the session's message sequence up to --at
>     (harness-side context eviction is a future refinement).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `SESSION_ID` | arg | text | `Sentinel.UNSET` | required |  |
| `--at` | opt | integer | `None` | dest=at_seq | seq cutoff (inclusive). Omit for the whole session's context. |
| `--project` | opt | text | `None` | — | Project slug to fence to. A session in another project is treated as out-of-scope (empty-but-valid). |
| `--limit` | opt | integer | `100` | — | Cap on the number of events returned (earliest first, in seq order). |
| `--context-budget` | opt | integer | `None` | — | Token budget for --json results: events are kept in order until ~this many estimated tokens are used. Pass 0 to disable. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |

### `discovery` — UNPORTED · W8-T2 · RS-8-018

> Inspect / maintain the discovery citation-feedback telemetry.

Subcommands: `demote-uncited`, `telemetry`

*No declared parameters.*

### `discovery demote-uncited` — UNPORTED · W8-T2 · RS-8-045

> Flag sessions surfaced N+ times over M+ days that were never cited.
> 
>     Demoted sessions drop out of default discovery ranking (their
>     cite-rate ranking term is zeroed) but stay reachable via direct
>     lookup. ``--dry-run`` reports the candidates without touching them.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--dry-run` | opt | boolean | `False` | flag | List candidates without flagging them. |
| `--min-loads` | opt | integer | `20` | — | Minimum times surfaced. |
| `--min-age-days` | opt | integer | `7` | — | Minimum age (days since first surfaced). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `discovery telemetry` — UNPORTED · W8-T2 · RS-8-046

> Show discovery telemetry: loaded/cited counters + cite-rate per session.
> 
>     ``cite_rate`` = cited_count / loaded_count (0.0 if never loaded).
>     Rows sorted by most-recently-surfaced first.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--command` | opt | text | `None` | dest=command_filter | Filter to one discovery command (find_sessions_in_path \| find_sessions_touching_file \| search_past_decisions). |
| `--session` | opt | text | `None` | dest=session_filter | Filter to one session id. |
| `--limit` | opt | integer | `50` | — | Max rows to show. <= 0 means no limit. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `docs` — PORTED · W8-T4 · RS-8-019

> Read StackUnderflow's own docs, offline from the installed package.

Subcommands: `list`, `show`

*No declared parameters.*

### `docs list` — PORTED · W8-T4 · RS-8-047, RS-8-002

> List available documentation topics.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--audience` | opt | text | `None` | — | Filter to pages for this audience (all, agent, user). |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit the topic list as JSON. |

### `docs show` — PORTED · W8-T4 · RS-8-048, RS-8-002

> Print an embedded documentation topic.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `TOPIC` | arg | text | `Sentinel.UNSET` | required |  |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit {slug, title, audience, summary, body} as JSON. |

### `doctor` — PORTED · W8-T6 · RS-8-049

> Read-only health + delivery check of the local store.
> 
>     Health: SQLite integrity + foreign-key checks plus watermark/orphan
>     sanity, opening the store read-only (never migrates or writes).
> 
>     Delivery: the per-provider scoreboard (disk sessions → base messages →
>     usage_events → marts) that catches data loading but never reaching the
>     dashboard. Exit is non-zero on health findings always, and on delivery
>     gaps only with ``--fail-on-gap``.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit {"ok": bool, "findings": [...], "delivery": {...}} as JSON. |
| `--fail-on-gap` | opt | boolean | `False` | flag | Also exit non-zero when any provider's data is stranded (GAP/DISK_GAP in the delivery scoreboard). For CI / pre-release gates. |

### `etl` — UNPORTED · W8-T2 · RS-8-103*

> Run the ETL pipeline (raw messages → events → marts).

Subcommands: `backfill`, `status`

*No declared parameters.*

### `etl backfill` — UNPORTED · W8-T2 · RS-8-104*

> Convert all existing messages into usage_events, then refresh marts.
> 
>     Default mode is incremental: messages already converted on a prior
>     run are skipped via the ``uniq_events_msg`` UNIQUE index.
> 
>     ``--force`` first wipes ``usage_events`` + ``mart_watermark``,
>     rebuilds every mart from scratch, and then runs the normalize
>     pass fresh — useful after a normalizer change or a model rate
>     update.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--force` | opt | boolean | `False` | flag | Drop events + marts + watermarks and rebuild from scratch. |

### `etl status` — UNPORTED · W8-T2 · RS-8-105*

> Show ETL pipeline health: watcher, marts, events, lag.
> 
>     Reads the live store and renders a one-screen snapshot — the same
>     payload ``GET /api/etl/status`` returns. Works without a running
>     server (the CLI opens its own connection to ``~/.stackunderflow/store.db``).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format (text or json). |

### `export` — UNPORTED · W8-T3 · RS-8-050, RS-5-005

> Export aggregated usage data to a CSV or JSON file.
> 
>     With ``--period`` set, exports a single window. Without it, exports
>     a multi-period rollup (today / last 7 days / last 30 days) so a JSON
>     consumer never has to make three CLI calls. CSV always lays out
>     one section per period in the same file, separated by a blank line.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-f / --format` | opt | choice[csv, json] | `Sentinel.UNSET` | required, dest=fmt | Output format. |
| `-o / --output` | opt | path(file_okay=True, dir_okay=False, exists=False) | `Sentinel.UNSET` | required, dest=output | Destination file path. |
| `-p / --period` | opt | choice[today, week, month, all] | `None` | dest=period | Window. Omit to roll up today + 7 days + 30 days into one file. |
| `--provider` | opt | text | `None` | — | Filter by provider (e.g. claude, codex, cursor). |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include | Include only this project slug (repeatable). |
| `--exclude` | opt | text | `Sentinel.UNSET` | multiple | Exclude this project slug (repeatable). |
| `--force` | opt | boolean | `False` | flag | Overwrite the output file if it already exists. |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `find-failure-modes-for-file` — PORTED · W1 · RS-1-010

> List sessions where editing FILE led to a follow-up correction.
> 
>     Surfaces the sessions where a past edit to FILE was followed by the
>     user reporting it broke, the agent reverting it (``git revert`` /
>     ``git reset --hard`` / ``git checkout --``), or a complaint — each
>     with the evidence (the triggering message) plus an
>     ``outcome_confidence`` in [0.0, 1.0]. Rows below ``--min-confidence``
>     (default 0.5) are filtered out. The companion of
>     ``find-sessions-where-action-worked``: use this to learn why an edit
>     went wrong, that one to learn how a successful change was done.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `FILE` | arg | path(file_okay=True, dir_okay=True, exists=False) | `Sentinel.UNSET` | required |  |
| `--since` | opt | text | `None` | — | Only sessions whose edit is newer than this. Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Max sessions to return. |
| `--min-confidence` | opt | float | `None` | — | Minimum outcome confidence in [0.0, 1.0]. Default 0.5; both explicit-phrase complaints (0.8) and agent revert tool calls (0.5) clear it. Pass 0.0 to include lower-confidence inferences. |
| `--verbose / -v` | opt | boolean | `False` | flag | Append outcome_confidence to each row in text output. (JSON always carries it.) |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `find-sessions-in-path` — PORTED · W1 · RS-1-011

> List sessions whose project root is PATH or any ancestor of PATH.
> 
>     Useful when an agent is working in /a/b/c and wants to know what
>     has happened in the project rooted at /a/b. The match is
>     ancestor-only — projects rooted *below* PATH do not match.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PATH` | arg | path(file_okay=True, dir_okay=True, exists=False) | `Sentinel.UNSET` | required |  |
| `--since` | opt | text | `None` | — | Only sessions whose last activity is newer than this. Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Max sessions to return (hard cap). |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output. Results are ranked (recency + cost + relevance) and packed greedily until ~this many estimated tokens are used; a tail marker reports how many more matched. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |
| `--provider` | opt | text | `None` | — | Filter by provider slug (e.g. claude, codex, cursor). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `find-sessions-touching-file` — PORTED · W1 · RS-1-012

> List sessions where FILE shows up in tool calls or message text.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `FILE` | arg | path(file_okay=True, dir_okay=True, exists=False) | `Sentinel.UNSET` | required |  |
| `--limit` | opt | integer | `20` | — | Max sessions to return (hard cap). |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output (ranked + greedily packed). Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |
| `--mode` | opt | choice[read, write, any] | `'any'` | — | Match against Read tool args, Edit/Write tool args, or any mention (tools or freeform). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `find-sessions-where-action-worked` — PORTED · W1 · RS-1-013

> List sessions where ACTION was performed and the next user turn confirmed it worked.
> 
>     ACTION is matched as a substring against tool calls and message text,
>     so it can be a tool name ("Edit"), a file fragment ("cost.py"), or a
>     phrase from the conversation ("add caching"). For each session the
>     *last* matching message is the anchor; the outcome is inferred from
>     the following user turns (an explicit "thanks"/"that worked", an
>     agent revert command, or — at lower confidence — no signal at all
>     before the session ended). Each row carries an ``outcome_confidence``
>     in [0.0, 1.0]; rows below ``--min-confidence`` (default 0.5) are
>     filtered out. Pair with ``find-failure-modes-for-file`` to see where
>     an edit went wrong.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `ACTION` | arg | text | `Sentinel.UNSET` | required |  |
| `--project` | opt | text | `None` | — | Filter by project slug (e.g. -Users-yad-dev-foo). |
| `--file` | opt | text | `None` | dest=file_path | Narrow to sessions that also touched this file. |
| `--since` | opt | text | `None` | — | Only sessions whose matching activity is newer than this. Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Max sessions to return. |
| `--min-confidence` | opt | float | `None` | — | Minimum outcome confidence in [0.0, 1.0]. Default 0.5 — explicit-phrase confirmations clear it, 'silence ⇒ worked' rows (0.3) do not. Pass 0.0 to restore the legacy anything-that-didn't-break-is-a-success behaviour. |
| `--verbose / -v` | opt | boolean | `False` | flag | Append outcome_confidence to each row in text output. (JSON always carries it.) |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `guide` — PORTED · W8-T2 · RS-8-020

> Manage the StackUnderflow agent-discovery snippet in CLAUDE.md / AGENTS.md.

Subcommands: `install`, `status`, `uninstall`

*No declared parameters.*

### `guide install` — PORTED · W8-T2 · RS-8-051, RS-8-001

> Write the agent-discovery snippet into the instruction file(s) (idempotent, backs up first).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | project = ./CLAUDE.md and ./AGENTS.md in cwd's git root; user = ~/.claude/CLAUDE.md |
| `--dry-run` | opt | boolean | `False` | flag | Show what would change; write nothing. |

### `guide status` — PORTED · W8-T2 · RS-8-052

> Show where the StackUnderflow guide snippet is installed.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `None` | — | Limit to one scope (default: show both project and user). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |

### `guide uninstall` — PORTED · W8-T2 · RS-8-053

> Remove the StackUnderflow guide snippet (only our marked block; never the file).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | Which instruction file(s) to clean. |

### `hooks` — PORTED · W8-T2 · RS-8-021

> Manage opt-in Claude Code lifecycle hooks (hybrid capture).

Subcommands: `install`, `repair`, `run`, `status`, `uninstall`

*No declared parameters.*

### `hooks install` — PORTED · W8-T2 · RS-8-054, RS-8-004

> Register the StackUnderflow hooks in a settings.json (idempotent, backs up first).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | project = .claude/settings.json in cwd's git root; user = ~/.claude/settings.json |
| `--dry-run` | opt | boolean | `False` | flag | Show what would change; write nothing. |
| `--capture-content` | opt | boolean | `False` | flag | Store full hook payloads (prompt text, tool output) instead of sanitised metadata. Off by default — the conservative choice. |
| `--inject` | opt | boolean | `False` | flag | Also install the context-injection hooks (SessionStart / UserPromptSubmit / PreToolUse) that feed StackUnderflow's memory back into the live agent. Opt-in separately from capture; off by default. |

### `hooks repair` — PORTED · W8-T2 · RS-8-055, RS-8-005

> Rewrite stale StackUnderflow hook commands to the portable form (changes nothing else).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user, all] | `'project'` | — | project = cwd's git root; user = ~/.claude; all = walk $HOME for every .claude/settings.json |
| `--dry-run` | opt | boolean | `False` | flag | Report stale entries; rewrite nothing. |

### `hooks run` — PORTED · W8-T2 · RS-8-056

> Internal — invoked by Claude Code. Reads the hook payload as JSON on stdin.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `HOOK_ID` | arg | text | `Sentinel.UNSET` | required |  |
| `--capture-content` | opt | boolean | `False` | flag | Store the full payload (set by `hooks install --capture-content`). |

### `hooks status` — PORTED · W8-T2 · RS-8-057

> Show which StackUnderflow hooks are installed, where, and whether any are stale.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `None` | — | Limit to one scope (default: show both project and user). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |

### `hooks uninstall` — PORTED · W8-T2 · RS-8-058

> Remove the StackUnderflow hooks (only ours; never the file or other tools' hooks).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | Which settings.json to clean. |

### `import` — UNPORTED · W8-T2 · RS-8-101*

> Import external agent history via a user-supplied export command.
> 
>     For sources with no local transcript (cloud-gated tools), you supply an
>     export command in a ``stackunderflow-history-plugin.json`` manifest; we own
>     only the ``stackunderflow-history-jsonl-v1`` stream format. The command is
>     run with **no shell**, a cleared + allowlisted environment, and byte + time
>     caps; its stream is validated whole and upserted under the ``custom``
>     provider (namespaced by the manifest's ``source_id``). Resumption uses an
>     opaque cursor we store and replay but never interpret.
> 
>     Fail-closed: a non-zero exit, a timeout, or a malformed line aborts the
>     whole import and leaves the stored cursor un-advanced. Re-running an
>     unchanged export is an idempotent no-op (content-addressed ids).
> 
>     Also available as ``stax import``.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--history-source` | opt | text | `Sentinel.UNSET` | required | A named history source (resolved under ./.stackunderflow/history-plugins/ or ~/.stackunderflow/history-plugins/) or a path to a stackunderflow-history-plugin.json manifest (file or its directory). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `ingest` — UNPORTED · W7 · RS-8-110*

> Pull PR / CI data into the local store (REST backfill + webhook receiver).

Subcommands: `github`, `webhook`

*No declared parameters.*

### `ingest github` — UNPORTED · W7 · RS-8-111*, RS-5-020

> Backfill GitHub PRs + workflow runs for REPO into the local store.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--repo` | opt | text | `Sentinel.UNSET` | required | The GitHub repository slug (e.g. 'octocat/hello-world'). |
| `--token` | opt | text | `None` | — | GitHub PAT. Falls back to $STACKUNDERFLOW_GITHUB_TOKEN, then $GITHUB_TOKEN. Public repos work without one but rate-limit much faster. |
| `--state` | opt | choice[all, open, closed] | `'all'` | — | PR state filter passed to the GitHub API. |
| `--max-pages` | opt | int range(1..50) | `10` | — | Maximum pages of 100 to fetch per endpoint (PRs + CI). |
| `--no-ci` | opt | boolean | `False` | flag | Skip the workflow-runs fetch — useful for quick PR-only refreshes. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `ingest webhook` — UNPORTED · W7 · RS-8-112*

> Run the opt-in webhook receiver (PR + CI events).

Subcommands: `serve`

*No declared parameters.*

### `ingest webhook serve` — UNPORTED · W7 · RS-8-113*

> Serve the /api/webhooks/* endpoints on a dedicated port.
> 
>     The receiver verifies signatures against
>     $STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET (HMAC-SHA256) /
>     $STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET (token compare) /
>     $STACKUNDERFLOW_CI_WEBHOOK_SECRET (HMAC-SHA256). Any unset secret
>     causes the matching endpoint to return 503 — this is opt-in by
>     design; we never accept anonymous payloads.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--port` | opt | integer | `8096` | — | Port to bind the receiver on. |
| `--host` | opt | text | `'127.0.0.1'` | — | Bind address. Default 127.0.0.1 (loopback only). Override to 0.0.0.0 if you're tunneling from a public webhook URL. |

### `init` — PORTED · W7 · RS-8-059

> Start the dashboard (alias for ``start``).
> 
>     With ``--install-skills``, copies the three shipped Claude Code
>     ``SKILL.md`` files into ``~/.claude/skills/`` (or ``--skills-dest``)
>     before starting the dashboard. See ``docs/skills.md``.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--port` | opt | integer | `None` | — |  |
| `--host` | opt | text | `None` | — |  |
| `--no-browser` | opt | boolean | `False` | flag |  |
| `--clear-cache` | opt | boolean | `False` | flag |  |
| `--install-skills` | opt | boolean | `False` | flag | Copy every shipped Claude Code skill (discovered from the packaged skills/ tree) into the skills destination (default ~/.claude/skills/) before starting the dashboard. Idempotent: byte-identical files are skipped silently. |
| `--skills-dest` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | — | Destination directory for --install-skills. Defaults to ~/.claude/skills/. Useful for testing and advanced setups where Claude Code reads skills from a non-standard location. |
| `--skills-force` | opt | boolean | `False` | flag | With --install-skills, overwrite destination SKILL.md files that differ from the shipped copy. Default behaviour preserves local edits — a modified destination is skipped with a warning. |

### `memory` — PORTED · W1 · RS-1-001

> Ask the local store what past sessions already know.
> 
>     ``memory`` is the agent-facing namespace: one set of commands, one
>     output contract. Run any subcommand with ``--json`` to get the stable,
>     token-bounded agent-output envelope an agent can splice straight into
>     its context window; without it you get a human-readable summary.
> 
>     Every subcommand shares ``--format`` / ``--json``, ``--project``,
>     ``--since``, ``--limit`` and ``--context-budget``. ``--project``
>     defaults to the current directory's project when StackUnderflow
>     recognises it, so these commands Just Work when run inside a repo.

Subcommands: `ask`, `decisions`, `embed`, `file`, `sessions`, `worked`

*No declared parameters.*

### `memory ask` — PORTED · W1 · RS-1-014

> Ask a natural-language question of the local store.
> 
>     ``ask`` runs a **hybrid** retrieval: a keyword search over past
>     decisions fused (reciprocal-rank fusion) with a local semantic vector
>     search. The vector half uses a small local embedding model served by
>     Ollama; when Ollama is not running it is silently skipped and ``ask``
>     degrades to the keyword search alone — so the command always works,
>     and gets sharper (finds sessions you didn't have the exact words for)
>     when a local Ollama is available. Every result carries its provenance:
>     session id, date (``last_ts``) and cost (``cost_usd``).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `QUESTION` | arg | text | `Sentinel.UNSET` | required |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--project` | opt | text | `None` | — | Project slug to scope to. Default: the current directory's project, when StackUnderflow recognises it. |
| `--since` | opt | text | `None` | — | Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Hard cap on the number of results. |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output: results are ranked and packed greedily until ~this many estimated tokens are used. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |

### `memory decisions` — PORTED · W1 · RS-1-001

> Search past decisions — "did I decide something about this before?"
> 
>     Substring-searches QUERY across past message content and returns the
>     matching sessions, newest first, each with a short snippet. Wraps
>     ``services/discovery.py``'s ``search_past_decisions``.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `QUERY` | arg | text | `Sentinel.UNSET` | required |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--project` | opt | text | `None` | — | Project slug to scope to. Default: the current directory's project, when StackUnderflow recognises it. |
| `--since` | opt | text | `None` | — | Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Hard cap on the number of results. |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output: results are ranked and packed greedily until ~this many estimated tokens are used. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |

### `memory embed` — UNPORTED · W8-T2 · RS-8-088*

> Backfill vector embeddings for your existing indexed messages.
> 
>     ``memory ask`` embeds NEW messages as they're ingested; this one-time
>     backfill embeds everything already in the search index so semantic recall
>     works over your whole history. Needs a reachable Ollama — cloud
>     (``STACKUNDERFLOW_OLLAMA_URL`` + ``STACKUNDERFLOW_OLLAMA_API_KEY``) or
>     local; with neither it explains how to enable one and exits.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--batch` | opt | integer | `512` | — | Messages embedded per batch. |

### `memory file` — PORTED · W1 · RS-1-001

> Everything known about a file — "what do I know about this file?"
> 
>     Merges three file-scoped discovery calls into one report: known
>     failure modes, every session that touched the file, and a risk
>     summary (revert / fail / work counts). PATH is resolved against the
>     current directory, so ``memory file src/foo.py`` works inside a repo.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PATH` | arg | path(file_okay=True, dir_okay=True, exists=False) | `Sentinel.UNSET` | required |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--project` | opt | text | `None` | — | Project slug to scope to. Default: the current directory's project, when StackUnderflow recognises it. |
| `--since` | opt | text | `None` | — | Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Hard cap on the number of results. |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output: results are ranked and packed greedily until ~this many estimated tokens are used. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |

### `memory sessions` — PORTED · W1 · RS-1-001

> List past sessions that touched here — "which sessions ran here?"
> 
>     With no PATH, lists sessions for the current directory's project. Give
>     a directory to scope to that project tree, or a file to list only the
>     sessions that touched that file. An explicit ``--project SLUG``
>     overrides PATH. Wraps ``services/discovery.py``'s
>     ``find_sessions_in_path`` / ``find_sessions_touching_file``; note the
>     file form has no time bound, so ``--since`` applies to the path form
>     only.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PATH` | arg | text | `None` | — |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--project` | opt | text | `None` | — | Project slug to scope to. Default: the current directory's project, when StackUnderflow recognises it. |
| `--since` | opt | text | `None` | — | Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Hard cap on the number of results. |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output: results are ranked and packed greedily until ~this many estimated tokens are used. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |

### `memory worked` — PORTED · W1 · RS-1-001

> Find where an action worked — "what worked last time I tried this?"
> 
>     ACTION is matched as a substring against tool calls and message text.
>     Returns sessions where ACTION was performed and the next user turn
>     confirmed success. Wraps ``services/discovery.py``'s
>     ``find_sessions_where_action_worked``.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `ACTION` | arg | text | `Sentinel.UNSET` | required |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. 'json' emits the stable agent-output envelope. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Shortcut for --format json. |
| `--project` | opt | text | `None` | — | Project slug to scope to. Default: the current directory's project, when StackUnderflow recognises it. |
| `--since` | opt | text | `None` | — | Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--limit` | opt | integer | `20` | — | Hard cap on the number of results. |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output: results are ranked and packed greedily until ~this many estimated tokens are used. Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |

### `month` — PORTED · W8-T3 · RS-8-060, RS-5-002

> This month's usage.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include |  |
| `--exclude` | opt | text | `Sentinel.UNSET` | multiple |  |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `optimize` — UNPORTED · W8-T3 · RS-8-061, RS-5-007

> Find wasted spend: looped Q&A pairs plus seven structural waste patterns.
> 
>     The legacy ``waste`` block lists projects where the assistant had to
>     retry repeatedly. The ``patterns`` block surfaces structural waste
>     detected from filesystem state and tool-call history (bloated
>     CLAUDE.md, unused MCP servers, ghost agents, junk reads, cache
>     thrash, oversized bash output, exploration-only sessions).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-p / --period` | opt | text | `'30days'` | dest=period |  |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include |  |
| `--exclude` | opt | text | `Sentinel.UNSET` | multiple |  |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `plan` — PORTED · W8-T3 · RS-8-022

> Manage and inspect a monthly plan budget (Claude Pro, Cursor Pro, custom).

Subcommands: `reset`, `set`, `show`, `thresholds`

*No declared parameters.*

### `plan reset` — PORTED · W8-T3 · RS-8-062

> Clear the active plan.

*No declared parameters.*

### `plan set` — PORTED · W8-T3 · RS-8-063

> Set the active plan. NAME is one of: claude-pro, claude-max, cursor-pro, cursor-max, custom.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `NAME` | arg | text | `Sentinel.UNSET` | required |  |
| `--monthly-usd` | opt | float | `None` | — | Monthly budget in USD (required for 'custom', overrides preset otherwise). |
| `--reset-day` | opt | int range(1..31) | `1` | — | Day of month the budget resets (default 1). |

### `plan show` — PORTED · W8-T3 · RS-8-064

> Show the active plan, current usage against budget, and burn projection.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |

### `plan thresholds` — PORTED · W8-T3 · RS-8-023

> Configure burn-projector alert thresholds (default 50% / 75% / 90%).

Subcommands: `reset`, `set`, `show`

*No declared parameters.*

### `plan thresholds reset` — PORTED · W8-T3 · RS-8-065

> Restore the default thresholds (50% / 75% / 90%).

*No declared parameters.*

### `plan thresholds set` — PORTED · W8-T3 · RS-8-066

> Set the alert thresholds (positional integers in [1, 200]).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `VALUES` | arg | integer | `Sentinel.UNSET` | required, nargs=-1 |  |

### `plan thresholds show` — PORTED · W8-T3 · RS-8-067

> Show the active alert thresholds.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |

### `pricing` — UNPORTED · W8-T2 · RS-8-108*

> Inspect model pricing health (read-only).

Subcommands: `doctor`

*No declared parameters.*

### `pricing doctor` — UNPORTED · W8-T2 · RS-8-109*

> Report pricing health: unpriced models, stale rates, unknown cost rows.
> 
>     Reads the live store (``~/.stackunderflow/store.db``) and renders the
>     same payload ``GET /api/pricing/doctor`` returns. Works without a
>     running server. Strictly read-only — no DB writes, no network.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format (text or json). |
| `--stale-days` | opt | integer | `7` | — | Flag the rate overlay stale when older than this many days. |
| `--limit` | opt | integer | `50` | — | Max model entries listed per section (full counts stay in the summary). |
| `--strict` | opt | boolean | `False` | flag | Exit non-zero when a hard defect is found (billable unpriced model or unknown row with nonzero cost) — for CI gating. |

### `recommend` — PORTED · W8-T4 · RS-8-024

> Proactive recommendations mined from your local session store.
> 
>     Recommendations are read-only — accepting one is always a separate
>     explicit step (e.g. ``stackunderflow skills generate --pattern <id>``).

Subcommands: `mode`, `skills`

*No declared parameters.*

### `recommend mode` — PORTED · W8-T4 · RS-8-068

> Recommend the cheapest model that fits this task.
> 
>     Uses your local session history (``~/.stackunderflow/store.db``) —
>     nothing leaves the machine. ``confidence == 0.0`` means "not enough
>     similar past sessions, no opinion".

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--prompt` | opt | text | `Sentinel.UNSET` | required | The task prompt to score (text in quotes). |
| `--current-model` | opt | text | `None` | — | Model you'd otherwise route to. Drives the cost-delta. |
| `--no-cache` | opt | boolean | `False` | flag | Skip the 24h cache (recompute from history). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `recommend skills` — PORTED · W8-T4 · RS-8-069, RS-8-012

> List patterns you've manually re-run that could become auto-skills.
> 
>     Reads ``messages`` + on-disk skills to find workflow patterns above
>     ``--threshold`` occurrences that you don't yet have a skill for.
>     Acceptance is never automatic — each row carries an ``accept_command``
>     you can paste to install the skill.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--project` | opt | text | `None` | — | Project slug to scan. Default: the project the current directory belongs to. |
| `--threshold` | opt | int range(1..inf) | `5` | — | A pattern must appear in this many distinct sessions. |
| `--window-days` | opt | int range(1..inf) | `30` | — | Lookback window in days. |
| `--no-cache` | opt | boolean | `False` | flag | Bypass the recommendation cache and re-mine. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `reindex` — UNPORTED · W8-T2 · RS-8-102*

> Rebuild the session store from scratch.

*No declared parameters.*

### `report` — PORTED · W8-T3 · RS-8-070, RS-5-002

> Dashboard-style summary over a date range.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-p / --period` | opt | text | `'7days'` | dest=period | Period: today, 7days, 30days, month, all |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include | Include only these project dir names (repeatable) |
| `--exclude` | opt | text | `Sentinel.UNSET` | multiple | Exclude these project dir names (repeatable) |
| `--provider` | opt | choice[all, antigravity, claude, cline, codeium, codex, continue, copilot, cursor, cursor-agent, droid, gemini, grok, hermes, kilocode, kiro, openclaw, opencode, pi, qwen, roocode] | `'all'` | — | Provider filter (stub — wired in Plan C) |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `resume` — PORTED · W1 · RS-1-002

> Session/resume ids for every coding agent under PATH (default: cwd).
> 
>     Groups recent sessions by provider and renders each agent's real
>     resume invocation (templates are data in ``adapters/capabilities.json``,
>     verified against the actual CLIs — e.g. ``claude --resume <id>``,
>     ``codex resume <id>``). Matching is bidirectional: standing inside a
>     project finds it, and giving a workspace folder lists every project
>     underneath. Read-only; agents whose CLI has no known resume command
>     still list their session ids.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PATH` | arg | text | `None` | — |  |
| `--provider / -p` | opt | text | `Sentinel.UNSET` | multiple, dest=provider_filter | Only this agent (repeatable): claude, codex, grok, … Case-insensitive; an unambiguous prefix works (e.g. -p cod). |
| `--limit-per-provider` | opt | int range(1..inf) | `5` | — | Max sessions listed per coding agent. |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit the machine envelope. |

### `risk` — PORTED · W8-T6 · RS-8-025

> Surface "this file has caused N reverts in M days" before editing it.
> 
>     Read-only aggregator over the v0.7.2 outcome heuristic. No new
>     schema; counts are computed from existing
>     ``messages`` / ``sessions`` rows on each call.

Subcommands: `file`

*No declared parameters.*

### `risk file` — PORTED · W8-T6 · RS-8-071, RS-8-011

> Risk summary for PATH: how many sessions reverted / failed / worked.
> 
>     Counts distinct sessions classified by the v0.7.2 outcome heuristic.
>     ``recent_session_ids`` is the up-to-5 most recent failure-mode
>     sessions (reverted ∪ failed) for the file.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `PATH` | arg | path(file_okay=True, dir_okay=True, exists=False) | `Sentinel.UNSET` | required |  |
| `--since` | opt | text | `None` | — | Only sessions whose activity is newer than this. Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `search-past-decisions` — PORTED · W1 · RS-1-020

> Substring-search QUERY across past message content; return matching sessions.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `QUERY` | arg | text | `Sentinel.UNSET` | required |  |
| `--project` | opt | text | `None` | — | Filter by project slug (e.g. -Users-yad-dev-foo). |
| `--since` | opt | text | `None` | — | Filter to messages newer than this. Accepts '7d', '1w', '1m', '24h', or ISO. |
| `--limit` | opt | integer | `20` | — | Max sessions to return (hard cap). |
| `--context-budget` | opt | integer | `None` | — | Token budget for the output (ranked + greedily packed). Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable. |
| `--use-embeddings` | opt | boolean | `False` | flag | Re-rank substring matches by Ollama embeddings (cosine similarity), the same backend as `memory ask`. The substring filter still runs first; embeddings only re-rank the candidate set. Each JSON row gains an `embedding_score` in [0, 1]. Degrades silently to substring ranking when Ollama is unreachable. |
| `--embed-model` | opt | text | `None` | — | Override the Ollama embed model. Default: STACKUNDERFLOW_EMBED_MODEL or nomic-embed-text. Ignored without --use-embeddings. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `skills` — PORTED · W8-T4 · RS-8-026

> Generate / list / clean project-specific Claude Code skills.
> 
>     These are mined from your local session store — never from CLAUDE.md
>     or memory — and are always project-scoped unless you ask otherwise.

Subcommands: `clean`, `generate`, `list`

*No declared parameters.*

### `skills clean` — PORTED · W8-T4 · RS-8-072, RS-8-013

> Remove auto-generated skills (never touches hand-authored ones).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | Where to clean: ./.claude/skills/ or ~/.claude/skills/. |
| `--out` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | dest=out_path | Skills directory to clean. Default depends on --scope. |
| `--older-than` | opt | text | `None` | — | Only remove skills generated before this ('30d'/'2w'/ISO). Default: remove all auto-generated skills. |
| `--dry-run` | opt | boolean | `False` | flag | Show what would be removed; delete nothing. |
| `--yes / -y` | opt | boolean | `False` | flag, dest=assume_yes | Actually delete. Without this, clean only previews. |

### `skills generate` — PORTED · W8-T4 · RS-8-073, RS-8-013

> Mine session patterns and emit auto-generated SKILL.md files.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--project` | opt | text | `None` | — | Project slug to mine. Default: the project the current directory belongs to (for --scope project). |
| `--projects` | opt | text | `None` | — | Comma-separated slugs for cross-project mining (required for --scope user when --project is not given). |
| `--scope` | opt | choice[project, user] | `'project'` | — | project → ./.claude/skills/ ; user → ~/.claude/skills/ (global; requires explicit --project/--projects). |
| `--min-occurrences` | opt | int range(1..inf) | `5` | — | A pattern must appear in this many distinct sessions. |
| `--kind` | opt | choice[avoids-X, never-touches-paths, canonical-test-command, always-runs-X-after-Y, uses-tool-flag-combo] | `Sentinel.UNSET` | multiple, dest=kinds | Restrict to these pattern kinds (repeatable). Default: all. |
| `--window` | opt | text | `'90d'` | — | Only consider sessions newer than this ('90d'/'1w'/ISO; 'all' or empty for no bound). |
| `--out` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | dest=out_path | Output directory. Default depends on --scope. |
| `--dry-run` | opt | boolean | `False` | flag | Show what would be generated; write nothing. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `skills list` — PORTED · W8-T4 · RS-8-074, RS-8-013

> List the auto-generated skills present in the skills directory.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--scope` | opt | choice[project, user] | `'project'` | — | Where to look: ./.claude/skills/ or ~/.claude/skills/. |
| `--out` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | dest=out_path | Skills directory to inspect. Default depends on --scope. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |

### `start` — PORTED · W7 · RS-8-075

> Launch the StackUnderflow dashboard.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-p / --port` | opt | integer | `None` | dest=port | Server port |
| `-H / --host` | opt | text | `None` | dest=host | Bind address |
| `--headless` | opt | boolean | `False` | flag | Don't open the browser |
| `--fresh` | opt | boolean | `False` | flag | Clear disk cache first |
| `--no-watcher` | opt | boolean | `False` | flag | Disable the Wave 2C ETL filesystem watcher (headless / debugging). |
| `--no-lock` | opt | boolean | `False` | flag | Skip the singleton watcher lock at ~/.stackunderflow/server.lock. Headless / test scenarios only — letting two instances run watchers against the same store will race on ingest+marts. |
| `--data-dir` | opt | path(file_okay=False, dir_okay=True, exists=False) | `None` | — | Serve a dataset from somewhere other than ~/.stackunderflow — a store copied off another machine, or a backup's stackunderflow-state/ directory. Same as setting STACKUNDERFLOW_HOME. |

### `status` — PORTED · W8-T1 · RS-8-089*

> Compact one-liner: today + month cost and message counts.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `sync` — PORTED · W8-T2 · RS-8-096*

> Encrypted, bring-your-own-bucket backup of your analytics aggregates (opt-in).

Subcommands: `init`, `pull`, `push`, `status`

*No declared parameters.*

### `sync init` — PORTED · W8-T2 · RS-8-097*

> Generate this device's encryption key and record the bucket destination.
> 
>     Prints the freshly generated key ONCE — save it, and copy it to your other
>     devices. Only the key's fingerprint is stored in the database; the secret
>     lives in a 0600 file (or the keychain / STACKUNDERFLOW_SYNC_KEY env var).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--bucket` | opt | text | `Sentinel.UNSET` | required, dest=bucket_url | Sync destination: s3://my-bucket[/prefix] for any S3-compatible store, or ssh://[user@]host[:port]/abs/path to sync between machines you own with no bucket at all |
| `--endpoint` | opt | text | `None` | dest=endpoint_url | Custom object-store endpoint URL (set it for non-default storage providers) |
| `--force` | opt | boolean | `False` | flag | Replace an existing sync key on this device (destroys access to data encrypted under the old key — back it up first) |

### `sync pull` — PORTED · W8-T2 · RS-8-099*

> Fetch and merge every OTHER device's encrypted aggregates from your bucket.
> 
>     Reads each peer's prefix (never writes to it), downloads only the shards that
>     changed since the last pull, decrypts + verifies them, and lands them in the
>     local remote tables. The unified cross-device view is then available at
>     /api/sync/overview?scope=all-devices. Idempotent — an unchanged peer downloads
>     nothing. Exits non-zero on a hard failure (e.g. bucket unreachable) so it is
>     safe to script; per-peer/per-shard problems are reported as warnings, not fatal.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit machine-readable JSON |

### `sync push` — PORTED · W8-T2 · RS-8-098*

> Encrypt and upload changed aggregate shards to your bucket.
> 
>     Idempotent — an unchanged shard is skipped (zero uploads). Exits non-zero on
>     any failure so it is safe to script.

*No declared parameters.*

### `sync status` — PORTED · W8-T2 · RS-8-100*

> Show sync configuration and how many shards are pending upload (local only).

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--json` | opt | boolean | `False` | flag, dest=as_json | Emit machine-readable JSON |

### `today` — PORTED · W8-T3 · RS-8-076, RS-5-002

> Today's usage.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt |  |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include |  |
| `--exclude` | opt | text | `Sentinel.UNSET` | multiple |  |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

### `worktrees` — UNPORTED · W8-T3 · RS-8-027

> Inspect git worktrees: owner project, cost, prune safety (read-only).

Subcommands: `attribute`, `list`

*No declared parameters.*

### `worktrees attribute` — UNPORTED · W8-T3 · RS-8-077

> Attribute worktree session fragments to their parent projects.
> 
>     Rolls phantom sibling "projects" (worktree session logs) up into the
>     project that owns them. Writes ONLY the additive attribution column in
>     the store — never git state. Idempotent: once every fragment is linked,
>     re-running reports 0 rows updated.

*No declared parameters.*

### `worktrees list` — UNPORTED · W8-T3 · RS-8-078

> List known worktrees with a verdict: ACTIVE, MERGED_SAFE_TO_PRUNE, or HAS_UNIQUE_WORK.
> 
>     Reads the live store and renders the same payload ``GET /api/worktrees``
>     returns. Works without a running server. Read-only: git is only queried,
>     never mutated — prune commands ship in the json payload as a preview for
>     you to run yourself.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `--project` | opt | path(file_okay=True, dir_okay=True, exists=False) | `None` | — | Project log path or repo root to scan; omit to scan every known root. |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format (text or json). |

### `yield` — PORTED · W8-T3 · RS-8-079

> Yield analysis: productive vs reverted vs abandoned sessions.
> 
>     Cross-references each session's cwd with the git commit history of that
>     repo over a 24-hour window after the session started. A session is
>     "productive" if a non-reverted commit lands in that window, "reverted"
>     if the commit was later reverted (or wiped from HEAD), "abandoned" if
>     no commit followed, and "no_repo" if the cwd isn't a git repo.
> 
>     Heuristic warning: this correlates by time, not by content. A commit
>     within 24h is credited to the session even if it's about something else.

| spec | kind | type | default | modifiers | help |
| --- | --- | --- | --- | --- | --- |
| `-p / --period` | opt | choice[today, week, month, all, 7days, 30days] | `'month'` | dest=period | Period to analyse. |
| `--project` | opt | text | `Sentinel.UNSET` | multiple, dest=include | Filter by project slug (repeatable). |
| `--format` | opt | choice[text, json] | `'text'` | dest=fmt | Output format. |
| `--ingest` | opt | boolean | `False` | flag, dest=do_ingest | Force a fresh ingest+backfill pass before running the command. Useful when 'stackunderflow start' is not active. |
| `--auto-ingest \\| --no-auto-ingest` | opt | boolean | `True` | flag | Refresh the store automatically when its newest event is older than the staleness threshold. Default on. Disable with --no-auto-ingest. |

## 4. Ledger gaps this inventory found

`rust/TASKS-RS.md`'s wave-8 block carries 87 items, of which RS-8-014..RS-8-079 name a command path. Cross-checking that list against the live tree leaves the following commands with **no item at all** — every one of them is a real, user-reachable verb:

| path | why it was missed |
| --- | --- |
| `status` | **the reserved verb.** DIV-025 renamed Rust's `status` to `store` precisely so this could be ported, and the item list never carried it |
| `backup` | the whole `backup` group (6 nodes) is absent — it is `cli.py`'s single largest command by body length (`create`, 123 lines) |
| `sync` | the whole `sync` group (5 nodes) is absent |
| `etl` | the whole `etl` group (3 nodes) is absent |
| `ingest` | the whole `ingest` group (4 nodes incl. the nested `webhook` group) is absent |
| `pricing` | the whole `pricing` group (2 nodes) is absent |
| `analyze` | the group node and `analyze backfill` are absent (`quality` / `session` have items) |
| `import` | absent |
| `reindex` | absent |
| `memory embed` | absent — the one `memory` verb wave 1 did not port |

Filed as RS-8-088..RS-8-113 in `rust/TASKS-RS.md` (additive; no existing item was renumbered).

## 5. Output shapes — the tranche-1 verbs

Extracted from the command bodies rather than from Click, so this section is hand-maintained and scoped to what this tranche ported.

| command | stdout shape | exit | writes |
| --- | --- | ---: | --- |
| `cfg ls` | `Settings:` then one `  {key:<34s}  {rendered:<14s}  [{src}]` line per key, `sorted(data)`; `rendered` is `json.dumps(v)` for a dict else `str(v)`; `src` ∈ env/file/default | 0 | — |
| `cfg ls --json` | `json.dumps(get_all(), indent=2)` — **declaration order**, not sorted | 0 | — |
| `cfg set K V` | `  {key} = {final}` where `final` is re-read after persist (currency uppercases) | 0 | `$HOME/config.json` |
| `cfg set` (bad key / dict key / `plan_*` key) | Click `BadParameter` with `param_hint=KEY`: usage block + `Error: Invalid value for KEY: …` | 2 | — |
| `cfg rm K` | `  {key} removed` — unconditionally, even for a key that was never set | 0 | `config.json` (**created** if absent — `_save` always writes) |
| `cfg model-alias ls` | `No model aliases configured.` or `Model aliases:` + `  {src:<width}  ->  {dst}` per sorted source | 0 | — |
| `cfg model-alias ls --json` | `json.dumps(aliases, indent=2, sort_keys=True)` | 0 | — |
| `cfg model-alias set S T` | `  {source} -> {target}` | 0 | `config.json` |
| `cfg model-alias rm S` | `  {source} removed`, or `  no alias for {source!r}` (Python `repr`) and **no write** | 0 | `config.json` (only on a hit) |
| `config show|set|unset` | `ctx.invoke` into `cfg ls` / `cfg set` / `cfg rm` — byte-identical output to the target | as target | as target |
| `clear-cache [PROJECT]` | optional `  cursor parse cache cleared.` then two fixed lines; **PROJECT is accepted and ignored** | 0 | deletes `$HOME/cache/cursor-results.json` |
| `status` | `today: $X.XX (N msg) \| month: $Y.YY (M msg)` | 0 | `store.db` (schema apply; ingest when stale unless `--no-auto-ingest`) |
| `status --format json` | `json.dumps({"today": report, "month": report}, indent=2, sort_keys=False)` | 0 | as above |
| `backup list` | `  No backups yet. Run: stackunderflow backup create`, or `  N backup(s) in {dir}` + blank + `  {name}  ({files} files, {mb:.1f} MB)` per dir | 0 | — |
| `backup verify` | `  Verifying {name}` + `    {artifact:<16} ok\|MISSING` ×4 + a summary line | 0 / 1 | — |


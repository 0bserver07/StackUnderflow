# StackUnderflow Technical Overview

StackUnderflow is a local log parser, transaction analyzer, and command-history indexer for terminal-based code generators and IDE extensions. It runs as a lightweight, single-process service on your local machine with zero external network dependencies or telemetry.

---

## What It Does (The Core Mechanics)

Instead of relying on cloud telemetry, StackUnderflow acts as a local database engine that parses, normalizes, and aggregates command-line and filesystem action logs written by coding tools (such as Claude Code, Cursor, or Cline).

```
[Local Logs] ──(Watcher)──> [Raw DB Rows] ──(Normalizer)──> [Unified Transactions] ──(ETL Marts)──> [Fast REST API]
```

It solves three practical problems:
1. **Spending Audits:** What are these tools costing in API tokens, which models are being queried, and where is the budget going?
2. **Command & File Playback:** How did a terminal tool modify a specific file step-by-step over a multi-hour session?
3. **Session Querying & Memory:** How can a new session query the results, command history, and failures of previous runs in the same directory?

---

## Under the Hood: The Data Pipeline

StackUnderflow’s backend is a Python service structured as a classic data-warehousing pipeline:

### 1. Ingest & Watcher Layer
* **Log Watcher:** A background thread monitors file system paths where supported tools write their run logs (e.g., `~/.claude/projects/`, Cursor's SQLite database, or Cline's JSON tasks).
* **Incremental Ingest:** When a change is detected, it reads the new lines of logs and writes them to an immutable `raw` layer in a local SQLite database (`~/.stackunderflow/store.db`).

### 2. Normalization Layer
* **Provider Adapters:** Custom adapters parse logs from 17 different coding tools, extracting fields like input tokens, output tokens, system prompts, API calls, and tool outputs.
* **Pricing Calculator:** Maps these tokens to an offline price card (per-model cost tables) to calculate estimated dollar costs in real-time.
* **Unified Schema:** Writes these standard records to a centralized `usage_events` table.

### 3. Marts Layer (Data Agregation)
To keep the dashboard fast (rendering in under 50ms), StackUnderflow runs a local ETL builder. It aggregates data from the `usage_events` table into high-performance reporting tables ("marts"):
* **`daily_mart`:** Rolled up token consumption and costs per calendar day.
* **`session_mart`:** Total duration, costs, and statuses for each unique coding run.
* **`tool_mart` & `command_mart`:** Execution counts and costs for specific terminal commands (e.g., `grep`, `git`, `npm run build`).
* **`yield_mart`:** Cross-references session runtimes with your local `git log` to measure how many runs actually resulted in a commit (productive vs. abandoned runs).

---

## Key Technical Features

### 🕒 Filesystem Playback (Time-Travel)
When a coding tool runs edits on a project, it issues file-modification commands. StackUnderflow's playback engine parses these mutations (Read, Write, Edit, Multi-Edit, and Notebook edits) and reconstructs a sandboxed virtual directory at any specific millisecond in the timeline. You can scrub through a session's history and see exactly what the files looked like before and after each automated edit.

### 🧠 Local Command & Session Memory (CLI)
StackUnderflow exposes a local CLI — the `stackunderflow memory ...` command namespace. 
* This allows active terminal tools to query their own local run history *before* starting a new task (e.g., running `stackunderflow memory file <path>` to see what previous runs changed in a file, or finding what actions succeeded/failed previously).
* It provides programmatic, offline semantic search over command outputs using a local sentence-transformers model.

### 💬 Local Chat Proxy
The dashboard sidebar links directly to your localhost Ollama instance (`http://localhost:11434`). It proxies requests through a local FastAPI controller to avoid CORS friction, letting you chat with your local command history using models like `qwen2.5-coder` or `llama3.2` without sending any data over the internet.

---
name: find-related-sessions
description: Use when the user references a SPECIFIC FILE in a coding task — "fix the bug in routes/cost.py", "what was that change to api.ts about", "refactor python-legacy: store/queries.py", or any request that names a path. Surfaces past sessions that read or wrote that file so the work is grounded in prior context (recent edits, why the file is shaped that way, half-finished refactors).
---

# Find sessions related to a specific file

## Why this skill exists

When the user mentions a specific file, prior work touching that file is the highest-signal context available. StackUnderflow's per-message tool index records which files each session read or wrote. One CLI call returns the recent sessions that touched the path — so you know what the last change was, why it was made, and whether the user's current request continues or contradicts it.

## When this skill fires

Trigger when the user's request includes a concrete file path (with extension or recognizable file shape), AND the request is non-trivial (modify, fix, explain-in-context, refactor):

- "Fix the bug in `routes/cost.py`"
- "What was that change to `api.ts` about?"
- "Why is `python-legacy: store/queries.py` so long?"
- "Refactor `src/components/Dashboard.tsx`"
- "Can you look at `tests/test_aggregator.py`?"

Do NOT fire for:

- Pure read requests with no follow-up ("show me `foo.py`")
- File paths in error messages or stack traces (use only when the user is explicitly directing attention there)
- Path-shaped strings that aren't real files (URLs, config keys)

## Step 1: surface sessions touching the file

```bash
stackunderflow find-sessions-touching-file <path> --format json --limit 5 --mode any
```

The `<path>` argument can be relative to the user's cwd or absolute — both work. The CLI normalizes against the project's session index.

`--mode` filters by access type:
- `any` (default) — sessions that read OR wrote the file
- `read` — sessions that opened/grepped the file
- `write` — sessions that edited the file (highest signal for "what changed and why")

For most requests, `--mode any` is right. If the user says "what changed in this file", switch to `--mode write` to focus on edits.

JSON shape (one entry per session):

```json
{
  "sessions": [
    {"session_id": "...", "project_slug": "...", "project_path": "/abs/path",
     "provider": "claude", "first_ts": "...", "last_ts": "...",
     "message_count": 42, "cost_usd": 0.85, "snippet": "first user message"}
  ]
}
```

## Step 2: read the snippets, prioritize recency

Sessions come back newest-first. The `snippet` summarizes what each session was about. For a "what changed" question, the most recent session that wrote the file is usually the answer. For a "why is this shaped this way" question, scan the last several sessions for the design discussion.

## Step 3: ground the work

- If a recent session refactored or rewrote the file: don't redo work the user already accepted. Mention what was done; ask if the new request supersedes or extends it.
- If the file was the subject of an unfinished change: pick up where that session left off.
- If no sessions touched the file in the indexed window: just proceed with the task. (`sessions: []` is a valid result — `find-sessions-touching-file` is cheap to run speculatively.)

## Showing the user vs. consuming internally

Default `--format json` is for agent consumption. When the user asks "show me sessions that touched X" or "when was Y last edited", use the human-readable form:

```bash
stackunderflow find-sessions-touching-file <path> --mode write --limit 10
```

Text output is sorted newest-first with timestamps and one-line snippets — readable as a changelog of who-touched-what.

## Alternate path: MCP

If the StackUnderflow MCP server is connected, the equivalent is the `find_sessions_touching_file` tool with the same arguments. The CLI is the fallback because it's universally available.

## Cost

Local SQLite query — no network, no LLM call. <100 ms even on large stores. Run liberally.

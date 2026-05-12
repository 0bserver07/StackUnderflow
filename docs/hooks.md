# Hybrid capture — opt-in Claude Code hooks

StackUnderflow normally learns about your sessions *after the fact*: a filesystem
watcher reads the JSONL transcripts once Claude Code has written them. That tells
you *what was said* but loses *what just happened* — a tool call that failed, a
correction mid-flight, a session about to compact.

Claude Code exposes lifecycle **hooks**: commands it runs on certain events.
StackUnderflow can register a handful of them so it gets that signal *as the
action completes*, deterministically — no scanning stdout for `ERROR`, no waiting
on the debounce. This is **opt-in**: nothing is installed unless you run
`stackunderflow hooks install` yourself, and uninstalling is one command.

If you don't install hooks, nothing changes — StackUnderflow keeps working off the
transcripts, and consumers that benefit from hook data (outcome-aware discovery)
fall back to a transcript heuristic.

## What gets captured

`install` registers four hooks. Each fire that's worth recording becomes one row
in the `captured_events` table (migration `v010`):

| Claude Code event | hook id                       | recorded when                              | `event_kind` |
|-------------------|-------------------------------|--------------------------------------------|--------------|
| `PostToolUse` (scoped to `Bash`) | `stackunderflow-post-tool-use` | the Bash call exited non-zero               | `failure`    |
| `UserPromptSubmit` | `stackunderflow-user-prompt`  | the prompt matched the correction heuristic | `correction` |
| `Stop`             | `stackunderflow-stop`         | always (a turn boundary)                    | `boundary`   |
| `PreCompact`       | `stackunderflow-pre-compact`  | always (pre-compaction snapshot)            | `snapshot`   |

A *successful* Bash call records nothing. An ordinary prompt records nothing. The
table stays small and focused on the interesting moments.

`captured_events` shape:

```sql
CREATE TABLE captured_events (
    id              INTEGER PRIMARY KEY,
    ts              TEXT NOT NULL,         -- ISO 8601 UTC, sub-second
    project_id      INTEGER,                -- best-effort cwd→slug match; nullable
    session_id      TEXT,                   -- from the hook payload, if present
    hook_id         TEXT NOT NULL,
    event_kind      TEXT NOT NULL,          -- 'failure' | 'correction' | 'boundary' | 'snapshot'
    payload_json    TEXT NOT NULL,          -- sanitised hook payload (see below)
    UNIQUE (ts, hook_id, session_id)
);
CREATE INDEX idx_captured_events_session ON captured_events(session_id);
CREATE INDEX idx_captured_events_kind    ON captured_events(event_kind, ts);
```

## Privacy: what's stored

**By default, only metadata** — never the raw prompt text, never tool
stdout/stderr. Concretely:

- `failure`: hook event name, tool name, exit code, cwd, and a *single-line*
  truncated error excerpt if one is obviously available. Not the command, not the
  output.
- `correction`: hook event name, the keyword that matched, the prompt's length,
  cwd. **Not the prompt text.**
- `boundary` / `snapshot`: hook event name, `stop_hook_active` / `trigger`, cwd,
  and a small session-totals rollup (message count + token sums + cost if the
  `session_mart` has it) queried from the store — not from the payload.

If you *want* the full, unsanitised payload (prompt text, tool output included),
install with `--capture-content`. That's the explicit, non-default choice; the
installed hook commands then carry the `--capture-content` flag so the handler
keeps everything.

## CLI

```
stackunderflow hooks install   [--scope project|user] [--dry-run] [--capture-content]
stackunderflow hooks uninstall [--scope project|user]
stackunderflow hooks status    [--scope project|user] [--format text|json]
stackunderflow hooks repair    [--scope project|user|all] [--dry-run]
stackunderflow hooks run <hook-id>     # internal — Claude Code calls this; reads the payload on stdin
```

**Scope** is explicit and never implicitly broadened:

- `project` (default) — `.claude/settings.json` in the git root of your current
  directory (or the cwd itself if it's not a git repo).
- `user` — `~/.claude/settings.json`.
- `all` — *repair only* — walk `$HOME` for every project's `.claude/settings.json`.

### install

Merges StackUnderflow's hook entries into the target `settings.json`. It is
**idempotent and convergent**: re-running it lands on exactly the config the
current flags describe — a stale entry (e.g. an old hardcoded path) or a changed
`--capture-content` choice is *replaced*, never duplicated. Before any mutation it
writes `settings.json.bak.<utc-timestamp>` next to the file. It never creates a
backup on a no-op re-install or under `--dry-run`. **It never touches another
tool's hooks** — your `PreToolUse` guard, your formatter, your notifier: all left
exactly as found.

`--dry-run` prints the `hooks` block it *would* write and changes nothing.

### uninstall

Removes only the entries StackUnderflow recognises as its own (by the
`stackunderflow-<event>` id token in the command). It never deletes the file and
never touches other entries. A backup is written first — but only if the file
actually changes.

### status

Shows which StackUnderflow hooks are installed, in which scope, whether any are
*stale* (a recognisably-ours command that isn't in the canonical portable form),
and how many *other* hook entries the file carries. `--format json` for a stable
machine-readable shape.

### repair

If a hook command ever ends up with a stale absolute path
(`/old/venv/bin/stackunderflow hooks run …` after a venv move) or the legacy
singular `hook run` spelling, `repair` rewrites just that `command` string back to
the portable `stackunderflow hooks run <id>` form (preserving `--capture-content`
if it was there). It **changes nothing else** — no hooks added or removed, every
non-StackUnderflow entry untouched, a per-file backup written first, and
`--dry-run` reports without writing.

`--scope all` walks `$HOME`. That walk is bounded and conservative: it never
follows symlinks, it's depth-limited (≤ 8 directory levels below `$HOME`), and it
prunes the usual heavy / irrelevant trees (`node_modules`, `.git`, `.npm`,
`.cache`, `.nvm`, build/dist/target dirs, virtualenvs, caches, macOS `Library`,
…). It is **only ever run when you ask for it** — never from a postinstall, never
automatically.

## Why hook commands are portable, not absolute paths

The installed command is always `stackunderflow hooks run <id>` — the bare
`stackunderflow` binary, resolved on `PATH` — never `/path/to/some/venv/bin/...`.
That survives a venv move, a `pipx reinstall`, a Python upgrade. The cost: the
`stackunderflow` binary must be on the `PATH` that Claude Code uses when it runs
the hook. `install` warns if it can't find `stackunderflow` on your `PATH`. If
hooks silently stop firing after an environment change, `stackunderflow hooks
status` will show them as stale and `stackunderflow hooks repair` will fix the
command form (though if the binary genuinely isn't on `PATH` anymore you'll need
to make it resolvable — that's the one thing `repair` can't do for you).

## Performance notes (read before installing widely)

- Each hook fire is a **separate process** Claude Code spawns
  (`stackunderflow hooks run …`). Python interpreter startup + imports dominates —
  expect a couple hundred milliseconds per fire, not microseconds. That's why
  `PostToolUse` is scoped to `Bash` only (the tool whose exit code is the clean
  failure signal) rather than firing after *every* tool call: it keeps the
  per-tool-call overhead off your hot path. `UserPromptSubmit` fires once per
  prompt, `Stop` once per turn, `PreCompact` rarely — all tolerable.
- The handler *itself* (once the process is up) is cheap: one
  `CREATE TABLE IF NOT EXISTS` (a no-op after the first fire), at most one indexed
  `SELECT` for the session-totals snapshot, one `INSERT OR IGNORE`. No marts
  refresh, no schema migration, no ingest. Comfortably under a 50 ms budget.
- The handler **never disrupts Claude Code**: a malformed payload, a locked store,
  a missing table — all swallowed, exit code always `0`. It's a tape recorder, not
  a gate. If you installed hooks before ever running the dashboard, the handler
  creates `captured_events` itself on first fire (without touching `user_version`
  or any other table).
- The hook handler and the background filesystem watcher both write to the SQLite
  store. WAL mode handles concurrent writers; the handler sets a short
  `busy_timeout` so a fire under contention waits briefly rather than dropping the
  event.

## Only Claude Code, for now

This feature wires **Claude Code's** hook system. Other coding agents
StackUnderflow ingests (Codex, Cursor, Cline, the beta providers) don't expose an
equivalent lifecycle-hook mechanism, so there's nothing analogous to install for
them — those providers stay on passive transcript ingestion. Outcome-aware
discovery accounts for this: hook-installed Claude Code projects get deterministic
failure/correction signals; everything else gets the transcript heuristic. Same
surface, different fidelity.

## Windows

The install / uninstall / status / repair logic is plain JSON-file manipulation
and Python `os.walk` — portable. The backup-timestamp format
(`settings.json.bak.20260512T120000Z`) is filesystem-safe everywhere (no colons).
The repair walk's symlink avoidance relies on `os.walk(followlinks=False)`, which
is cross-platform. **Caveat:** the symlink-loop test is POSIX-only (Windows
symlink semantics differ and creating them needs elevated privileges), and the
hooks path hasn't been exercised on a real Windows box — treat Windows support as
*written but unverified* until someone runs it there.

# Opt-in Claude Code hooks — capture and injection

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
transcripts, and the consumers that benefit from hook data fall back to a transcript
heuristic. Outcome-aware discovery is the clearest example: the outcome-aware
`memory` commands prefer the deterministic `captured_events` feed but degrade
gracefully without it.

Hooks come in two families. **Capture** hooks — the default — *record* what
happens. **Injection** hooks — added with `--inject` — *feed memory back* into
the live agent before it works; `--inject` also installs the **active-recall**
hook, which runs the same `memory file` lookup you would by hand, right before
an Edit/Write/Bash. This document covers capture first; context injection,
active recall, and the agent-discovery snippet are the last sections.

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
stackunderflow hooks install   [--scope project|user] [--dry-run] [--capture-content] [--inject]
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

**Prefer `project` scope.** A hook entry is just the command `stackunderflow
hooks run <id>`, which works only where the `stackunderflow` binary resolves on
`PATH`. At `project` scope the hooks fire inside that one repo, so the binary
only has to resolve there. At `user` scope they fire in *every* Claude Code
session in every directory — which works only if `stackunderflow` is on `PATH`
everywhere; see [Why hook commands are portable](#why-hook-commands-are-portable-not-absolute-paths).

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

### Version managers are the common trap

If `stackunderflow` was installed into a `pyenv`, `conda`, or `asdf`
environment, the binary resolves only while that environment is selected.
`install` checks the `PATH` of the shell *it* runs in, so it cannot see this —
it reports success, and the hook still fails in any directory that selects a
different environment. Two ways out:

- **`project` scope** in a repo that always selects the environment where
  `stackunderflow` lives — the hook fires only there, so it just works.
- For **`user` scope**, install `stackunderflow` independent of any environment
  manager. `pipx install` puts it in `~/.local/bin`, on `PATH` for every shell.

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

## Context injection (opt-in)

Capture hooks record *what happened*. **Injection hooks** do the opposite — they
read what StackUnderflow already knows and feed it back into the live agent,
before it acts. Injection is opt-in *separately* from capture:

```
stackunderflow hooks install --inject     # capture hooks + 3 injection hooks + the recall hook
stackunderflow hooks install              # capture hooks only (unchanged)
```

`--inject` adds three in-process injection hooks alongside the capture four
(plus the active-recall hook — its own section below):

| Claude Code event | hook id | injects |
|-------------------|---------|---------|
| `SessionStart` | `stackunderflow-inject-session-start` | a digest of recent recorded sessions in this project |
| `UserPromptSubmit` | `stackunderflow-inject-user-prompt` | a past decision whose text overlaps the prompt |
| `PreToolUse` (Edit/Write/MultiEdit) | `stackunderflow-inject-pre-tool-use` | known failure modes for the file about to be edited |

Each handler queries the local store in-process (through the same
`services/discovery.py` engine the `memory` commands use — no new analytics, no
shelling out) and prints Claude Code's context-injection envelope on stdout:

```json
{ "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "<digest text>" } }
```

`hookEventName` is the firing event; `additionalContext` is the text spliced
into the agent's context. This shape was verified against the current Claude
Code hooks reference — `additionalContext` nested under `hookSpecificOutput` is
the documented field for all three of `SessionStart`, `UserPromptSubmit`, and
`PreToolUse`.

### Invariants

Injection inherits the capture hooks' non-negotiables, sharpened:

- **Never disrupt the agent.** Any error — a bad payload, a missing store, a
  locked db, a slow query — produces *empty output* and exit `0`. An injection
  hook that wedges a prompt is a worse bug than no injection. A payload with
  nothing useful to say is the same silent no-op.
- **Token-bounded.** Every injection is hard-clipped to a small budget —
  ~400 tokens for `SessionStart`, ~200 for the other two. An agent's context
  window is not a dumping ground.
- **Fast and read-only.** Each fire is a fresh process running one or two
  indexed `SELECT`s; it never applies the schema and never writes a capture row.
  `PreToolUse` fires often, so its matcher is scoped to the file-editing tools
  (`Edit|Write|MultiEdit`) — it never fires on a Read or a Bash call.

`hooks status`, `uninstall`, and `repair` cover the injection hooks with no
change in contract: `uninstall` removes all of ours, and a convergent
re-`install` *without* `--inject` cleanly drops the injection + recall hooks
again. Injection hooks never carry `--capture-content` — they read the store,
they don't record.

## Active recall (PreToolUse → the `memory` CLI)

The fourth hook `--inject` installs turns the `memory` surface into a live
guardrail. Right before Claude Code runs an **Edit**, **Write**, or **Bash**
tool call, the recall hook asks memory about the file that's about to be
touched — the same lookup you'd run by hand — and warns the agent *before* the
edit if that file has real failure history:

| Claude Code event | hook id | injects |
|-------------------|---------|---------|
| `PreToolUse` (Edit/Write/Bash) | `stackunderflow-pretool-recall` | the file's failure history, via `stackunderflow memory file <path> --json` |

Unlike the three injection hooks above (in-process store reads), the recall
hook **shells the public CLI** — `stackunderflow memory file <path> --json` —
as a subprocess under a hard deadline, and parses the token-bounded
`stackunderflow.memory/1` envelope. That buys two things: the lookup runs the
exact code path agents and humans already use (one contract to trust), and the
deadline is a *process-level* hard stop — a slow store can be killed, not
merely tolerated.

**How the target path is found.** For Edit/Write it's the `file_path` in the
tool call. For Bash, a deliberately light heuristic pulls file-looking tokens
out of the command (`pytest tests/test_auth.py -q` → `tests/test_auth.py`),
skipping flags, URLs, and pseudo-files (`/dev/null`); at most three candidate
paths per command, and a command with no extractable path (`npm run build`,
`git status`) never triggers a lookup at all.

**What gets injected.** Only a *risk signal* warrants output: past sessions
that `failed` or `reverted` after touching this file (the `memory file` risk
block plus its failure-mode rows). A clean file — even one with plenty of
recorded history — injects nothing. When it fires, the block looks like:

```
[StackUnderflow memory] risky.py has failure history (2 failed and 1 reverted
of 7 past sessions touching it). Recent trouble:
  • 2026-05-17  failed: that broke the build — please look again
  • 2026-05-12  reverted: git checkout undid the change
Full history: `stackunderflow memory file /path/to/risky.py --json`.
```

The block is hard-capped at ~600 tokens (the same chars/4 estimate the other
hooks use); over budget, the **oldest** failure lines are dropped first. The
output uses the identical `hookSpecificOutput.additionalContext` envelope as
the injection hooks.

**The no-op guarantees.** This runs inside your live coding session, so the
contract is absolute — the hook always exits `0` and is silent on *anything*
short of a genuine risk signal:

- `stackunderflow` missing from `PATH`, the CLI exiting non-zero, stdout that
  isn't JSON, an envelope schema it doesn't recognise, any exception at all →
  empty output, tool proceeds untouched.
- The lookup runs under one shared wall-clock deadline — **1.5s by default**,
  tunable via `STACKUNDERFLOW_RECALL_TIMEOUT` (seconds). When it expires the
  child process is killed and the hook stays silent. Multiple Bash paths share
  that single deadline; the tool call is never delayed beyond it.
- Nothing is recorded: recall is read-only end to end (the child process is
  the same read-only query `memory file` always is).

**Privacy.** The recall hook adds no new data flow. The child process is a
local, read-only query against your own store; what it injects is drawn from
what `memory file` already returns (outcome labels, dates, short evidence
excerpts) and goes only into the local agent's context. Nothing is written,
nothing is uploaded, nothing leaves the machine.

**Cost.** Each fire is a hook process *plus* a nested CLI process — a few
hundred milliseconds worst case, bounded by the deadline. The matcher scopes
it to `Edit|Write|Bash`, and the Bash heuristic means most shell commands
(no file-looking arguments) skip the lookup entirely.

## Only Claude Code, for now

Every hook family — capture, injection, recall — wires **Claude Code's** hook system.
Other coding agents StackUnderflow ingests (Codex, Cursor, Cline, the beta
providers) don't expose an equivalent lifecycle-hook mechanism, so there's
nothing analogous to install for them — those providers stay on passive
transcript ingestion. Outcome-aware discovery accounts for this: hook-installed
Claude Code projects get deterministic failure/correction signals; everything
else gets the transcript heuristic. Same surface, different fidelity.

The one cross-agent piece is the agent-discovery snippet (next section): its
`AGENTS.md` half is how a Codex agent finds the same `memory` commands.

## The agent-discovery snippet (`stackunderflow guide`)

Hooks are one way memory reaches the agent. The other is plain discoverability:
teaching the agent that the `stackunderflow memory` commands *exist* at all. The
`guide` command writes a short, marked snippet into the agent instruction file:

```
stackunderflow guide install   [--scope project|user] [--dry-run]
stackunderflow guide uninstall [--scope project|user]
stackunderflow guide status    [--scope project|user] [--format text|json]
```

- **Scope.** `project` (default) writes `./CLAUDE.md` *and* `./AGENTS.md` in the
  cwd's git root — so both Claude Code and Codex pick the snippet up. `user`
  scope writes `~/.claude/CLAUDE.md`.
- **Idempotent and convergent.** The snippet sits between
  `<!-- stackunderflow:guide:start -->` and `<!-- stackunderflow:guide:end -->`
  markers, exactly like the hooks installer's settings block: re-running
  `install` replaces the block in place (never a second copy), nothing outside
  the markers is touched, a timestamped backup is written before any mutation,
  and `--dry-run` writes nothing. `uninstall` strips only our block and never
  deletes the file.
- **Content.** ~15 lines naming the `memory` commands
  (`decisions` / `file` / `worked` / `sessions` / `ask`), the `--json` output
  contract, and when to reach for each — the discoverability a bundled MCP
  server would have given for free.

Unlike hooks, the guide snippet is **not Claude-Code-specific**: the `AGENTS.md`
half is how a Codex agent — which has no lifecycle-hook mechanism — discovers
the same `memory` surface.

## Windows

The install / uninstall / status / repair logic is plain JSON-file manipulation
and Python `os.walk` — portable. The backup-timestamp format
(`settings.json.bak.20260512T120000Z`) is filesystem-safe everywhere (no colons).
The repair walk's symlink avoidance relies on `os.walk(followlinks=False)`, which
is cross-platform. **Caveat:** the symlink-loop test is POSIX-only (Windows
symlink semantics differ and creating them needs elevated privileges), and the
hooks path hasn't been exercised on a real Windows box — treat Windows support as
*written but unverified* until someone runs it there.

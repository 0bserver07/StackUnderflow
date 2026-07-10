---
name: resume-agent-sessions
description: Use when the user wants to RESUME or CONTINUE prior coding-agent work — "resume my codex session", "continue where I left off in <project>", "what sessions ran in this folder", "pick up that grok conversation", or any ask for session/resume ids across agents (Claude Code, Codex, Grok, Cursor Agent, …). Surfaces every agent's recent sessions under a path with the exact resume command for each.
---

# Resume any coding agent's session from a path

## Why this skill exists

Every coding agent keeps its own session history with its own resume UX —
`claude --resume`, `codex resume`, `grok --continue` — and none of them can
see the others. StackUnderflow indexes all of them, so one local CLI call
answers "what was running here, in any tool, and how do I get back into it".

## When this skill fires

- "Resume my codex session in ~/dev/foo" / "continue that grok run"
- "What sessions have run in this folder?" (across agents)
- "Give me the resume id for the claude session from Tuesday"
- The user switches projects and wants to pick up prior work in ANY tool.

Do NOT fire when the user wants **this** conversation to recall past context —
that's the `stackunderflow memory` namespace (`decisions` / `file` / `ask`),
not session resumption.

## Step 1: one call

```bash
stackunderflow resume <path> --json
```

`<path>` defaults to cwd. Matching is bidirectional: standing inside a project
finds it, and a workspace folder (e.g. `~/dev/my_workspace/`) lists every
project underneath it. Underscore/hyphen path quirks are handled (matching is
slug-space).

Envelope (`stackunderflow.resume/1`):

```json
{
  "schema": "stackunderflow.resume/1",
  "path": "/abs/path",
  "providers": [
    {"provider": "codex",
     "resume": {"command": "codex resume {session_id}", "scope": "session"},
     "sessions": [
       {"session_id": "…", "last_ts": "…", "message_count": 151,
        "project": "-slug", "project_path": null,
        "resume_command": "codex resume 019f…"}
     ]}
  ]
}
```

## Step 2: act on the shape

- `resume.scope == "session"` → each session carries a ready
  `resume_command`. Present it to the user (with when/size context so they
  pick the right one).
- `resume.scope == "latest"` (e.g. grok) → there is no per-id resume; tell the
  user to run the provider-level command **from that project's directory**.
- `resume == null` → the tool has no verified resume command for that agent;
  give the session id and never invent a flag.

## Rules

- **Present commands; don't execute them.** Resume commands launch interactive
  TUIs — running one inside an agent session hangs it. Hand the command to the
  user (or write it to their terminal input if the harness supports that).
- All data is local SQLite, read-only, <100 ms. Run liberally.
- Human-readable form (no `--json`) is fine to paste verbatim when the user
  just asks "what can I resume here?".

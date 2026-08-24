---
name: staxtrace-memory
description: Search local coding-agent session history before acting. Use BEFORE an Edit or Write to a file, BEFORE a non-trivial Bash command (a build, migration, or destructive/long-running command), and whenever the user references a past decision ("what did we decide about X", "why is this done this way", "didn't we agree to Y"), a specific file, or "what worked last time" — cases where a prior session on this machine may already hold the decision, the failure mode, or the exact working command. Queries the local `stax memory` CLI (staxtrace) across every past Claude Code / Codex / Cursor session so you do not re-derive an answer, contradict an earlier choice, or repeat a known failure. Local and read-only; nothing leaves the machine.
---

# staxtrace — query your past coding sessions before acting

This machine indexes every past coding-agent session locally with staxtrace.
The sessions are stateless; the history is not. A decision, a failure mode, or the
exact command you need may already be recorded. Before re-deriving something,
check whether the answer is already there.

## When to reach for this

Check history **before** you act, whenever a prior session plausibly knows something:

- **Before you Edit or Write a file** — run `stax memory file <path>` first. Past
  failure modes on that file ("broke CI the last 3 of 5 times it was touched") and
  who changed it and why are the highest-signal context for a non-trivial edit.
- **Before a non-trivial Bash command** — a build, a migration, anything
  destructive or long-running — `stax memory worked "<action>"` shows whether it
  succeeded before and what the working invocation was.
- **When the user references a past decision** — "what did we decide about
  caching?", "why is X done this way?", "didn't we agree to Y?" — `stax memory
  decisions "<topic>"` finds the session where it was decided.
- **When picking up work in a repo** — `stax memory sessions` lists recent sessions
  here so you neither duplicate nor contradict them.
- **For an open-ended recall** — `stax memory ask "<question>"` runs a
  natural-language search over the whole history.

Skip it for pure read/explain questions about the current code, greenfield work
with no history, or when the user already gave you the answer this turn. The check
is cheap (local SQLite, no network) and an empty result is itself a useful signal.

## The command menu

```
stax memory decisions "<topic>"   # past decisions on a topic
stax memory file <path>           # a file's history: failure modes, who touched it, risk
stax memory worked "<action>"     # past sessions where an action succeeded, with evidence
stax memory sessions [path]       # recent sessions in this project (or touching a file)
stax memory ask "<question>"      # natural-language query over all history
```

Narrowing flags on every command: `--since 30d` (tighten the window), `--limit N`
(cap results), `--project <slug>` (scope). `--project` defaults to the current
directory's project, so inside a repo these Just Work.

`stax` is the short command. The long form `stax memory …` is identical
and works anywhere `stax` is not on PATH.

## Prefer text output; `--json` is for scripts only

Run these commands **without** `--json`. The default text output is compact and
readable — meant to be scanned by you, the agent. **JSON output is large and can
consume the context window**: it is the stable, token-bounded envelope
(`schema: stackunderflow.memory/1`) for a *script or hook* that parses the result
programmatically, not for splicing into a conversation. Reach for `--json` only
when something downstream actually parses the output.

## Cite what you surface

When a result changes your answer, or you report it to the user, cite it: the
**session id** and its **provider** (claude / codex / cursor), plus the date. A
recall the user can verify against the linked session is trustworthy; an uncited
paraphrase reads as a guess. For example: "On 2026-04-25 (codex session
`def456…`) the prior session landed on SQLite because [snippet] — proceeding on
that basis."

## Safety

- **Do not overclaim.** Report a decision as made only if the surfaced snippet
  actually says so. If the search comes up empty, say so and ask — never fabricate
  a prior decision or a past outcome to fill the gap.
- **Do not paste secrets or large payloads.** Surface the short excerpt you need;
  do not dump raw session logs, tokens, keys, or large result blobs into the
  conversation.
- **The store is private and local.** `~/.stackunderflow` is this machine's local
  index. Every `stax memory` query is read-only and nothing leaves the machine —
  do not write to it and do not exfiltrate its contents.

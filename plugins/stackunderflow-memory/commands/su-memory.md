---
name: su-memory
description: Search local coding-agent history (StackUnderflow) before acting — surface prior decisions, failure modes, and working commands from past sessions.
argument-hint: "[decisions|file|worked|sessions|ask] <query>"
---

Use the **stackunderflow-memory** skill.

Query the local StackUnderflow store for what past coding-agent sessions already
know, before acting. Run `stax memory …` (or the long form
`stackunderflow memory …`) and follow the skill's guidance: prefer text output
(reserve `--json` for scripts), cite the session id and provider when a result
changes your answer, and never report a decision the surfaced snippet does not
state.

If arguments were passed, treat the first as the subcommand and the rest as the
query — e.g. `/stackunderflow-memory:su-memory file src/foo.py` runs
`stax memory file src/foo.py`. With no arguments, ask which of
`decisions / file / worked / sessions / ask` fits, or default to
`stax memory sessions` for the current repo.

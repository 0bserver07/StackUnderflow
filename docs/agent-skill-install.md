# The `stackunderflow-memory` agent skill

StackUnderflow's `memory` CLI lets a coding agent ask the local store what past
sessions already know — a decision, a failure mode, the exact working command —
*before* it re-derives the answer or repeats a mistake. For the agent to reach
for it at the right moment, the agent has to know it exists and when to use it.

This is that discovery layer as a **frontmatter-triggered Skill** plus **thin
per-host plugins**, so the same guidance rides into every agent that supports
skills — Claude Code, Codex, and Cursor — from one synced source.

> This is a sibling to the three shipped skills documented in
> [`docs/skills.md`](skills.md). Those teach the older top-level discovery
> commands; this one teaches the consolidated `memory` namespace (`stax memory …`)
> and is packaged as an installable, cross-host plugin.

## What ships

| Path | What it is |
|---|---|
| `skills/stackunderflow-memory/SKILL.md` | **The canonical skill.** Frontmatter `description` is the auto-trigger surface; the body is the command menu + rules. The single source of truth. |
| `plugins/stackunderflow-memory/` | A self-contained plugin: per-host manifests, a `/su-memory` command delegator, and a byte-identical mirror of the skill under `skills/`. |
| `plugins/.{claude,codex,cursor}-plugin/marketplace.json` | Root marketplace manifests (the `plugins/` directory is the marketplace root). |
| `scripts/sync_plugin_skills.py` | Keeps the plugin's bundled skill copy byte-identical to the canonical one (`--check` / `--write`). |

The canonical skill and the plugin's mirror are **byte-identical** by
construction — see [Keeping the copies in sync](#keeping-the-copies-in-sync).

## The short command

The skill teaches `stax memory …` (the short alias) and notes the long form
`stackunderflow memory …` works anywhere `stax` is not on `PATH`. The subcommands:

```
stax memory decisions "<topic>"   # past decisions on a topic
stax memory file <path>           # a file's history: failure modes, who touched it, risk
stax memory worked "<action>"     # past sessions where an action succeeded
stax memory sessions [path]       # recent sessions in this project
stax memory ask "<question>"      # natural-language query over all history
```

## Install

### Claude Code — as a plugin (recommended)

Claude Code has first-class plugin + marketplace support. Point it at the
`plugins/` directory (the marketplace root) and install the plugin:

```
/plugin marketplace add /path/to/StackUnderflow/plugins
/plugin install stackunderflow-memory@stackunderflow
```

The plugin bundles the skill (auto-discovered from its `skills/` directory) and a
`/stackunderflow-memory:su-memory` command. Restart or start a fresh session and
the skill's `description` joins the trigger surface Claude Code evaluates each turn.

### Claude Code — skill only (no plugin)

If you would rather not use the plugin system, drop the canonical skill straight
into the skills directory Claude Code reads:

```bash
mkdir -p ~/.claude/skills/stackunderflow-memory
cp skills/stackunderflow-memory/SKILL.md ~/.claude/skills/stackunderflow-memory/
```

User scope (`~/.claude/skills/`) applies to every project on this machine — the
right scope here, since the store is per-machine anyway. Project scope
(`<repo>/.claude/skills/`) works too and overrides user scope for the same name.

### Codex and Cursor

Both read agent guidance from an instruction file (`AGENTS.md` / project rules)
and, where supported, a skill file. Two paths:

1. **The skill file.** Copy `skills/stackunderflow-memory/SKILL.md` into the
   location your host reads skills from (e.g. a project or user skills directory).
   The body is host-agnostic — it only teaches the `stax memory` CLI.
2. **The instruction-file snippet.** `stackunderflow guide install` writes a
   short, marked, idempotent block into `AGENTS.md` (and `CLAUDE.md`) teaching the
   `memory` commands — the always-available fallback that needs no plugin system.
   See [Move 4 in the memory-CLI spec](specs/agent-memory-cli.md).

The `.codex-plugin/` and `.cursor-plugin/` manifests carry the same plugin
metadata as the Claude manifest, host-namespaced, for hosts that consume a plugin
manifest. The guaranteed-to-work path on any host, though, is one of the two
above: install the skill where the host reads skills, or add the snippet and let
the agent call `stax memory` directly. Same engine, same output contract — only
the discovery wiring differs per host.

## Prerequisites

The skill assumes the CLI is on `PATH` and the local store is populated:

```bash
which stax || which stackunderflow      # the CLI is installed and on PATH
stackunderflow etl status               # usage_events count > 0
```

If the store is empty, run `stackunderflow etl backfill` first. Every `memory`
query is local (a SQLite read) — no network, no LLM call for the structured
subcommands — so the skill is always cheap to fire, and an empty result is itself
a useful signal.

## Keeping the copies in sync

The plugin bundles its own copy of `SKILL.md` so the plugin directory is
self-contained. That copy must never drift from the canonical
`skills/stackunderflow-memory/SKILL.md`. One script owns the relationship:

```bash
# assert the plugin mirror is byte-identical to the canonical source (CI + test)
python scripts/sync_plugin_skills.py --check

# regenerate the mirror after editing the canonical SKILL.md
python scripts/sync_plugin_skills.py --write
```

Edit the **canonical** file, then run `--write`. `tests/stackunderflow/test_plugin_skill_sync.py`
runs `--check` in CI, so an edit to one copy but not the other fails the build.

## Design rules baked into the skill

- **Prefer text output; `--json` is for scripts.** The default text form is
  compact and meant for the agent to read. The JSON envelope
  (`schema: stackunderflow.memory/1`) is large and can consume the context window
  — it exists for a script or hook that parses the result, not for splicing into a
  conversation.
- **Cite what you surface.** Report a result with its session id + provider
  (claude / codex / cursor) and date, so the user can verify it against the linked
  session.
- **Don't overclaim, don't leak.** Report a decision as made only if the surfaced
  snippet says so; never paste secrets or large payloads; treat `~/.stackunderflow`
  as this machine's private, read-only index.

## Validation

```bash
pytest tests/stackunderflow/test_plugin_skill_sync.py -q
```

Checks the sync guard (mirror byte-identical to canonical, and that the guard
bites on drift), every plugin + marketplace manifest (well-formed JSON, host-shape
fields, `source` resolving to the plugin directory), and the canonical skill's
trigger surface + behavioural rules.

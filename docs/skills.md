# Claude Code skills

StackUnderflow ships **Claude Code skills** — markdown files that teach Claude Code *when* to invoke StackUnderflow's discovery commands. With these installed, Claude Code automatically surfaces prior session context at the right moments: starting work in a known project, touching a specific file, or recalling a past decision. The store stops being passive and becomes a reflex.

> **What's a skill?** A skill is a directory under `~/.claude/skills/<name>/` containing a `SKILL.md` file with YAML frontmatter (`name`, `description`) and a markdown body. Claude Code reads the descriptions of every installed skill at session start and decides which to invoke based on the user's request. Skills are documented at <https://code.claude.com/docs/en/skills>.

## What ships

Three skills, one directory each, under `stackunderflow/skills/` in this repo:

| Skill | Trigger | What it runs |
|---|---|---|
| `check-prior-work` | First substantive coding task in a project | `stackunderflow find-sessions-in-path "$(pwd)" --format json --limit 5 --since 30d` |
| `find-related-sessions` | User mentions a specific file path | `stackunderflow find-sessions-touching-file <path> --format json --limit 5 --mode any` |
| `recall-past-decisions` | User references a past decision or rationale | `stackunderflow search-past-decisions "<query>" --format json --limit 10` |

Each skill body documents both the JSON form (for the agent to consume internally) and the human-readable `--format text` form (for when the user asks Claude Code to *show* them the result).

## Where Claude Code looks for skills

Claude Code discovers skills from two locations, in order:

1. **User-level**: `~/.claude/skills/<name>/SKILL.md` — applies to every Claude Code session on this machine. **Recommended for StackUnderflow skills** since the store is per-machine anyway.
2. **Project-level**: `<repo>/.claude/skills/<name>/SKILL.md` — applies only when Claude Code is run from inside that repo. Useful for project-specific skills, but redundant for StackUnderflow's, which apply to every project.

Project-level skills override user-level skills with the same name.

## Install (today, manual)

Until `stackunderflow init --install-skills` ships (see [Future work](#future-work)), copy them by hand:

```bash
# from the repo root
mkdir -p ~/.claude/skills
cp -r stackunderflow/skills/* ~/.claude/skills/
```

Verify the install:

```bash
ls ~/.claude/skills/check-prior-work/SKILL.md
ls ~/.claude/skills/find-related-sessions/SKILL.md
ls ~/.claude/skills/recall-past-decisions/SKILL.md
```

Restart Claude Code (or start a fresh session) and the skills are picked up. Confirm by checking the skill list at session start (Claude Code prints available skills when running with verbose output, or skills are listed in the system prompt context).

### Prerequisites

The skills assume `stackunderflow` is on `PATH` and the local store is populated. If you haven't already:

```bash
# Install the package (editable for dev, or pip install for production)
pip install -e .

# Populate the store from existing Claude / Codex / Cursor session files
stackunderflow etl backfill
```

Once `stackunderflow etl status` shows your usage_events count > 0, the skills have data to query.

## Example transcripts

### `check-prior-work` in action

```
User: Add a /api/projects/<slug>/timeline endpoint that returns events ordered by timestamp.

[Claude Code recognizes this as new feature work in an existing project, fires check-prior-work]

[Claude Code runs: stackunderflow find-sessions-in-path "$(pwd)" --format json --limit 5 --since 30d]

Claude Code: Before I dive in, I checked recent sessions on this project:
  - 2026-05-04: "add session export to /api/sessions" (merged)
  - 2026-05-02: "wire up timeline view in frontend" — interesting, this looks
    related. The session worked on the UI side; the backend route doesn't
    exist yet, which matches your request.

I'll proceed with the new endpoint. The existing /api/sessions/<id>/messages
helper has a similar shape — I'll model the timeline route after it.
```

The user gets free continuity: prior frontend work surfaces, the agent uses the same patterns as the prior session, and there's no risk of duplicating effort.

### `find-related-sessions` in action

```
User: Why is stackunderflow/store/queries.py so long? Can we split it up?

[Claude Code recognizes a specific file reference, fires find-related-sessions]

[Claude Code runs: stackunderflow find-sessions-touching-file stackunderflow/store/queries.py --format json --limit 5 --mode any]

Claude Code: I checked the recent sessions on queries.py. The last write was
on 2026-04-30 (session abc123…) — the "Wave 4 mart migration" PR added
the bulk_get_session_costs and bulk_project_summaries helpers there.

The file is long because it's the single SQL home for every read path —
that's a deliberate choice from that session ("one place for all SQL"
in the docstring). Splitting it would mean picking a split axis (by
mart? by route?) and breaking the convention.

Worth talking through before I refactor. What's prompting the question
— readability, test isolation, or something else?
```

The agent doesn't blindly refactor — the prior session's intent surfaces and the user gets a chance to confirm before the new direction overrides the old one.

### `recall-past-decisions` in action

```
User: Wait, didn't we decide to keep watchfiles instead of inotify?

[Claude Code recognizes a past-decision reference, fires recall-past-decisions]

[Claude Code runs: stackunderflow search-past-decisions "watchfiles inotify" --format json --limit 10]

Claude Code: Yes — on 2026-04-25 (session def456…) you and the prior session
landed on watchfiles. The reasoning recorded was:

  "watchfiles is Rust-backed and cross-platform; raw inotify would mean
   a separate macOS path via FSEvents. The watcher is non-critical-path
   so the slight overhead is fine."

So I'll stick with watchfiles for the new feature. Cancel my last suggestion
to swap in inotify.
```

The agent corrects itself with a verifiable citation. If the recall is wrong, the user can open the linked session and check.

## Optional: project-specific overrides

If you want different behavior per repo (e.g. tighter `--since` window for a fast-moving project), drop a project-level override:

```bash
mkdir -p .claude/skills
cp -r ~/.claude/skills/check-prior-work .claude/skills/
# edit .claude/skills/check-prior-work/SKILL.md to taste
```

Project-level wins over user-level for the same skill name.

## Future work

A future `stackunderflow init --install-skills` command will automate the install (copy skills, idempotently, with `--force` for upgrades). Until that ships, the manual `cp` above is the supported path. Tracking issue: ship in the next release after the discovery CLI lands.

## Validation

The repo has a smoke test (`tests/test_skills.py`) that verifies each shipped `SKILL.md` parses as valid YAML frontmatter + non-empty body. Run it after editing skill files:

```bash
pytest tests/test_skills.py -q
```

## Troubleshooting

**Skill descriptions not appearing in Claude Code's skill list.** Check the file lives at `~/.claude/skills/<name>/SKILL.md` (not `~/.claude/skills/<name>.md`). The directory layout is required.

**Skill fires but the CLI command fails.** `stackunderflow` not on `PATH`, or the store is empty. Run `which stackunderflow` and `stackunderflow etl status`. If the store has zero events, run `stackunderflow etl backfill` first.

**Skill never fires.** The `description` frontmatter is the trigger surface — Claude Code reads it and decides. If the skill is too narrow ("only use when user says X verbatim"), it won't fire on paraphrases. The shipped descriptions are tuned with example phrasings; if you customize, keep them example-driven.

**Wrong skill fires.** Two skills with overlapping triggers can both match; Claude Code picks one. Tighten the descriptions to disambiguate (e.g. add explicit "Do NOT fire for X" sections in the body).

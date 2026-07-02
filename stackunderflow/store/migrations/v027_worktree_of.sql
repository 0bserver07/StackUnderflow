-- v027: ``projects.worktree_of`` — attribute worktree fragment projects to
-- their parent project.
--
-- Parallel agents run in git worktrees (``<repo>/.claude/worktrees/<name>``,
-- ``<repo>/.worktrees/<name>``). Claude Code derives its project slug from the
-- session cwd (every non-alphanumeric character mangled to ``-``), so a
-- session run inside a worktree lands under a PHANTOM SIBLING project —
-- ``<parent-slug>--claude-worktrees-<name>`` / ``<parent-slug>--worktrees-<name>``
-- — instead of the repo's own project. On the maintainer's machine four such
-- "projects" hold real sessions and cost that every per-project surface counts
-- separately (see ``docs/campaigns/intelligence-layer.md`` #8).
--
-- ``worktree_of`` holds the PARENT project slug on fragment rows so consumers
-- can roll a fragment's analytics up into its parent (and drop the phantom
-- sibling from Overview). NULL = a normal project — the default for every
-- existing and future row until ``services.worktrees.attribute_fragments``
-- stamps it (idempotent, re-runnable, pure slug-shape matching).
--
-- Migration is **additive** — one nullable ``ALTER TABLE ADD COLUMN``, no
-- backfill here (attribution is service-driven so it can re-run as new
-- fragments are ingested). Idempotency-guarded by ``schema.py``'s
-- ``_ADD_COLUMN_GUARDS`` ``("projects", "worktree_of")`` entry so a partial
-- prior run (column added, ``user_version`` not bumped) recovers cleanly
-- instead of erroring on "duplicate column".

BEGIN;

ALTER TABLE projects ADD COLUMN worktree_of TEXT;

PRAGMA user_version = 27;

COMMIT;

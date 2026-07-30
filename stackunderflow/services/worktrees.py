"""Worktree intelligence — detect, attribute, and preview-prune git worktrees.

Parallel agents leave worktrees behind, and the sessions they run inside them
fragment per-project analytics into phantom sibling projects (Claude Code
derives the project slug from the session cwd, so ``<repo>/.claude/worktrees/x``
becomes the separate project ``<repo-slug>--claude-worktrees-x``). This module
makes the store know every worktree: which repo owns it, what it cost, whether
its work landed, and whether pruning is safe.

Three jobs, all **read-only against git**:

1. **Detect** — :func:`list_worktrees` runs ``git worktree list --porcelain``
   ONCE per repository — never per session (the yield-route lesson: per-session
   git fan-out timed the route out on real-store-shape projects, see
   ``services/yield_tracker.py``). Candidate repos come from the store's most
   recent distinct session cwds (bounded), or a single ``project_root`` when
   the caller already knows the repo.
2. **Attribute** — :func:`attribute_fragments` stamps ``projects.worktree_of``
   (v027) on every projects row whose slug matches a known worktree shape, so
   consumers can roll fragment analytics up into the parent project.
   :func:`is_worktree_slug` is the pure shape test.
3. **Hygiene verdicts** — per worktree: branch, HEAD, age, dirty-file count,
   unique commits vs the default branch (``git cherry``), attributed sessions +
   cost, and a conservative-first verdict:

   * ``ACTIVE`` — the worktree directory saw activity (mtime) in the last 48h.
     Activity wins over everything else.
   * ``HAS_UNIQUE_WORK`` — unique commits > 0 OR dirty files > 0, **or any git
     error** (a failure never becomes "safe"; it degrades here with a note).
   * ``MERGED_SAFE_TO_PRUNE`` — ONLY when unique commits == 0 AND dirty files
     == 0 and every git probe succeeded.

Design contract:

* Every git call is an explicit argv list (no shell), ≤ 5s timeout, and every
  exception is caught — a missing git binary, a non-repo dir, or a timeout
  degrades (skip / conservative verdict), never raises.
* NEVER a mutating git command. A single allowlisted chokepoint
  (:func:`_run_git`) refuses anything outside ``worktree list`` / ``status`` /
  ``cherry`` / ``rev-parse`` / ``symbolic-ref``, and ``--no-optional-locks``
  keeps even ``status`` from touching the index. Prune output is preview
  strings only — this module never deletes git state.
* Sessions/cost attribution reads the store by **fragment slug**, not by
  per-cwd message scans — rationale on :func:`_fragment_rollup`.
"""

from __future__ import annotations

import logging
import re
import shlex
import sqlite3
import subprocess
import time
from collections.abc import Sequence
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

__all__ = [
    "WorktreeInfo",
    "VERDICT_ACTIVE",
    "VERDICT_HAS_UNIQUE_WORK",
    "VERDICT_MERGED_SAFE_TO_PRUNE",
    "attribute_fragments",
    "is_worktree_slug",
    "list_worktrees",
]

# ── tunables ────────────────────────────────────────────────────────────────

# 5s is enough for any local git query against a healthy repo and short enough
# that a hung repo (NFS, lock contention) can't stall a report — same posture
# as services/yield_tracker.py.
_GIT_TIMEOUT_SECONDS = 5

# A worktree whose directory mtime is within this window is ACTIVE regardless
# of its merge/dirty state — never suggest pruning something in use.
_ACTIVE_WINDOW_HOURS = 48.0

# Bounds on the store-driven repo discovery (``project_root=None``): scan the
# most recent N sessions' cwds, keep at most M distinct cwds to git-probe.
# Repo roots only need ONE session cwd each to surface, and `git worktree
# list` then finds every worktree of that repo regardless of session recency,
# so recent-session bounding loses only repos with no sessions at all in the
# window.
_MAX_SESSIONS_SCANNED = 500
_MAX_DISTINCT_CWDS = 50

# Verdict vocabulary (string contract shared with routes / CLI / FE).
VERDICT_ACTIVE = "ACTIVE"
VERDICT_MERGED_SAFE_TO_PRUNE = "MERGED_SAFE_TO_PRUNE"
VERDICT_HAS_UNIQUE_WORK = "HAS_UNIQUE_WORK"

# Slug shapes produced by Claude Code's cwd mangling (every non-alphanumeric
# character → '-'):
#   <repo>/.worktrees/<name>        → <parent-slug>--worktrees-<name>
#   <repo>/.claude/worktrees/<name> → <parent-slug>--claude-worktrees-<name>
# The double dash comes from the '/.' in the path, which is what keeps a
# genuine directory literally named "worktrees" (single dash) from matching.
_WORKTREE_SLUG_MARKERS: tuple[str, ...] = ("--claude-worktrees-", "--worktrees-")

_SLUG_MANGLE_RE = re.compile(r"[^A-Za-z0-9]")

# The complete set of git invocations this module is allowed to make. All of
# them are read-only. ``_run_git`` refuses (returns None for) anything else so
# the module can never grow a mutating call unnoticed — tests pin this.
_ALLOWED_GIT_PREFIXES: tuple[tuple[str, ...], ...] = (
    ("worktree", "list"),
    ("status", "--porcelain"),
    ("cherry",),
    ("rev-parse",),
    ("symbolic-ref",),
)


# ── public dataclass ────────────────────────────────────────────────────────


@dataclass
class WorktreeInfo:
    """One linked git worktree, with hygiene verdict and store attribution.

    ``verdict`` is one of ``ACTIVE`` / ``MERGED_SAFE_TO_PRUNE`` /
    ``HAS_UNIQUE_WORK``. ``prune_commands`` are preview strings only — they
    are NEVER executed by this module. ``note`` (additive field) carries the
    reason for a conservative degrade (git error, prunable/locked flags).
    """

    path: str
    branch: str | None
    head: str | None
    parent_repo: str | None
    parent_slug: str | None
    dirty_count: int
    unique_commits: int
    age_days: float | None
    verdict: str
    sessions: int
    cost_usd: float
    prune_commands: list[str] = field(default_factory=list)
    note: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ── pure slug logic (no I/O) ────────────────────────────────────────────────


def is_worktree_slug(slug: str) -> str | None:
    """Return the parent project slug when *slug* has a known worktree shape.

    PURE function — string logic only, no I/O. Recognised shapes (both
    observed on real machines; the mangling is Claude Code's ``/`` → ``-``,
    ``.`` → ``-``):

    * ``<parent>--worktrees-<name>``         (from ``<repo>/.worktrees/<name>``)
    * ``<parent>--claude-worktrees-<name>``  (from ``<repo>/.claude/worktrees/<name>``)

    Returns ``None`` for anything else — in particular a slug where
    ``worktrees`` is a genuine directory name (``-Users-x-worktrees-app`` has a
    single dash before ``worktrees`` and does not match). When markers nest
    (a worktree inside a worktree) the LEFTMOST marker wins, so the returned
    parent is the ROOT repo slug — attribution folds cost into the real
    project, and the projects roll-up only folds into parents that are not
    themselves fragments (matching ``routes/projects.py`` semantics).
    """
    if not slug:
        return None
    best_idx: int | None = None
    for marker in _WORKTREE_SLUG_MARKERS:
        idx = slug.find(marker)
        if idx > 0 and slug[idx + len(marker):]:
            best_idx = idx if best_idx is None else min(best_idx, idx)
    if best_idx is None:
        return None
    return slug[:best_idx]


def _path_to_slug(path: str) -> str:
    """Filesystem path → Claude Code project slug (non-alphanumeric → '-').

    Matches the observed real-world mangling: ``/Users/x/dev_dev/proj`` →
    ``-Users-x-dev-dev-proj`` and ``<repo>/.claude/worktrees/a`` →
    ``<repo-slug>--claude-worktrees-a``. Trailing path separators are stripped
    so ``/repo/`` and ``/repo`` mangle identically.
    """
    return _SLUG_MANGLE_RE.sub("-", str(path).rstrip("/\\"))


# ── store attribution ───────────────────────────────────────────────────────


def attribute_fragments(conn: sqlite3.Connection) -> int:
    """Stamp ``projects.worktree_of`` on every row whose slug is worktree-shaped.

    Pure slug-shape matching via :func:`is_worktree_slug` — the parent slug is
    recorded even when no parent project row exists yet (it may be ingested
    later, or live under another provider); consumers join on the slug and
    simply find nothing until then. Matching is provider-agnostic: the
    fragment shapes are produced by Claude Code's cwd mangling, and a
    same-shaped slug under another provider is the same fragmentation problem.

    Idempotent: only rows whose ``worktree_of`` differs from the computed
    parent are updated, so a second run returns 0. Returns the number of rows
    updated. Degrades to 0 (never raises) on a store that predates v027 or on
    any SQL error.
    """
    try:
        if not _column_exists(conn, "projects", "worktree_of"):
            return 0
        rows = conn.execute("SELECT id, slug, worktree_of FROM projects").fetchall()
    except sqlite3.Error as e:
        logger.debug("worktrees: attribute_fragments read failed: %s", e)
        return 0

    updated = 0
    for row in rows:
        project_id = int(row[0])
        slug = str(row[1] or "")
        current = row[2]
        parent = is_worktree_slug(slug)
        if parent is None or current == parent:
            continue
        try:
            conn.execute(
                "UPDATE projects SET worktree_of = ? WHERE id = ?",
                (parent, project_id),
            )
            updated += 1
        except sqlite3.Error as e:
            logger.debug("worktrees: worktree_of update failed for %s: %s", slug, e)
    if updated:
        try:
            conn.commit()
        except sqlite3.Error:  # autocommit connections have nothing to commit
            pass
    return updated


def _fragment_rollup(
    conn: sqlite3.Connection | None, worktree_path: str
) -> tuple[int, float]:
    """Sessions + cost attributed to *worktree_path*, via its fragment slug.

    HOW (and why not per-cwd): a session's ``cwd`` lives only inside
    ``messages.raw_json`` — matching "sessions whose cwd is under the worktree
    path" would be a ``json_extract`` scan over the partitioned ``messages``
    view, the exact unbounded pattern the yield-route fix removed (and
    ``session_mart.cwd`` is NULL by design in v1). Instead we use the fact
    that Claude Code *already* buckets worktree sessions into a fragment
    project whose slug is the mangled worktree path:

    * ``sessions``  = COUNT of ``sessions`` rows for projects whose slug
      equals ``_path_to_slug(worktree_path)`` (all providers).
    * ``cost_usd``  = SUM of ``project_mart.total_cost_usd`` for those
      projects, falling back to SUM of ``usage_events.cost_usd`` when the
      mart has no row (empty-mart stores).

    Both are single indexed lookups — bounded regardless of store size. A
    worktree with no fragment project (sessions ran only in the parent repo,
    or none at all) reports ``(0, 0.0)``. Never raises.
    """
    if conn is None:
        return 0, 0.0
    slug = _path_to_slug(worktree_path)
    try:
        ids = [
            int(r[0])
            for r in conn.execute(
                "SELECT id FROM projects WHERE slug = ?", (slug,)
            ).fetchall()
        ]
        if not ids:
            return 0, 0.0
        # ``placeholders`` is a fixed ``?`` skeleton derived from the id-list
        # length, never user input; values bind parametrically (same posture
        # as the dynamic IN() builders in yield_tracker / mart_queries).
        placeholders = ",".join("?" for _ in ids)
        sessions = conn.execute(
            f"SELECT COUNT(*) FROM sessions WHERE project_id IN ({placeholders})",  # noqa: S608
            ids,
        ).fetchone()[0]
        cost: float | None = None
        if _table_exists(conn, "project_mart"):
            row = conn.execute(
                f"SELECT SUM(total_cost_usd) FROM project_mart WHERE project_id IN ({placeholders})",  # noqa: S608
                ids,
            ).fetchone()
            cost = row[0] if row is not None else None
        if cost is None and _table_exists(conn, "usage_events"):
            row = conn.execute(
                f"SELECT SUM(cost_usd) FROM usage_events WHERE project_id IN ({placeholders})",  # noqa: S608
                ids,
            ).fetchone()
            cost = row[0] if row is not None else None
        return int(sessions or 0), round(float(cost or 0.0), 4)
    except sqlite3.Error as e:
        logger.debug("worktrees: fragment rollup failed for %s: %s", worktree_path, e)
        return 0, 0.0


# ── repo-level scan ─────────────────────────────────────────────────────────


def list_worktrees(
    conn: sqlite3.Connection,
    project_root: str | None = None,
) -> list[WorktreeInfo]:
    """Enumerate linked git worktrees across the store's known repos.

    When *project_root* is given, only that repo is scanned; otherwise
    candidate roots come from the store's most recent distinct session cwds
    (bounded — see :func:`_candidate_roots_from_store`). Roots that resolve to
    the same repository (worktrees of one repo, or subdirectory cwds) are
    deduplicated by ``git rev-parse --git-common-dir``, and ``git worktree
    list --porcelain`` then runs ONCE per repo. The main worktree and bare
    entries are skipped — only linked worktrees are reported.

    Read-only and never raises: a missing git binary, a non-repo root, or a
    timeout skips that root; per-worktree probe failures degrade to the
    conservative ``HAS_UNIQUE_WORK`` verdict with a note.
    """
    roots = [project_root] if project_root else _candidate_roots_from_store(conn)

    out: list[WorktreeInfo] = []
    seen_repos: set[str] = set()
    seen_worktrees: set[str] = set()
    for root in roots:
        if not root:
            continue
        common = _git_common_dir(root)
        if common is None or common in seen_repos:
            continue
        seen_repos.add(common)

        listing = _run_git(root, ["worktree", "list", "--porcelain"])
        if listing is None:
            continue
        entries = _parse_worktree_porcelain(listing)
        if not entries:
            continue
        main = entries[0]
        parent_repo = None if main.bare else main.path
        # One default-branch resolution per repo (batched, like the listing).
        default_branch = _default_branch(root)

        for entry in entries[1:]:
            if entry.bare or entry.path == main.path or entry.path in seen_worktrees:
                continue
            seen_worktrees.add(entry.path)
            out.append(
                _inspect_worktree(
                    conn,
                    root=root,
                    entry=entry,
                    parent_repo=parent_repo,
                    default_branch=default_branch,
                )
            )
    out.sort(key=lambda w: (w.parent_repo or "", w.path))
    return out


def _inspect_worktree(
    conn: sqlite3.Connection | None,
    *,
    root: str,
    entry: _PorcelainEntry,
    parent_repo: str | None,
    default_branch: str | None,
) -> WorktreeInfo:
    """Build one :class:`WorktreeInfo` — every probe failure degrades, never raises."""
    notes: list[str] = []

    # unique commits vs the default branch (git cherry '+' lines)
    target = entry.branch or entry.head
    unique: int | None
    if default_branch is None:
        unique = None
        notes.append(
            "could not resolve the repo's default branch; "
            "treated as unique work (conservative)"
        )
    elif target is None:
        unique = None
        notes.append(
            "worktree has neither a branch nor a readable HEAD; "
            "treated as unique work (conservative)"
        )
    else:
        unique = _unique_commits(root, default_branch, target)
        if unique is None:
            notes.append(
                f"git cherry against {default_branch} failed; "
                "treated as unique work (conservative)"
            )

    dirty = _dirty_count(entry.path)
    if dirty is None:
        notes.append("git status failed; treated as unique work (conservative)")

    age = _age_days(entry.path)
    if entry.prunable:
        notes.append(f"git reports the worktree prunable ({entry.prunable})")
    if entry.locked:
        notes.append(f"worktree is locked ({entry.locked})")

    verdict = _verdict(age_days=age, unique_commits=unique, dirty_count=dirty)
    sessions, cost_usd = _fragment_rollup(conn, entry.path)

    return WorktreeInfo(
        path=entry.path,
        branch=entry.branch,
        head=entry.head,
        parent_repo=parent_repo,
        parent_slug=_path_to_slug(parent_repo) if parent_repo else None,
        dirty_count=int(dirty or 0),
        unique_commits=int(unique or 0),
        age_days=round(age, 2) if age is not None else None,
        verdict=verdict,
        sessions=sessions,
        cost_usd=cost_usd,
        prune_commands=_prune_commands(entry.path, entry.branch, default_branch),
        note="; ".join(notes) if notes else None,
    )


def _verdict(
    *,
    age_days: float | None,
    unique_commits: int | None,
    dirty_count: int | None,
) -> str:
    """Conservative-first verdict. ``None`` counts mean "a git probe failed".

    Truth table (contract):

    * activity within the last 48h (mtime)        → ``ACTIVE`` (wins over all)
    * any probe failed (unique/dirty unknown)     → ``HAS_UNIQUE_WORK``
    * unique_commits > 0 OR dirty_count > 0       → ``HAS_UNIQUE_WORK``
    * unique_commits == 0 AND dirty_count == 0    → ``MERGED_SAFE_TO_PRUNE``

    ``MERGED_SAFE_TO_PRUNE`` is therefore only reachable when *both* probes
    succeeded and *both* returned 0 — never on any git error.
    """
    if age_days is not None and age_days * 24.0 <= _ACTIVE_WINDOW_HOURS:
        return VERDICT_ACTIVE
    if unique_commits is None or dirty_count is None:
        return VERDICT_HAS_UNIQUE_WORK
    if unique_commits > 0 or dirty_count > 0:
        return VERDICT_HAS_UNIQUE_WORK
    return VERDICT_MERGED_SAFE_TO_PRUNE


def _prune_commands(
    path: str, branch: str | None, default_branch: str | None
) -> list[str]:
    """The exact prune PREVIEW strings — never executed by this module.

    Always includes ``git worktree remove <path>``; adds ``git branch -D
    <branch>`` when the worktree has a branch, unless that branch *is* the
    repo's default branch (deleting main/master is never sensible advice, even
    as a preview). Arguments are shell-quoted so the previews are safe to
    copy-paste even for paths with spaces.
    """
    commands = [f"git worktree remove {shlex.quote(path)}"]
    default_short = None
    if default_branch is not None:
        # "origin/main" → "main"; a bare local name passes through unchanged.
        default_short = default_branch.rsplit("/", 1)[-1]
    if branch and branch != default_short:
        commands.append(f"git branch -D {shlex.quote(branch)}")
    return commands


# ── candidate repo discovery (store-driven) ─────────────────────────────────


def _candidate_roots_from_store(conn: sqlite3.Connection | None) -> list[str]:
    """Distinct session cwds from the store, most recent first, bounded.

    Reads the ``_MAX_SESSIONS_SCANNED`` most recent sessions and bulk-resolves
    each one's first non-empty message cwd with chunked window queries (the
    same single-pass shape ``yield_tracker._bulk_first_cwd_for_sessions``
    introduced — never one query per session). Truncated to
    ``_MAX_DISTINCT_CWDS`` distinct cwds; repo-level dedup happens later via
    ``git rev-parse --git-common-dir`` so several cwds inside one repo cost
    one listing.
    """
    if conn is None:
        return []
    try:
        rows = conn.execute(
            "SELECT id FROM sessions "
            "ORDER BY COALESCE(last_ts, first_ts) DESC, id DESC LIMIT ?",
            (_MAX_SESSIONS_SCANNED,),
        ).fetchall()
    except sqlite3.Error as e:
        logger.debug("worktrees: session enumeration failed: %s", e)
        return []
    session_fks = [int(r[0]) for r in rows]
    cwd_by_fk = _bulk_first_cwd(conn, session_fks)

    ordered: list[str] = []
    seen: set[str] = set()
    for fk in session_fks:  # preserves recency order
        cwd = cwd_by_fk.get(fk, "")
        if cwd and cwd not in seen:
            seen.add(cwd)
            ordered.append(cwd)
            if len(ordered) >= _MAX_DISTINCT_CWDS:
                break
    return ordered


def _bulk_first_cwd(
    conn: sqlite3.Connection, session_fks: list[int]
) -> dict[int, str]:
    """``{session_fk: first non-empty cwd}`` in chunked window queries.

    Same SQL shape as ``yield_tracker._bulk_first_cwd_for_sessions`` (kept
    local so this module never leans on another service's private helper):
    ``ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq)`` surfaces only
    the first cwd-bearing row per session, and the ``IN`` list is chunked
    under SQLite's default variable cap. Degrades to whatever was resolved
    before an error.

    ``json_extract`` is evaluated **once** per row, in an inner CTE the
    ranking and the NULL/'' filter then read as a plain column. SQLite does
    no common-subexpression elimination, so spelling the extract three times
    (select list + two WHERE terms) parsed each message's ``raw_json`` blob
    three times: measured on a 3.9 GB store (500 sessions / ~90k messages)
    that cost 1.56 s versus 1.19 s for the single-extract form, same rows.
    """
    if not session_fks:
        return {}
    out: dict[int, str] = {}
    chunk_size = 500
    for start in range(0, len(session_fks), chunk_size):
        chunk = session_fks[start : start + chunk_size]
        placeholders = ",".join("?" for _ in chunk)
        # ``placeholders`` is a fixed ``?`` skeleton derived from the chunk
        # length, never user input; every value binds parametrically. Same
        # posture (and same one-line + noqa shape) as the long-standing bulk
        # cwd query in ``yield_tracker._bulk_first_cwd_for_sessions``.
        sql = "WITH extracted AS (SELECT session_fk, seq, json_extract(raw_json, '$.cwd') AS cwd FROM messages WHERE session_fk IN (" + placeholders + ")), ranked AS (SELECT session_fk, cwd, ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn FROM extracted WHERE cwd IS NOT NULL AND cwd != '') SELECT session_fk, cwd FROM ranked WHERE rn = 1"  # noqa: S608, E501
        try:
            for row in conn.execute(sql, chunk):
                out[int(row[0])] = str(row[1] or "")
        except sqlite3.Error as e:
            logger.debug("worktrees: bulk cwd resolution failed: %s", e)
            break
    return out


# ── git plumbing (read-only, allowlisted, degrade-on-error) ─────────────────


@dataclass
class _PorcelainEntry:
    """One block of ``git worktree list --porcelain`` output."""

    path: str
    head: str | None = None
    branch: str | None = None  # short name (refs/heads/ stripped)
    detached: bool = False
    bare: bool = False
    locked: str | None = None
    prunable: str | None = None


def _parse_worktree_porcelain(out: str) -> list[_PorcelainEntry]:
    """Parse ``git worktree list --porcelain`` output into entries.

    Blocks are separated by blank lines; each starts with ``worktree <path>``
    followed by attribute lines (``HEAD``, ``branch``, ``detached``, ``bare``,
    ``locked [reason]``, ``prunable [reason]``). Unknown lines are ignored so
    a newer git can add attributes without breaking us.
    """
    entries: list[_PorcelainEntry] = []
    cur: _PorcelainEntry | None = None
    for line in out.splitlines():
        if not line.strip():
            if cur is not None:
                entries.append(cur)
                cur = None
            continue
        key, _, value = line.partition(" ")
        if key == "worktree":
            if cur is not None:  # tolerate a missing blank separator
                entries.append(cur)
            cur = _PorcelainEntry(path=value)
        elif cur is None:
            continue
        elif key == "HEAD":
            cur.head = value or None
        elif key == "branch":
            cur.branch = value.removeprefix("refs/heads/") or None
        elif key == "detached":
            cur.detached = True
        elif key == "bare":
            cur.bare = True
        elif key == "locked":
            cur.locked = value or "locked"
        elif key == "prunable":
            cur.prunable = value or "prunable"
    if cur is not None:
        entries.append(cur)
    return entries


def _git_common_dir(path: str) -> str | None:
    """Resolve *path*'s repo identity (absolute git common dir), or ``None``.

    The common dir is shared by every worktree of a repo, which makes it the
    dedup key: scanning two worktrees (or two subdirectories) of one repo must
    produce one listing. Relative output (``.git`` at the repo root) is
    resolved against *path*; any git failure → ``None`` (skip this root).
    """
    try:
        p = Path(path)
        if not p.is_dir():
            return None
    except OSError:
        return None
    out = _run_git(str(p), ["rev-parse", "--git-common-dir"])
    if out is None or not out.strip():
        return None
    common = Path(out.strip().splitlines()[0])
    if not common.is_absolute():
        common = p / common
    try:
        return str(common.resolve())
    except OSError:
        return str(common)


def _default_branch(root: str) -> str | None:
    """The repo's default branch, or ``None`` when undeterminable.

    Preference order (contract): ``symbolic-ref refs/remotes/origin/HEAD``
    (→ e.g. ``origin/main``, the safest "did it land" comparison base), then a
    local ``main`` / ``master`` that verifies. ``None`` means every candidate
    failed — callers degrade to the conservative verdict.
    """
    out = _run_git(root, ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
    if out and out.strip():
        name = out.strip().splitlines()[0].removeprefix("refs/remotes/")
        if name:
            return name
    for candidate in ("main", "master"):
        if _run_git(
            root, ["rev-parse", "--verify", "--quiet", f"refs/heads/{candidate}"]
        ) is not None:
            return candidate
    return None


def _unique_commits(root: str, default_branch: str, target: str) -> int | None:
    """Count commits on *target* missing from *default_branch* (``git cherry``).

    ``git cherry`` matches by patch id, so a commit that was merged, squashed,
    or cherry-picked under a different SHA still counts as landed (``-``);
    only genuinely unlanded work shows as ``+``. ``None`` on any git failure.
    """
    out = _run_git(root, ["cherry", default_branch, target])
    if out is None:
        return None
    return sum(1 for line in out.splitlines() if line.startswith("+"))


def _dirty_count(worktree_path: str) -> int | None:
    """Changed + untracked path count from ``git status --porcelain``.

    Untracked files count: they would be lost by ``git worktree remove`` just
    like modifications, so they matter for prune safety. ``None`` on any git
    failure (including a deleted worktree directory).
    """
    out = _run_git(worktree_path, ["status", "--porcelain"])
    if out is None:
        return None
    return sum(1 for line in out.splitlines() if line.strip())


def _age_days(worktree_path: str) -> float | None:
    """Days since the worktree directory's mtime; ``None`` when unreadable."""
    try:
        mtime = Path(worktree_path).stat().st_mtime
    except OSError:
        return None
    return max(0.0, (time.time() - mtime) / 86400.0)


def _run_git(cwd: str, args: Sequence[str]) -> str | None:
    """Single chokepoint for every git invocation in this module.

    Explicit argv (no shell), ``--no-optional-locks`` (so not even ``status``
    writes the index), ≤ 5s timeout, and ALL exceptions caught — a missing git
    binary, a non-repo dir, a timeout, or a nonzero exit all return ``None``
    so callers degrade instead of raising.

    Refuses (logs + returns ``None`` for) any subcommand outside the read-only
    allowlist — this module must never mutate a user repo, and the allowlist
    makes that a property of the chokepoint rather than of reviewer vigilance.
    """
    argv = [str(a) for a in args]
    if not any(
        tuple(argv[: len(prefix)]) == prefix for prefix in _ALLOWED_GIT_PREFIXES
    ):
        logger.warning("worktrees: refused non-allowlisted git call %r", argv)
        return None
    try:
        result = subprocess.run(  # noqa: S603 — fixed read-only argv, never shell
            ["git", "--no-optional-locks", "-C", cwd, *argv],  # noqa: S607 — git on PATH
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT_SECONDS,
        )
    except Exception as e:  # noqa: BLE001 — contract: every failure degrades
        logger.debug("worktrees: git %s failed in %s: %s", argv[0] if argv else "?", cwd, e)
        return None
    if result.returncode != 0:
        return None
    return result.stdout


# ── small SQL probes ────────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """True when *name* is a queryable table or view (sqlite_master guard)."""
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master "
            "WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
            (name,),
        ).fetchone()
    except sqlite3.Error:
        return False
    return row is not None


def _column_exists(conn: sqlite3.Connection, table: str, column: str) -> bool:
    """True when *table* has *column* (row-factory-agnostic PRAGMA probe)."""
    try:
        rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    except sqlite3.Error:
        return False
    return any(r[1] == column for r in rows)

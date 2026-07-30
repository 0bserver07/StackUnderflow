"""Proactive skill recommender — surface repeated patterns the user hasn't yet automated.

The :mod:`stackunderflow.services.skill_synth` module mines session history
for project-specific workflow patterns (canonical test command, always-runs-X-
after-Y, …) and emits ``SKILL.md`` files. That surface is *reactive* — the user
must remember to run ``stackunderflow skills generate``.

This module is the *proactive* counterpart. It runs the same pattern miners
behind :func:`recommend_skills` and returns ``Recommendation`` rows that say
"you ran ``X`` ``N`` times across ``M`` sessions, here's the skill that would
replace it". The recommendations are served from a JSON cache under
``~/.stackunderflow/cache/skill_recommendations.json`` so a heavy mining pass
is only paid once per cache TTL.

Hard guardrails (see issue #89 / spec 19):

* **Reuse, don't fork.** All pattern detection is delegated to
  :func:`skill_synth.synthesize_skills`; this module is the gate
  (occurrence threshold) + the surface (recommendation, not generation),
  never a second copy of the detectors.
* **Never auto-apply.** Acceptance is always an explicit user action
  (``stackunderflow skills generate --pattern <id>``). This module returns
  read-only data; nothing here writes a ``SKILL.md``.
* **Filter against existing skills.** Patterns the user already has a skill
  for — checked against ``<project>/.claude/skills/auto-*/`` and
  ``~/.claude/skills/`` — are dropped. We never re-recommend something
  the user has already accepted.
* **Local-only.** Reads the SQLite store + the on-disk skills directories
  + the JSON cache file. No network call, no LLM, no remote anything.
"""

from __future__ import annotations

import json
import logging
import sqlite3
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from stackunderflow.services import skill_synth
from stackunderflow.services.skill_synth import (
    ALL_PATTERN_KINDS,
    DEFAULT_WINDOW,
    SkillCandidate,
)
from stackunderflow.settings import app_dir

__all__ = [
    "Recommendation",
    "RecommendationResult",
    "recommend_skills",
    "default_cache_path",
    "load_cached_recommendations",
    "save_cached_recommendations",
    "clear_recommendation_cache",
    "DEFAULT_THRESHOLD",
    "DEFAULT_WINDOW_DAYS",
    "DEFAULT_CACHE_TTL_SECONDS",
]

_log = logging.getLogger(__name__)

# Cache schema version — bump to invalidate stale on-disk payloads.
_CACHE_VERSION = 1

# A pattern needs to occur in at least this many distinct sessions before
# we surface it. Same default as ``skill_synth`` so the recommender and
# the generator agree on "is this worth automating?".
DEFAULT_THRESHOLD = 5

# Lookback window for fresh recommendations. The spec asks for ~30 days
# (recent enough to be actionable, wide enough to find a habit).
DEFAULT_WINDOW_DAYS = 30

# Cache freshness — a recommendation set is reused for this long before
# we re-mine. Six hours keeps the dashboard cheap without going stale on
# a dev who picks up new patterns mid-day.
DEFAULT_CACHE_TTL_SECONDS = 6 * 60 * 60


# ── data shape ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Recommendation:
    """One pattern the user could automate as a skill.

    The wire shape is intentionally a thin alias of
    :class:`skill_synth.SkillCandidate` plus the recommendation-only fields
    (``occurrences`` is ``evidence_count``; ``sessions`` lists the example
    session ids; ``suggested_skill_template`` carries the rendered
    ``SKILL.md`` body so a downstream "accept" flow can write it without
    re-mining).
    """

    pattern_id: str
    pattern_kind: str
    suggested_skill_name: str
    description: str
    occurrences: int
    sessions: tuple[str, ...]
    last_seen_ts: str
    project_slug: str | None
    suggested_skill_template: str
    accept_command: str
    normalized_command: str | None = field(default=None)

    def to_dict(self) -> dict[str, Any]:
        return {
            "pattern_id": self.pattern_id,
            "pattern_kind": self.pattern_kind,
            "suggested_skill_name": self.suggested_skill_name,
            "description": self.description,
            "occurrences": self.occurrences,
            "sessions": list(self.sessions),
            "last_seen_ts": self.last_seen_ts,
            "project_slug": self.project_slug,
            "suggested_skill_template": self.suggested_skill_template,
            "accept_command": self.accept_command,
            "normalized_command": self.normalized_command,
        }


@dataclass(frozen=True)
class RecommendationResult:
    """Top-level payload returned by :func:`recommend_skills`.

    ``cache_status`` is one of ``"hit"`` / ``"miss"`` / ``"bypassed"`` so
    the caller can show a "(cached)" hint in the CLI / UI when relevant.
    ``filtered_already_installed`` reports how many candidates were
    suppressed because the user already has a skill with that
    ``pattern_id`` — useful for telemetry and for "we found 5, but 3 are
    already installed" messaging.
    """

    recommendations: tuple[Recommendation, ...]
    project: str | None
    threshold: int
    window_days: int
    generated_at: float
    cache_status: str
    filtered_already_installed: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "recommendations": [r.to_dict() for r in self.recommendations],
            "project": self.project,
            "threshold": self.threshold,
            "window_days": self.window_days,
            "generated_at": self.generated_at,
            "cache_status": self.cache_status,
            "filtered_already_installed": self.filtered_already_installed,
        }


# ── existing-skill detection ────────────────────────────────────────────────


def _project_skills_dir(project_path: str | None) -> Path | None:
    """Return ``<project_path>/.claude/skills`` when the project has one."""
    if not project_path:
        return None
    p = Path(project_path).expanduser()
    skills = p / ".claude" / "skills"
    return skills if skills.is_dir() else None


def _user_skills_dir() -> Path | None:
    """Return ``~/.claude/skills`` when it exists."""
    p = Path.home() / ".claude" / "skills"
    return p if p.is_dir() else None


def _resolve_project_path(conn: sqlite3.Connection, slug: str) -> str | None:
    """Look up the on-disk path for a project slug. Returns ``None`` if absent."""
    try:
        row = conn.execute(
            "SELECT path FROM projects WHERE slug = ? AND path IS NOT NULL "
            "ORDER BY last_modified DESC LIMIT 1",
            (slug,),
        ).fetchone()
    except sqlite3.DatabaseError as exc:
        _log.debug("skill_recommender: cannot resolve project path for %s: %s", slug, exc)
        return None
    if not row:
        return None
    path = row[0] if not hasattr(row, "keys") else row["path"]
    return str(path) if path else None


def _installed_pattern_ids(*, project_path: str | None) -> set[str]:
    """Collect ``pattern_id`` values for every auto-generated skill on disk.

    Walks the per-project skills directory (``<project>/.claude/skills``)
    and the user-level one (``~/.claude/skills``); duplicates are fine
    because we collect into a set. The skill_synth helper does the
    auto-generated detection (only files marked ``auto_generated: true``
    are reported) so a hand-authored skill in the same directory is
    correctly ignored.
    """
    seen: set[str] = set()
    candidates: list[Path] = []
    proj_dir = _project_skills_dir(project_path)
    if proj_dir is not None:
        candidates.append(proj_dir)
    user_dir = _user_skills_dir()
    if user_dir is not None:
        candidates.append(user_dir)
    for skills_dir in candidates:
        try:
            for entry in skill_synth.list_generated_skills(skills_dir):
                pid = entry.get("pattern_id")
                if pid:
                    seen.add(str(pid))
        except OSError as exc:
            _log.debug("skill_recommender: cannot list %s: %s", skills_dir, exc)
            continue
    return seen


# ── candidate → recommendation ───────────────────────────────────────────────


def _accept_command(*, candidate: SkillCandidate, project: str | None) -> str:
    """Render the CLI invocation a user would run to install ``candidate``.

    The ``--pattern`` flag isn't wired into ``skills generate`` yet — for
    v1, accepting a recommendation is "regenerate everything for this
    project and pick this one out". The flag-form below is forward-
    compatible: when issue #89 follow-ups land that accept-flow, the
    string shape doesn't change.
    """
    parts = ["stackunderflow", "skills", "generate"]
    if project:
        parts.extend(["--project", project])
    parts.extend(["--pattern", candidate.pattern_id])
    return " ".join(parts)


def _candidate_to_recommendation(
    candidate: SkillCandidate, *, project: str | None
) -> Recommendation:
    return Recommendation(
        pattern_id=candidate.pattern_id,
        pattern_kind=candidate.pattern_kind,
        suggested_skill_name=candidate.name,
        description=candidate.description,
        occurrences=candidate.evidence_count,
        sessions=tuple(candidate.example_session_ids),
        last_seen_ts=candidate.last_seen_ts,
        project_slug=candidate.project_slug or project,
        suggested_skill_template=skill_synth.render_skill_md(candidate),
        accept_command=_accept_command(candidate=candidate, project=project),
        normalized_command=candidate.normalized_command,
    )


# ── cache file ──────────────────────────────────────────────────────────────


def default_cache_path() -> Path:
    """Return the on-disk recommendation-cache file path.

    Lives under ``~/.stackunderflow/cache/`` alongside the cursor cache —
    the spec calls out this exact location.
    """
    return app_dir() / "cache" / "skill_recommendations.json"


def _cache_key(*, project: str | None, threshold: int, window_days: int) -> str:
    """Stable key for one recommendation set.

    Different (project, threshold, window) combinations don't share cache
    entries — a recommendation under ``--threshold 3`` is not a valid
    response to a request for ``--threshold 5``.
    """
    return f"project={project or '*'};threshold={threshold};window={window_days}"


def _load_cache_file(cache_path: Path) -> dict[str, Any]:
    """Read+parse the cache file. Returns ``{}`` on any failure."""
    if not cache_path.is_file():
        return {}
    try:
        text = cache_path.read_text(encoding="utf-8")
    except OSError as exc:
        _log.debug("skill_recommender cache: cannot read %s: %s", cache_path, exc)
        return {}
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        _log.debug("skill_recommender cache: corrupt JSON at %s: %s", cache_path, exc)
        return {}
    if not isinstance(data, dict) or data.get("version") != _CACHE_VERSION:
        return {}
    if not isinstance(data.get("entries"), dict):
        return {}
    return data


def load_cached_recommendations(
    *,
    project: str | None,
    threshold: int,
    window_days: int,
    ttl_seconds: int = DEFAULT_CACHE_TTL_SECONDS,
    cache_path: Path | None = None,
    now: float | None = None,
) -> RecommendationResult | None:
    """Return a cached recommendation set when one is fresh; else ``None``.

    Strict TTL — anything older than ``ttl_seconds`` is treated as a miss
    and the caller is expected to re-mine. Failures (corrupt file, wrong
    schema, missing keys) all degrade to "miss".
    """
    cache_file = cache_path or default_cache_path()
    data = _load_cache_file(cache_file)
    if not data:
        return None
    key = _cache_key(project=project, threshold=threshold, window_days=window_days)
    entry = data.get("entries", {}).get(key)
    if not isinstance(entry, dict):
        return None
    try:
        generated_at = float(entry.get("generated_at"))
    except (TypeError, ValueError):
        return None
    current = now if now is not None else time.time()
    if current - generated_at > ttl_seconds:
        return None

    payload = entry.get("payload")
    if not isinstance(payload, dict):
        return None
    raw_recs = payload.get("recommendations")
    if not isinstance(raw_recs, list):
        return None
    try:
        recs = tuple(_recommendation_from_dict(d) for d in raw_recs)
    except (KeyError, TypeError, ValueError) as exc:
        _log.debug("skill_recommender cache: malformed payload: %s", exc)
        return None
    return RecommendationResult(
        recommendations=recs,
        project=payload.get("project"),
        threshold=int(payload.get("threshold", threshold)),
        window_days=int(payload.get("window_days", window_days)),
        generated_at=generated_at,
        cache_status="hit",
        filtered_already_installed=int(payload.get("filtered_already_installed", 0)),
    )


def _recommendation_from_dict(d: dict[str, Any]) -> Recommendation:
    sessions = d.get("sessions") or ()
    return Recommendation(
        pattern_id=str(d["pattern_id"]),
        pattern_kind=str(d["pattern_kind"]),
        suggested_skill_name=str(d["suggested_skill_name"]),
        description=str(d.get("description", "")),
        occurrences=int(d["occurrences"]),
        sessions=tuple(str(s) for s in sessions),
        last_seen_ts=str(d.get("last_seen_ts", "")),
        project_slug=d.get("project_slug"),
        suggested_skill_template=str(d.get("suggested_skill_template", "")),
        accept_command=str(d.get("accept_command", "")),
        normalized_command=d.get("normalized_command"),
    )


def save_cached_recommendations(
    result: RecommendationResult,
    *,
    project: str | None,
    threshold: int,
    window_days: int,
    cache_path: Path | None = None,
) -> None:
    """Persist ``result`` to the cache. Best-effort — write errors swallowed."""
    cache_file = cache_path or default_cache_path()
    try:
        cache_file.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        _log.debug("skill_recommender cache: cannot mkdir %s: %s", cache_file.parent, exc)
        return

    existing = _load_cache_file(cache_file)
    if not existing:
        existing = {"version": _CACHE_VERSION, "entries": {}}
    key = _cache_key(project=project, threshold=threshold, window_days=window_days)
    existing["entries"][key] = {
        "generated_at": result.generated_at,
        "payload": {
            "recommendations": [r.to_dict() for r in result.recommendations],
            "project": result.project,
            "threshold": result.threshold,
            "window_days": result.window_days,
            "filtered_already_installed": result.filtered_already_installed,
        },
    }
    tmp = cache_file.with_suffix(cache_file.suffix + ".tmp")
    try:
        tmp.write_text(json.dumps(existing, separators=(",", ":")), encoding="utf-8")
        tmp.replace(cache_file)
    except OSError as exc:
        _log.debug("skill_recommender cache: cannot write %s: %s", cache_file, exc)
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass


def clear_recommendation_cache(*, cache_path: Path | None = None) -> bool:
    """Delete the cache file. Returns ``True`` if a file was removed."""
    cache_file = cache_path or default_cache_path()
    if not cache_file.exists():
        return False
    try:
        cache_file.unlink()
        return True
    except OSError as exc:
        _log.debug("skill_recommender cache: cannot remove %s: %s", cache_file, exc)
        return False


# ── public entry ────────────────────────────────────────────────────────────


def recommend_skills(
    conn: sqlite3.Connection,
    *,
    project: str | None = None,
    threshold: int = DEFAULT_THRESHOLD,
    window_days: int = DEFAULT_WINDOW_DAYS,
    pattern_kinds: list[str] | None = None,
    use_cache: bool = True,
    cache_ttl_seconds: int = DEFAULT_CACHE_TTL_SECONDS,
    cache_path: Path | None = None,
    project_path: str | None = None,
    now: float | None = None,
) -> RecommendationResult:
    """Return repeated patterns the user could turn into auto-skills.

    The work is delegated to :func:`skill_synth.synthesize_skills`; this
    function adds:

    * the cache file (``~/.stackunderflow/cache/skill_recommendations.json``)
    * the "user already has a skill for this pattern" filter
    * a flatter wire shape (``Recommendation`` rather than
      ``SkillCandidate``, with the pre-rendered SKILL.md attached)

    Parameters
    ----------
    conn:
        Main store connection.
    project:
        Project slug to scope to. ``None`` raises — the recommender, like
        the generator, never has an implicit "all projects" mode.
    threshold:
        Minimum distinct sessions a pattern must appear in to be
        recommended. Default :data:`DEFAULT_THRESHOLD`.
    window_days:
        Lookback window in days. Default :data:`DEFAULT_WINDOW_DAYS`.
        Translated into the ``Nd`` form ``skill_synth`` accepts.
    pattern_kinds:
        Restrict to these detector kinds (subset of
        :data:`skill_synth.ALL_PATTERN_KINDS`). Default: all.
    use_cache:
        When ``True`` (default), serve from the JSON cache when fresh.
        When ``False`` the cache is bypassed and overwritten with the
        fresh result.
    cache_ttl_seconds:
        Cache freshness window. Default
        :data:`DEFAULT_CACHE_TTL_SECONDS`.
    cache_path:
        Override the on-disk cache file path. Default
        :func:`default_cache_path`.
    project_path:
        Override the resolved on-disk path for ``project``. When ``None``
        the path is looked up from ``projects.path``; this argument is
        useful for tests that don't seed a real on-disk path or that
        want to point at a tmp directory.
    now:
        Wall-clock override for the cache timestamp / TTL check. Tests
        pin this; production passes ``None`` and uses :func:`time.time`.
    """
    if not project:
        raise ValueError(
            "recommend_skills requires project=<slug>. There is no implicit "
            "all-projects mode — match the spec's project-scoped guarantee."
        )
    if threshold < 1:
        raise ValueError("threshold must be >= 1")
    if window_days < 1:
        raise ValueError("window_days must be >= 1")
    kinds = list(pattern_kinds) if pattern_kinds else list(ALL_PATTERN_KINDS)
    unknown = set(kinds) - set(ALL_PATTERN_KINDS)
    if unknown:
        raise ValueError(f"unknown pattern kind(s): {sorted(unknown)}")

    current = now if now is not None else time.time()

    if use_cache:
        cached = load_cached_recommendations(
            project=project,
            threshold=threshold,
            window_days=window_days,
            ttl_seconds=cache_ttl_seconds,
            cache_path=cache_path,
            now=current,
        )
        if cached is not None:
            return cached

    since = f"{window_days}d" if window_days != _default_window_days_value() else DEFAULT_WINDOW
    candidates = skill_synth.synthesize_skills(
        conn,
        project=project,
        min_occurrences=threshold,
        pattern_kinds=kinds,
        since=since,
    )

    # Resolve which auto-generated skills already exist so we don't
    # re-recommend them. ``project_path`` overrides the lookup so tests
    # can point at a tmp dir; production reads ``projects.path``.
    resolved_path = project_path if project_path is not None else _resolve_project_path(conn, project)
    installed = _installed_pattern_ids(project_path=resolved_path)

    fresh: list[Recommendation] = []
    filtered = 0
    for cand in candidates:
        if cand.pattern_id in installed:
            filtered += 1
            continue
        fresh.append(_candidate_to_recommendation(cand, project=project))

    result = RecommendationResult(
        recommendations=tuple(fresh),
        project=project,
        threshold=threshold,
        window_days=window_days,
        generated_at=current,
        cache_status="bypassed" if not use_cache else "miss",
        filtered_already_installed=filtered,
    )
    # Always persist a freshly-mined result — including ``use_cache=False``
    # — so the next "with cache" call sees the most recent miner output.
    save_cached_recommendations(
        result,
        project=project,
        threshold=threshold,
        window_days=window_days,
        cache_path=cache_path,
    )
    return result


def _default_window_days_value() -> int:
    """Numeric form of ``DEFAULT_WINDOW`` for the "use the spec default" path.

    ``DEFAULT_WINDOW = "90d"`` upstream; the recommender's default is 30
    days. When a caller asks for the recommender default (30) we still
    pass ``"30d"`` to ``skill_synth``; this helper exists so the literal
    isn't duplicated and so a future change to the upstream default
    surfaces a single place to update.
    """
    return 90

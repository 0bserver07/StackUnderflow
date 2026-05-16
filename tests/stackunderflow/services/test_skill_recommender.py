"""Unit tests for ``stackunderflow.services.skill_recommender``.

Covers:

* Threshold gate (4 below / 5 at the threshold).
* Project scoping (recommend never returns another project's data, never
  has an implicit "all projects" mode).
* Skill-already-installed filter (auto-* skills with the same
  ``pattern_id`` are dropped from recommendations).
* Cache: file shape, TTL freshness, hit/miss/bypass states, separate
  entries per (project, threshold, window) tuple.
* Recommendation payload shape.

All tests use ``tmp_path``; the real ``~/.stackunderflow/store.db`` and
the real ``~/.stackunderflow/cache/`` are never touched.
"""

from __future__ import annotations

import json
import sqlite3
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from stackunderflow.services import skill_recommender
from stackunderflow.store import db, schema

# ── seeding helpers (mirror tests/services/test_skill_synth.py) ────────────


def _make_conn(tmp_path: Path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_project(
    conn: sqlite3.Connection,
    *,
    provider: str = "claude",
    slug: str = "-Users-yad-dev-foo",
    path: str | None = None,
) -> int:
    return int(
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, "
            "first_seen, last_modified) VALUES (?, ?, ?, ?, 0.0, 0.0)",
            (provider, slug, path, slug),
        ).lastrowid
    )


def _claude_raw(role: str, *, text: str | None, tool_uses: list) -> dict:
    content: list[dict] = []
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}}


def _seed_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    turns: list,
    last_ts: str | None = None,
) -> None:
    """``turns`` items are ``(role, text)`` or ``(role, text, [(tool, args), ...])``."""
    if last_ts is None:
        last_ts = datetime.now(UTC).isoformat()
    sfk = int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, ?, ?)",
            (project_id, session_id, last_ts, last_ts, len(turns)),
        ).lastrowid
    )
    for i, turn in enumerate(turns):
        role = turn[0]
        text = turn[1]
        tool_uses = turn[2] if len(turn) > 2 else []
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain) "
            "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, ?, ?, 0)",
            (
                sfk,
                i,
                last_ts,
                role,
                text,
                json.dumps([t[0] for t in tool_uses]),
                json.dumps(_claude_raw(role, text=text, tool_uses=tool_uses)),
            ),
        )


def _edit(path: str) -> tuple:
    return ("assistant", "editing", [("Edit", {"file_path": path, "old_string": "a", "new_string": "b"})])


def _bash(cmd: str) -> tuple:
    return ("assistant", "running", [("Bash", {"command": cmd})])


def _seed_n_test_sessions(conn, pid, *, n: int, prefix: str = "t", cmd: str = "pytest tests/ -q") -> None:
    """Seed ``n`` sessions, each with one edit + one ``cmd`` run."""
    last = datetime.now(UTC) - timedelta(days=1)
    for k in range(n):
        ts = (last + timedelta(minutes=k)).isoformat()
        _seed_session(
            conn,
            project_id=pid,
            session_id=f"{prefix}-{k}",
            turns=[
                ("user", "do a thing"),
                _edit("/Users/yad/dev/foo/pkg/m.py"),
                _bash(cmd),
            ],
            last_ts=ts,
        )
    conn.commit()


# ── threshold gate ──────────────────────────────────────────────────────────


class TestThresholdGate:
    """Spec: 4 occurrences below threshold of 5 → no recommendation."""

    def test_above_threshold_recommends(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        result = skill_recommender.recommend_skills(
            conn,
            project="-Users-yad-dev-foo",
            threshold=5,
            window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        assert len(result.recommendations) >= 1
        names = {r.suggested_skill_name for r in result.recommendations}
        assert "auto-canonical-test-command" in names
        rec = next(r for r in result.recommendations if r.pattern_kind == "canonical-test-command")
        assert rec.occurrences == 7
        assert "pytest tests/ -q" in rec.suggested_skill_template

    def test_below_threshold_no_recommendation(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=4)
        result = skill_recommender.recommend_skills(
            conn,
            project="-Users-yad-dev-foo",
            threshold=5,
            window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        assert result.recommendations == ()

    def test_at_exactly_threshold_recommends(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=5)
        result = skill_recommender.recommend_skills(
            conn,
            project="-Users-yad-dev-foo",
            threshold=5,
            window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        assert len(result.recommendations) >= 1


# ── scope guarantees ────────────────────────────────────────────────────────


class TestProjectScope:
    def test_no_implicit_all_projects(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError, match="project"):
            skill_recommender.recommend_skills(conn, threshold=5)

    def test_other_projects_data_not_returned(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid_a = _seed_project(conn, slug="-proj-a", path="/Users/yad/dev/a")
        pid_b = _seed_project(conn, slug="-proj-b", path="/Users/yad/dev/b")
        _seed_n_test_sessions(conn, pid_a, n=7, prefix="a")
        _seed_n_test_sessions(conn, pid_b, n=7, prefix="b", cmd="ruff check .")
        # Asking for project A should not surface the ruff pattern from B.
        result = skill_recommender.recommend_skills(
            conn, project="-proj-a", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        assert all(
            "ruff" not in (r.normalized_command or "")
            and "ruff" not in r.suggested_skill_template
            for r in result.recommendations
        )

    def test_validates_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError, match="threshold"):
            skill_recommender.recommend_skills(
                conn, project="-x", threshold=0,
            )

    def test_validates_window_days(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError, match="window_days"):
            skill_recommender.recommend_skills(
                conn, project="-x", window_days=0,
            )

    def test_unknown_pattern_kind_rejected(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError, match="unknown pattern kind"):
            skill_recommender.recommend_skills(
                conn, project="-x", pattern_kinds=["bogus-detector"],
            )


# ── filter against installed skills ────────────────────────────────────────


class TestAlreadyInstalledFilter:
    """Spec: avoid re-recommending patterns the user already has skills for.

    Checks ``<project>/.claude/skills/auto-*/`` and ``~/.claude/skills/``.
    """

    def _install_auto_skill(
        self, skills_dir: Path, *, pattern_id: str,
        name: str = "auto-canonical-test-command",
    ) -> None:
        """Drop a fake auto-generated SKILL.md so the filter sees it."""
        skill_dir = skills_dir / name
        skill_dir.mkdir(parents=True)
        (skill_dir / "SKILL.md").write_text(
            "---\n"
            f"name: {name}\n"
            "description: pre-existing auto-skill, do not re-recommend\n"
            "auto_generated: true\n"
            f"pattern_id: {pattern_id}\n"
            "---\n\n"
            "# x\n\nbody\n",
            encoding="utf-8",
        )

    def test_installed_pattern_filtered(self, tmp_path):
        conn = _make_conn(tmp_path)
        proj_path = tmp_path / "myproj"
        proj_path.mkdir()
        pid = _seed_project(conn, slug="-myproj", path=str(proj_path))
        _seed_n_test_sessions(conn, pid, n=7)

        # First call to find what pattern_id the miner would emit:
        first = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        assert first.recommendations
        target = first.recommendations[0]

        # Now drop a "matching" auto-skill into the project's skills dir
        # and re-run with cache bypassed.
        self._install_auto_skill(proj_path / ".claude" / "skills", pattern_id=target.pattern_id)
        second = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            use_cache=False,
            cache_path=tmp_path / "cache.json",
        )
        assert all(r.pattern_id != target.pattern_id for r in second.recommendations)
        assert second.filtered_already_installed >= 1

    def test_user_skills_dir_also_consulted(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-myproj")
        _seed_n_test_sessions(conn, pid, n=7)

        # Stand up a fake home directory with a user-level auto skill.
        fake_home = tmp_path / "fake_home"
        fake_home.mkdir()
        monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

        first = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
            project_path="",  # disable project-skills lookup
        )
        target = first.recommendations[0]

        # Install in the user dir.
        skills_dir = fake_home / ".claude" / "skills"
        TestAlreadyInstalledFilter()._install_auto_skill(
            skills_dir, pattern_id=target.pattern_id,
        )
        second = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            use_cache=False,
            cache_path=tmp_path / "cache.json",
            project_path="",
        )
        assert all(r.pattern_id != target.pattern_id for r in second.recommendations)
        assert second.filtered_already_installed >= 1

    def test_hand_authored_skill_not_treated_as_installed(self, tmp_path):
        """A non-auto skill at the same dir name is *not* a filter hit."""
        conn = _make_conn(tmp_path)
        proj_path = tmp_path / "myproj"
        proj_path.mkdir()
        pid = _seed_project(conn, slug="-myproj", path=str(proj_path))
        _seed_n_test_sessions(conn, pid, n=7)

        first = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        target = first.recommendations[0]

        # Hand-authored skill (no ``auto_generated: true``).
        skills_dir = proj_path / ".claude" / "skills" / target.suggested_skill_name
        skills_dir.mkdir(parents=True)
        (skills_dir / "SKILL.md").write_text(
            "---\n"
            f"name: {target.suggested_skill_name}\n"
            "description: hand authored, leave alone\n"
            "---\n\nbody\n",
            encoding="utf-8",
        )

        second = skill_recommender.recommend_skills(
            conn, project="-myproj", threshold=5, window_days=90,
            use_cache=False,
            cache_path=tmp_path / "cache.json",
        )
        # The filter only fires on auto-generated skills; the hand-authored
        # one with the same dir name should not have suppressed the rec.
        assert any(r.pattern_id == target.pattern_id for r in second.recommendations)


# ── cache behaviour ─────────────────────────────────────────────────────────


class TestCache:
    def test_first_call_marks_miss_then_hit(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        cache_path = tmp_path / "cache.json"

        first = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, now=1_000_000.0,
        )
        assert first.cache_status == "miss"
        assert cache_path.exists()

        # Second call within TTL — returns the cached payload.
        second = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, now=1_000_001.0,
        )
        assert second.cache_status == "hit"
        assert len(second.recommendations) == len(first.recommendations)
        assert second.generated_at == first.generated_at

    def test_no_cache_bypass_marks_bypassed(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        cache_path = tmp_path / "cache.json"

        skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, now=1_000_000.0,
        )
        bypass = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            use_cache=False, cache_path=cache_path, now=1_000_001.0,
        )
        assert bypass.cache_status == "bypassed"

    def test_ttl_expiry_re_mines(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        cache_path = tmp_path / "cache.json"
        ttl = 100  # seconds

        first = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, cache_ttl_seconds=ttl, now=1_000_000.0,
        )
        # After TTL — should re-mine, not hit.
        second = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, cache_ttl_seconds=ttl, now=1_000_000.0 + ttl + 1,
        )
        assert first.cache_status == "miss"
        assert second.cache_status == "miss"
        assert second.generated_at > first.generated_at

    def test_cache_partitioned_by_threshold(self, tmp_path):
        """Different threshold = different cache key (no false hit)."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        cache_path = tmp_path / "cache.json"

        skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path, now=1_000_000.0,
        )
        other = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=10, window_days=90,
            cache_path=cache_path, now=1_000_001.0,
        )
        assert other.cache_status == "miss"

    def test_corrupt_cache_falls_back_to_miss(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        cache_path = tmp_path / "cache.json"
        cache_path.write_text("not json at all", encoding="utf-8")

        result = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=cache_path,
        )
        assert result.cache_status == "miss"
        assert result.recommendations  # the recommender still ran

    def test_clear_cache_helper(self, tmp_path):
        cache_path = tmp_path / "cache.json"
        cache_path.write_text(json.dumps({"version": 1, "entries": {}}), encoding="utf-8")
        assert skill_recommender.clear_recommendation_cache(cache_path=cache_path) is True
        assert not cache_path.exists()
        assert skill_recommender.clear_recommendation_cache(cache_path=cache_path) is False


# ── payload shape ──────────────────────────────────────────────────────────


class TestPayloadShape:
    def test_recommendation_to_dict(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        result = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        rec = result.recommendations[0]
        d = rec.to_dict()
        assert set(d.keys()) >= {
            "pattern_id", "pattern_kind", "suggested_skill_name",
            "description", "occurrences", "sessions", "last_seen_ts",
            "project_slug", "suggested_skill_template", "accept_command",
        }
        assert d["accept_command"].startswith("stackunderflow skills generate")
        # Template is a real SKILL.md (frontmatter + body)
        assert d["suggested_skill_template"].startswith("---\n")
        assert "auto_generated: true" in d["suggested_skill_template"]

    def test_result_to_dict_has_metadata(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=7)
        result = skill_recommender.recommend_skills(
            conn, project="-Users-yad-dev-foo", threshold=5, window_days=90,
            cache_path=tmp_path / "cache.json",
        )
        d = result.to_dict()
        assert d["project"] == "-Users-yad-dev-foo"
        assert d["threshold"] == 5
        assert d["window_days"] == 90
        assert d["cache_status"] in {"hit", "miss", "bypassed"}
        assert isinstance(d["filtered_already_installed"], int)


# ── default cache path ──────────────────────────────────────────────────────


def test_default_cache_path_under_stackunderflow_cache(tmp_path, monkeypatch):
    fake_home = tmp_path / "h"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    p = skill_recommender.default_cache_path()
    assert p == fake_home / ".stackunderflow" / "cache" / "skill_recommendations.json"

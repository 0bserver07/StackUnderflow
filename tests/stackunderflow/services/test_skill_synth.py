"""Unit + end-to-end tests for ``stackunderflow.services.skill_synth``.

Covers:

* Each detector: empty store → empty, below ``min_occurrences`` → empty,
  exactly ``min_occurrences`` → a candidate (with the right shape).
* End-to-end synthesis on a synthetic multi-pattern store.
* Pattern-merge dedup (two detectors describing the same command collapse
  to one, highest-priority candidate).
* Scope guarantees: no implicit "all projects"; ``project=`` never reads
  another project's data; ``projects=[A, B]`` is cross-project.
* Rendering a ``SKILL.md`` (valid frontmatter, generated marker, body).
* Filesystem behaviour: ``--out`` dir creation, idempotent re-runs (with a
  ``.bak`` written before overwrite), never clobbering a hand-authored
  skill, slug-collision suffixing, ``list`` only seeing generated skills,
  ``clean`` only removing generated skills (and honouring ``--older-than``).
* ``pyproject.toml`` build config excludes ``**/auto-*/SKILL.md``.

All tests use ``tmp_path`` / ``:memory:``; the real
``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
import sqlite3
import tomllib
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from stackunderflow.services import skill_synth
from stackunderflow.services.skill_synth import (
    SkillCandidate,
    clean_generated_skills,
    list_generated_skills,
    render_skill_md,
    synthesize_skills,
    write_skill_files,
)
from stackunderflow.store import db, schema

# ── seeding helpers ─────────────────────────────────────────────────────────


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
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, 0.0, 0.0)",
            (provider, slug, path, slug),
        ).lastrowid
    )


def _claude_raw(
    role: str, *, text: str | None, tool_uses: list[tuple[str, dict]], is_tool_result: bool = False
) -> dict:
    content: list[dict] = []
    if is_tool_result:
        content.append(
            {"type": "tool_result", "tool_use_id": "toolu_x", "content": text or "(output)", "is_error": False}
        )
        text = None
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}, "sessionId": "s"}


def _seed_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    turns: list,
    last_ts: str = "2026-05-01T01:00:00+00:00",
) -> int:
    """``turns`` items: (role, text) or (role, text, [(tool_name, input_dict), ...]).

    A turn whose role is ``"tool"`` is written as a Claude tool_result echo
    (``user`` row in the store) so the user-correction detectors don't treat
    it as a real user turn.
    """
    sfk = int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) VALUES (?, ?, ?, ?, ?)",
            (project_id, session_id, "2026-05-01T00:00:00+00:00", last_ts, len(turns)),
        ).lastrowid
    )
    for i, turn in enumerate(turns):
        role = turn[0]
        text = turn[1]
        tool_uses = turn[2] if len(turn) > 2 else []
        store_role = "user" if role in ("user", "tool") else role
        raw = _claude_raw(store_role, text=text, tool_uses=list(tool_uses), is_tool_result=(role == "tool"))
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain) "
            "VALUES (?, ?, ?, ?, ?, 0, 0, 0, 0, ?, ?, ?, 0)",
            (
                sfk,
                i,
                f"2026-05-01T00:{i:02d}:00+00:00",
                store_role,
                "claude-sonnet-4-5",
                (text or "") if role != "tool" else "(tool output)",
                json.dumps([tu[0] for tu in tool_uses]),
                json.dumps(raw),
            ),
        )
    return sfk


# common turn fragments
def _edit(path: str) -> tuple:
    return ("assistant", "editing", [("Edit", {"file_path": path, "old_string": "a", "new_string": "b"})])


def _bash(cmd: str) -> tuple:
    return ("assistant", "running", [("Bash", {"command": cmd})])


def _seed_n_test_sessions(conn, pid, *, n: int, prefix: str = "t", cmd: str = "pytest tests/ -q") -> None:
    for k in range(n):
        _seed_session(conn, project_id=pid, session_id=f"{prefix}-{k}", turns=[
            ("user", "do a thing"), _edit("/Users/yad/dev/foo/stackunderflow/m.py"), _bash(cmd),
        ])
    conn.commit()


# ── canonical-test-command detector ─────────────────────────────────────────


class TestCanonicalTestCommand:
    def _run(self, conn, **kw):
        return synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["canonical-test-command"],
                                 since=None, **kw)

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn)
        assert self._run(conn, min_occurrences=5) == []

    def test_below_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=4)
        assert self._run(conn, min_occurrences=5) == []

    def test_exactly_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_n_test_sessions(conn, pid, n=5)
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        c = out[0]
        assert c.pattern_kind == "canonical-test-command"
        assert c.name == "auto-canonical-test-command"
        assert c.evidence_count == 5
        assert "pytest tests/ -q" in c.body
        assert "pytest tests/ -q" in c.description
        assert c.project_slug == "-Users-yad-dev-foo"
        assert len(c.example_session_ids) <= 3

    def test_picks_most_frequent_runner(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # 6 sessions on pytest, 5 on npm test — pytest should win.
        for k in range(6):
            _seed_session(conn, project_id=pid, session_id=f"p-{k}", turns=[("user", "x"), _bash("pytest -q")])
        for k in range(5):
            _seed_session(conn, project_id=pid, session_id=f"n-{k}", turns=[("user", "x"), _bash("npm test")])
        conn.commit()
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        assert "pytest" in out[0].body and out[0].evidence_count == 6


# ── always-runs-X-after-Y detector ──────────────────────────────────────────


class TestAlwaysRunsAfterEdit:
    def _run(self, conn, **kw):
        return synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["always-runs-X-after-Y"],
                                 since=None, **kw)

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn)
        assert self._run(conn, min_occurrences=5) == []

    def test_below_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(4):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}",
                          turns=[("user", "x"), _edit("/Users/yad/dev/foo/m.py"), _bash("ruff check --fix .")])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []

    def test_exactly_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(5):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}",
                          turns=[("user", "x"), _edit("/Users/yad/dev/foo/pkg/m.py"), _bash("ruff check --fix pkg/")])
        conn.commit()
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        c = out[0]
        assert c.pattern_kind == "always-runs-X-after-Y"
        assert c.name.startswith("auto-run-") and c.name.endswith("-after-edits")
        assert c.evidence_count == 5
        assert "ruff check --fix" in c.body

    def test_command_must_reliably_follow_edits(self, tmp_path):
        # 10 sessions edit; only 4 of them run the candidate command after the
        # edit — below the 0.5 "reliably follows" ratio, so no candidate.
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(4):
            _seed_session(conn, project_id=pid, session_id=f"y-{k}",
                          turns=[("user", "x"), _edit("/Users/yad/dev/foo/m.py"), _bash("eslint . --fix")])
        for k in range(6):
            _seed_session(conn, project_id=pid, session_id=f"n-{k}",
                          turns=[("user", "x"), _edit("/Users/yad/dev/foo/m.py"), ("assistant", "done")])
        conn.commit()
        assert self._run(conn, min_occurrences=4) == []


# ── uses-tool-flag-combo detector ───────────────────────────────────────────


class TestUsesToolFlagCombo:
    def _run(self, conn, **kw):
        return synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["uses-tool-flag-combo"],
                                 since=None, **kw)

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn)
        assert self._run(conn, min_occurrences=5) == []

    def test_below_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(4):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[("user", "x"), _bash("ruff check --fix .")])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []

    def test_exactly_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(5):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}",
                          turns=[("user", "x"), _bash("ruff check --fix src/")])
        conn.commit()
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        c = out[0]
        assert c.pattern_kind == "uses-tool-flag-combo"
        assert c.name == "auto-flags-ruff-check"
        assert c.evidence_count == 5
        assert "--fix" in c.body

    def test_bare_command_without_flags_not_a_candidate(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(8):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[("user", "x"), _bash("make build")])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []


# ── avoids-X detector ───────────────────────────────────────────────────────


class TestAvoidsCommand:
    def _run(self, conn, **kw):
        return synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["avoids-X"], since=None, **kw)

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn)
        assert self._run(conn, min_occurrences=5) == []

    def test_below_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(4):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "restart it"), _bash("pkill -9 -f uvicorn"), ("user", "don't use pkill — graceful first"),
            ])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []

    def test_exactly_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(5):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "restart it"),
                _bash("pkill -9 -f uvicorn"),
                ("user", "don't use pkill — use a graceful SIGTERM"),
            ])
        conn.commit()
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        c = out[0]
        assert c.pattern_kind == "avoids-X"
        assert c.name == "auto-avoid-pkill"
        assert c.evidence_count == 5
        assert "pkill" in c.body

    def test_no_correction_no_candidate(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(8):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "restart it"), _bash("pkill -9 -f uvicorn"), ("user", "great, thanks!"),
            ])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []

    def test_tool_result_echo_is_not_a_user_correction(self, tmp_path):
        # The "user"-role message after the Bash call is a tool_result echo
        # that happens to contain "error" — must NOT count as a correction.
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(8):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "restart it"), _bash("pkill -9 -f uvicorn"),
                ("tool", "no error; pkill: killed 1 process"), ("assistant", "done"),
            ])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []


# ── never-touches-paths detector ────────────────────────────────────────────


class TestNeverTouchesPaths:
    def _run(self, conn, **kw):
        return synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["never-touches-paths"],
                                 since=None, **kw)

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn)
        assert self._run(conn, min_occurrences=5) == []

    def test_below_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(4):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "fix the data"), _edit("/Users/yad/.stackunderflow/store.db"),
                ("user", "never touch store.db — tests use tmp_path"),
            ])
        conn.commit()
        assert self._run(conn, min_occurrences=5) == []

    def test_exactly_threshold(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for k in range(5):
            _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
                ("user", "fix the data"), _edit("/Users/yad/.stackunderflow/store.db"),
                ("user", "never touch store.db — tests use tmp_path"),
            ])
        conn.commit()
        out = self._run(conn, min_occurrences=5)
        assert len(out) == 1
        c = out[0]
        assert c.pattern_kind == "never-touches-paths"
        assert c.name == "auto-never-touch-store-db"
        assert c.evidence_count == 5
        assert "store.db" in c.body


# ── end-to-end + dedup ──────────────────────────────────────────────────────


def _seed_rich_store(conn) -> int:
    pid = _seed_project(conn)
    # 8 sessions: edit a .py file → pytest → ruff check --fix
    for k in range(8):
        _seed_session(conn, project_id=pid, session_id=f"e-{k}", turns=[
            ("user", "add a feature"),
            _edit("/Users/yad/dev/foo/stackunderflow/mod.py"),
            _bash("pytest tests/ -q"),
            _bash("ruff check --fix stackunderflow/"),
        ])
    # 6 sessions: pkill correction
    for k in range(6):
        _seed_session(conn, project_id=pid, session_id=f"k-{k}", turns=[
            ("user", "restart the server"),
            _bash("pkill -9 -f uvicorn"),
            ("user", "don't use pkill — use a graceful SIGTERM first"),
        ])
    # 5 sessions: store.db edit correction
    for k in range(5):
        _seed_session(conn, project_id=pid, session_id=f"d-{k}", turns=[
            ("user", "fix the data"),
            _edit("/Users/yad/.stackunderflow/store.db"),
            ("user", "never touch store.db — tests use tmp_path"),
        ])
    conn.commit()
    return pid


def test_end_to_end_synthesis(tmp_path):
    conn = _make_conn(tmp_path)
    _seed_rich_store(conn)
    cands = synthesize_skills(conn, project="-Users-yad-dev-foo", min_occurrences=5, since=None)
    kinds = {c.pattern_kind for c in cands}
    assert "canonical-test-command" in kinds
    assert "avoids-X" in kinds
    assert "never-touches-paths" in kinds
    # ruff check --fix reliably follows edits -> after-edit candidate
    assert "always-runs-X-after-Y" in kinds
    # sorted by evidence desc
    counts = [c.evidence_count for c in cands]
    assert counts == sorted(counts, reverse=True)
    # every name is auto-prefixed and slug-shaped
    for c in cands:
        assert c.name.startswith("auto-")
        assert all(ch.isalnum() or ch == "-" for ch in c.name)


def test_dedup_collapses_test_command_seen_two_ways(tmp_path):
    # pytest -q is both the canonical test command AND reliably runs after
    # edits — exactly the "two detectors, equivalent candidate" case.
    conn = _make_conn(tmp_path)
    pid = _seed_project(conn)
    for k in range(7):
        _seed_session(conn, project_id=pid, session_id=f"s-{k}", turns=[
            ("user", "do a thing"), _edit("/Users/yad/dev/foo/m.py"), _bash("pytest -q"),
        ])
    conn.commit()
    cands = synthesize_skills(conn, project="-Users-yad-dev-foo", min_occurrences=5, since=None)
    pytest_cands = [c for c in cands if "pytest" in (c.normalized_command or "")]
    assert len(pytest_cands) == 1
    assert pytest_cands[0].pattern_kind == "canonical-test-command"


def test_merge_and_dedup_unit():
    a = SkillCandidate(pattern_id="aaa", name="auto-x", description="d", body="b", evidence_count=3,
                       last_seen_ts="t", pattern_kind="uses-tool-flag-combo", normalized_command="ruff check --fix")
    b = SkillCandidate(pattern_id="bbb", name="auto-x", description="d", body="b", evidence_count=9,
                       last_seen_ts="t", pattern_kind="uses-tool-flag-combo", normalized_command="ruff check --fix")
    # same name -> keep higher evidence
    merged = skill_synth._merge_and_dedup([a, b])
    assert len(merged) == 1 and merged[0].evidence_count == 9
    # same normalized command, different kinds -> keep higher-priority kind
    c1 = SkillCandidate(pattern_id="c1", name="auto-flags-pytest", description="d", body="b", evidence_count=5,
                        last_seen_ts="t", pattern_kind="uses-tool-flag-combo", normalized_command="pytest -q")
    c2 = SkillCandidate(pattern_id="c2", name="auto-canonical-test-command", description="d", body="b",
                        evidence_count=5, last_seen_ts="t", pattern_kind="canonical-test-command",
                        normalized_command="pytest -q")
    merged2 = skill_synth._merge_and_dedup([c1, c2])
    assert len(merged2) == 1 and merged2[0].pattern_kind == "canonical-test-command"


# ── scope guarantees ────────────────────────────────────────────────────────


def test_scope_required():
    conn = sqlite3.connect(":memory:")
    schema.apply(conn)
    with pytest.raises(ValueError, match="scope"):
        synthesize_skills(conn)


def test_project_scope_does_not_read_other_projects(tmp_path):
    conn = _make_conn(tmp_path)
    a = _seed_project(conn, slug="-Users-yad-dev-aaa")
    b = _seed_project(conn, slug="-Users-yad-dev-bbb")
    # A has a strong pytest pattern; B has a strong cargo-test pattern.
    for k in range(6):
        _seed_session(conn, project_id=a, session_id=f"a-{k}", turns=[("user", "x"), _bash("pytest tests/ -q")])
    for k in range(6):
        _seed_session(conn, project_id=b, session_id=f"b-{k}", turns=[("user", "x"), _bash("cargo test")])
    conn.commit()
    cands_a = synthesize_skills(conn, project="-Users-yad-dev-aaa", min_occurrences=5, since=None)
    blob_a = " ".join(c.body for c in cands_a)
    assert "pytest" in blob_a and "cargo" not in blob_a
    assert all(c.project_slug == "-Users-yad-dev-aaa" for c in cands_a)


def test_explicit_multi_project_is_cross_project(tmp_path):
    conn = _make_conn(tmp_path)
    a = _seed_project(conn, slug="-Users-yad-dev-aaa")
    b = _seed_project(conn, slug="-Users-yad-dev-bbb")
    for k in range(3):
        _seed_session(conn, project_id=a, session_id=f"a-{k}", turns=[("user", "x"), _bash("pytest -q")])
    for k in range(3):
        _seed_session(conn, project_id=b, session_id=f"b-{k}", turns=[("user", "x"), _bash("pytest -q")])
    conn.commit()
    cands = synthesize_skills(
        conn, projects=["-Users-yad-dev-aaa", "-Users-yad-dev-bbb"], min_occurrences=5, since=None
    )
    assert cands  # 3 + 3 = 6 sessions across two projects
    assert all(c.project_slug is None for c in cands)


# ── rendering ───────────────────────────────────────────────────────────────


def test_render_skill_md_shape(tmp_path):
    conn = _make_conn(tmp_path)
    pid = _seed_project(conn)
    _seed_n_test_sessions(conn, pid, n=6)
    cand = synthesize_skills(conn, project="-Users-yad-dev-foo", pattern_kinds=["canonical-test-command"],
                             since=None, min_occurrences=5)[0]
    text = render_skill_md(cand, generated_at=datetime(2026, 5, 12, 9, 0, 0, tzinfo=UTC))
    lines = text.splitlines()
    assert lines[0] == "---"
    end = lines.index("---", 1)
    fm = dict(line.partition(":")[::2] for line in lines[1:end] if ":" in line)
    fm = {k.strip(): v.strip() for k, v in fm.items()}
    assert fm["name"] == cand.name
    assert fm["auto_generated"] == "true"
    assert fm["pattern_kind"] == "canonical-test-command"
    assert fm["pattern_id"] == cand.pattern_id
    assert fm["generated_at"] == "2026-05-12T09:00:00+00:00"
    assert fm["evidence_count"] == "6"
    assert fm["description"]  # non-empty
    body = "\n".join(lines[end + 1 :])
    assert "<!-- Generated by stackunderflow skills generate at 2026-05-12T09:00:00+00:00 from 6 sessions" in body
    assert body.strip().startswith("<!--")
    assert "# Run this project's tests" in body
    assert "## Evidence" in body


# ── filesystem: write / list / clean ────────────────────────────────────────


def _candidate(name="auto-demo", pattern_id="abc123def4567890", ev=5, kind="canonical-test-command") -> SkillCandidate:
    return SkillCandidate(
        pattern_id=pattern_id, name=name,
        description="Triggers in tests; run the canonical command.",
        body="# Demo\n\nbody text.\n\n## Evidence\n\nMost-recent example sessions: s-1, s-2\n",
        evidence_count=ev, last_seen_ts="2026-05-01T00:00:00+00:00", pattern_kind=kind,
        project_slug="-Users-yad-dev-foo",
    )


def _write_hand_authored(dir_path: Path, name: str) -> str:
    """Drop a *hand-authored* SKILL.md (no ``auto_generated`` marker). Returns its text."""
    text = (
        f"---\nname: {name}\ndescription: a hand-authored skill, hands off please leave it be\n"
        f"---\n\n# {name}\n\nhands off — this one is mine.\n"
    )
    dir_path.mkdir(parents=True, exist_ok=True)
    (dir_path / "SKILL.md").write_text(text)
    return text


def test_write_creates_out_dir_and_files(tmp_path):
    out = tmp_path / "deep" / ".claude" / "skills"
    assert not out.exists()
    res = write_skill_files([_candidate()], out)
    assert (out / "auto-demo" / "SKILL.md").is_file()
    assert [r.action for r in res] == ["created"]


def test_write_is_idempotent_with_backup(tmp_path):
    out = tmp_path / ".claude" / "skills"
    gen1 = datetime(2026, 5, 12, 9, 0, 0, tzinfo=UTC)
    write_skill_files([_candidate(ev=5)], out, generated_at=gen1)
    # identical content (only the volatile generated_at line differs) -> unchanged
    res2 = write_skill_files([_candidate(ev=5)], out, generated_at=datetime(2026, 5, 12, 10, 0, 0, tzinfo=UTC))
    assert [r.action for r in res2] == ["unchanged"]
    assert not (out / "auto-demo" / "SKILL.md.bak").exists()
    # changed content (evidence_count) -> updated, and a .bak is written
    res3 = write_skill_files([_candidate(ev=12)], out, generated_at=datetime(2026, 5, 12, 11, 0, 0, tzinfo=UTC))
    assert [r.action for r in res3] == ["updated"]
    bak = out / "auto-demo" / "SKILL.md.bak"
    assert bak.is_file()
    assert "evidence_count: 5" in bak.read_text()
    assert "evidence_count: 12" in (out / "auto-demo" / "SKILL.md").read_text()
    # still exactly one skill directory
    assert sorted(p.name for p in out.iterdir()) == ["auto-demo"]


def test_write_skips_hand_authored_skill(tmp_path):
    out = tmp_path / ".claude" / "skills"
    user_content = _write_hand_authored(out / "auto-demo", "auto-demo")
    res = write_skill_files([_candidate()], out)
    assert [r.action for r in res] == ["skipped-user-authored"]
    assert (out / "auto-demo" / "SKILL.md").read_text() == user_content  # untouched


def test_write_collision_gets_hash_suffix(tmp_path):
    out = tmp_path / ".claude" / "skills"
    a = _candidate(name="auto-clash", pattern_id="1111aaaabbbbcccc")
    b = _candidate(name="auto-clash", pattern_id="2222ddddeeeeffff")
    res = write_skill_files([a, b], out)
    names = {r.name for r in res}
    assert "auto-clash" in names
    assert any(n.startswith("auto-clash-") for n in names)
    assert (out / "auto-clash" / "SKILL.md").is_file()
    assert (out / f"auto-clash-{b.pattern_id[:6]}" / "SKILL.md").is_file()


def test_dry_run_writes_nothing(tmp_path):
    out = tmp_path / ".claude" / "skills"
    res = write_skill_files([_candidate()], out, dry_run=True)
    assert [r.action for r in res] == ["would-create"]
    assert not out.exists()


def test_list_only_sees_generated_skills(tmp_path):
    out = tmp_path / ".claude" / "skills"
    write_skill_files([_candidate(name="auto-real")], out)
    # a hand-authored skill that *looks* auto (no marker), and a non-auto dir
    _write_hand_authored(out / "auto-fake", "auto-fake")
    _write_hand_authored(out / "my-skill", "my-skill")
    items = list_generated_skills(out)
    assert [it["name"] for it in items] == ["auto-real"]


def test_list_missing_dir_is_empty(tmp_path):
    assert list_generated_skills(tmp_path / "nope") == []


def test_clean_removes_only_generated(tmp_path):
    out = tmp_path / ".claude" / "skills"
    write_skill_files([_candidate(name="auto-a"), _candidate(name="auto-b")], out)
    _write_hand_authored(out / "auto-fake", "auto-fake")
    _write_hand_authored(out / "my-skill", "my-skill")
    removed = clean_generated_skills(out)
    assert sorted(p.name for p in removed) == ["auto-a", "auto-b"]
    assert not (out / "auto-a").exists() and not (out / "auto-b").exists()
    assert (out / "auto-fake").exists() and (out / "my-skill").exists()


def test_clean_dry_run_keeps_everything(tmp_path):
    out = tmp_path / ".claude" / "skills"
    write_skill_files([_candidate(name="auto-a")], out)
    removed = clean_generated_skills(out, dry_run=True)
    assert [p.name for p in removed] == ["auto-a"]
    assert (out / "auto-a").exists()


def test_clean_older_than_window(tmp_path):
    out = tmp_path / ".claude" / "skills"
    old = datetime.now(UTC) - timedelta(days=60)
    recent = datetime.now(UTC) - timedelta(days=2)
    write_skill_files([_candidate(name="auto-old")], out, generated_at=old)
    write_skill_files([_candidate(name="auto-recent")], out, generated_at=recent)
    removed = clean_generated_skills(out, older_than="30d")
    assert [p.name for p in removed] == ["auto-old"]
    assert not (out / "auto-old").exists() and (out / "auto-recent").exists()


# ── packaging guardrail ─────────────────────────────────────────────────────


def _pyproject() -> dict:
    root = Path(__file__).resolve().parents[3]
    with (root / "pyproject.toml").open("rb") as fh:
        return tomllib.load(fh)


def test_pyproject_excludes_auto_skills_from_wheel_and_sdist():
    pp = _pyproject()
    wheel = pp["tool"]["hatch"]["build"]["targets"]["wheel"]
    sdist = pp["tool"]["hatch"]["build"]["targets"]["sdist"]
    assert "**/auto-*/SKILL.md" in wheel.get("exclude", [])
    assert "**/auto-*/SKILL.md" in sdist.get("exclude", [])
    # the package's own (static) skills are not auto-* — they still ship
    assert wheel.get("packages") == ["stackunderflow"]


@pytest.mark.slow
def test_built_wheel_contains_no_auto_skills(tmp_path):
    """Build a real wheel and confirm no ``auto-*/SKILL.md`` slipped in.

    Skipped if the build toolchain isn't importable. Belt-and-suspenders on
    top of :func:`test_pyproject_excludes_auto_skills_from_wheel_and_sdist`.
    """
    import subprocess
    import sys
    import zipfile

    root = Path(__file__).resolve().parents[3]
    # dogfood: drop a generated-looking skill in the repo's .claude/ first
    bait = root / ".claude" / "skills" / "auto-bait"
    created_bait = False
    try:
        if not bait.exists():
            bait.mkdir(parents=True)
            (bait / "SKILL.md").write_text("---\nname: auto-bait\nauto_generated: true\n---\n# bait\n")
            created_bait = True
        cmd = [sys.executable, "-m", "build", "--wheel", "--no-isolation", "--outdir", str(tmp_path), str(root)]
        try:
            subprocess.run(cmd, check=True, capture_output=True, timeout=180)  # noqa: S603 — fixed, trusted argv
        except (subprocess.CalledProcessError, FileNotFoundError, ImportError) as exc:
            pytest.skip(f"wheel build unavailable: {exc}")
        wheels = list(tmp_path.glob("*.whl"))
        assert wheels, "build produced no wheel"
        with zipfile.ZipFile(wheels[0]) as zf:
            names = zf.namelist()
        assert not any("auto-" in n and n.endswith("SKILL.md") for n in names), \
            f"auto-generated skill leaked into the wheel: {[n for n in names if 'auto-' in n]}"
    finally:
        if created_bait:
            import shutil
            shutil.rmtree(bait.parent if bait.parent.name == "skills" and not any(bait.parent.iterdir()) else bait,
                          ignore_errors=True)
            shutil.rmtree(bait, ignore_errors=True)

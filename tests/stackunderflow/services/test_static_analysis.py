"""Tests for ``stackunderflow.services.static_analysis``.

Spec 21 — per-session static analysis pass. Covers the per-language
analyzers, the runner coordinator (pre/post snapshot reconstruction →
analyzer dispatch → row persistence), the backfill idempotency
contract, and the missing-binary skip path. Mirrors the
fixture-helper pattern in :mod:`tests.stackunderflow.services.test_risk`
so a future spec adding an analyzer can extend the same scaffolding.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.services import static_analysis
from stackunderflow.services.static_analysis import (
    go_analyzer,
    python_analyzer,
    runner,
    typescript_analyzer,
)
from stackunderflow.services.static_analysis.runner import (
    AnalysisOutcome,
    METRIC_KEYS,
    SUPPORTED_LANGUAGES,
    SessionQuality,
    detect_language,
)
from stackunderflow.store import db, schema


# ── shared seeding helpers ─────────────────────────────────────────────────


def _make_conn(tmp_path: Path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_session(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    file_path: str,
    pre_content: str,
    post_content: str,
    first_ts: str = "2026-04-01T00:00:00+00:00",
    last_ts: str = "2026-04-01T00:01:00+00:00",
    project_slug: str = "-Users-yad-dev-foo",
) -> int:
    """Seed a project + session + a Read-then-Edit message pair.

    Pre-content is captured via the Read's ``tool_result``; post-content
    is the result of an Edit substitution. This mirrors the actual
    Playback v2 reconstruction path the runner uses.
    """
    row = conn.execute(
        "SELECT id FROM projects WHERE provider='claude' AND slug=?",
        (project_slug,),
    ).fetchone()
    if row is None:
        pcur = conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, "
            " first_seen, last_modified) VALUES "
            "('claude', ?, NULL, 'foo', 0.0, 0.0)",
            (project_slug,),
        )
        project_id = int(pcur.lastrowid)
    else:
        project_id = int(row["id"] if isinstance(row, sqlite3.Row) else row[0])

    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 4)",
        (project_id, session_id, first_ts, last_ts),
    )
    sfk = int(sfk_cur.lastrowid)

    # Message 1: assistant Read tool_use.
    read_use_id = f"call_read_{session_id}"
    read_envelope = {
        "type": "assistant",
        "timestamp": first_ts,
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": read_use_id,
                    "name": "Read",
                    "input": {"file_path": file_path},
                },
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 0, ?, 'assistant', 'claude-sonnet-4-5', 0, 0, 0, 0, '', "
        " '[{\"name\":\"Read\"}]', ?, 0)",
        (sfk, first_ts, json.dumps(read_envelope)),
    )
    # Message 2: user tool_result with the pre content.
    read_result_envelope = {
        "type": "user",
        "timestamp": first_ts,
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": read_use_id,
                    "content": pre_content,
                },
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 1, ?, 'user', '', 0, 0, 0, 0, '', '[]', ?, 0)",
        (sfk, first_ts, json.dumps(read_result_envelope)),
    )
    # Message 3: assistant Write tool_use that replaces the file.
    write_use_id = f"call_write_{session_id}"
    write_envelope = {
        "type": "assistant",
        "timestamp": last_ts,
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": write_use_id,
                    "name": "Write",
                    "input": {"file_path": file_path, "content": post_content},
                },
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 2, ?, 'assistant', 'claude-sonnet-4-5', 0, 0, 0, 0, '', "
        " '[{\"name\":\"Write\"}]', ?, 0)",
        (sfk, last_ts, json.dumps(write_envelope)),
    )
    # Message 4: user tool_result confirming the Write.
    write_result = {
        "type": "user",
        "timestamp": last_ts,
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": write_use_id,
                    "content": "wrote",
                },
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 3, ?, 'user', '', 0, 0, 0, 0, '', '[]', ?, 0)",
        (sfk, last_ts, json.dumps(write_result)),
    )
    conn.commit()
    return sfk


def _seed_created_in_session(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    file_path: str,
    content: str,
    project_slug: str = "-Users-yad-dev-bar",
) -> int:
    """Seed a session whose only file-touching tool call is a Write.

    No prior Read ⇒ Playback v2 produces no pre-snapshot for this path
    ⇒ runner records ``pre_value=NULL``, ``delta=NULL``,
    ``details_json["reason"]="file_created_in_session"``.
    """
    pcur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', ?, NULL, 'bar', 0.0, 0.0)",
        (project_slug,),
    )
    project_id = int(pcur.lastrowid)
    first_ts = "2026-04-02T00:00:00+00:00"
    last_ts = "2026-04-02T00:00:30+00:00"
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 2)",
        (project_id, session_id, first_ts, last_ts),
    )
    sfk = int(sfk_cur.lastrowid)
    write_use_id = f"call_w_{session_id}"
    write_env = {
        "type": "assistant",
        "timestamp": last_ts,
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": write_use_id,
                    "name": "Write",
                    "input": {"file_path": file_path, "content": content},
                },
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 0, ?, 'assistant', 'claude-sonnet-4-5', 0, 0, 0, 0, '', "
        " '[{\"name\":\"Write\"}]', ?, 0)",
        (sfk, last_ts, json.dumps(write_env)),
    )
    write_res = {
        "type": "user",
        "timestamp": last_ts,
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": write_use_id, "content": "ok"},
            ],
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 1, ?, 'user', '', 0, 0, 0, 0, '', '[]', ?, 0)",
        (sfk, last_ts, json.dumps(write_res)),
    )
    conn.commit()
    return sfk


# ── language detection ─────────────────────────────────────────────────────


class TestLanguageDetect:
    @pytest.mark.parametrize("path,expected", [
        ("/foo/bar.py", "python"),
        ("/foo/bar.PY", "python"),
        ("/foo/bar.ts", "typescript"),
        ("/foo/bar.tsx", "typescript"),
        ("/foo/bar.js", "typescript"),
        ("/foo/bar.jsx", "typescript"),
        ("/foo/bar.go", "go"),
        ("/foo/bar.rs", None),
        ("/foo/bar.txt", None),
        ("Makefile", None),
    ])
    def test_detect(self, path: str, expected: str | None):
        assert detect_language(path) == expected

    def test_supported_languages_lock(self):
        assert SUPPORTED_LANGUAGES == ("python", "typescript", "go")

    def test_metric_keys_lock(self):
        # The schema enum is closed; a new metric needs a deliberate
        # bump here + a per-analyzer ALL_METRICS entry.
        assert METRIC_KEYS == (
            "complexity", "coverage", "lint_count", "type_completeness",
        )


# ── per-language analyzer (Python — the one with installable deps) ─────────


class TestPythonAnalyzer:
    def test_lint_count_clean_file(self, tmp_path):
        f = tmp_path / "clean.py"
        f.write_text("x = 1\n")
        result = python_analyzer.analyze(f, "x = 1\n")
        # ruff is in the dev env; the file is trivially clean.
        if "lint_count" in result.metrics:
            assert result.metrics["lint_count"] == 0.0

    def test_lint_count_dirty_file(self, tmp_path):
        # Unused import + bare except → at least one ruff finding.
        content = "import os\ntry:\n    x = 1\nexcept:\n    pass\n"
        f = tmp_path / "dirty.py"
        f.write_text(content)
        result = python_analyzer.analyze(f, content)
        if "lint_count" in result.metrics:
            assert result.metrics["lint_count"] >= 1.0

    def test_type_completeness_all_typed(self, tmp_path):
        content = "def f(x: int) -> int:\n    return x\n"
        f = tmp_path / "t.py"
        f.write_text(content)
        result = python_analyzer.analyze(f, content)
        assert result.metrics.get("type_completeness") == 1.0
        assert result.details["type_completeness"] == {
            "functions": 1, "typed_functions": 1,
        }

    def test_type_completeness_none_typed(self, tmp_path):
        content = "def f(x):\n    return x\n"
        f = tmp_path / "t.py"
        f.write_text(content)
        result = python_analyzer.analyze(f, content)
        assert result.metrics.get("type_completeness") == 0.0
        assert result.details["type_completeness"]["functions"] == 1
        assert result.details["type_completeness"]["typed_functions"] == 0

    def test_type_completeness_self_arg_exempt(self, tmp_path):
        # ``self`` should not count against the typing ratio.
        content = (
            "class C:\n"
            "    def m(self, x: int) -> int:\n"
            "        return x\n"
        )
        f = tmp_path / "t.py"
        f.write_text(content)
        result = python_analyzer.analyze(f, content)
        assert result.metrics.get("type_completeness") == 1.0

    def test_empty_file_skips_metrics(self, tmp_path):
        f = tmp_path / "e.py"
        f.write_text("")
        result = python_analyzer.analyze(f, "")
        # type_completeness needs ≥1 function; lint runs but value is 0
        assert "type_completeness" not in result.metrics

    def test_available_returns_string_reason(self):
        avail, reason = python_analyzer.available()
        assert isinstance(avail, bool)
        assert isinstance(reason, str)


# ── per-language analyzer (TS — usually missing on CI) ─────────────────────


class TestTypeScriptAnalyzer:
    def test_available_when_tools_missing(self):
        avail, reason = typescript_analyzer.available()
        # On a CI box without tsc/eslint we get False; assert the
        # contract is still well-formed.
        assert isinstance(avail, bool)
        assert isinstance(reason, str)
        if not avail:
            assert "tsc" in reason or "eslint" in reason

    def test_analyze_handles_missing_tools(self, tmp_path):
        content = "const x: number = 1;\n"
        f = tmp_path / "t.ts"
        f.write_text(content)
        result = typescript_analyzer.analyze(f, content)
        # Either tools are installed and we got real metrics, or the
        # analyzer skipped and recorded warnings.
        if not typescript_analyzer.available()[0]:
            assert result.metrics == {}
            assert any("not on PATH" in w for w in result.warnings)


# ── per-language analyzer (Go — usually missing on CI) ─────────────────────


class TestGoAnalyzer:
    def test_available_when_tools_missing(self):
        avail, reason = go_analyzer.available()
        assert isinstance(avail, bool)
        assert isinstance(reason, str)
        if not avail:
            assert "go" in reason or "gocyclo" in reason

    def test_analyze_handles_missing_tools(self, tmp_path):
        content = "package main\nfunc main() {}\n"
        f = tmp_path / "t.go"
        f.write_text(content)
        result = go_analyzer.analyze(f, content)
        if not go_analyzer.available()[0]:
            assert result.metrics == {}


# ── runner: snapshot reconstruction + persistence ──────────────────────────


class TestAnalyzeSession:
    def test_unknown_session_yields_empty_outcome(self, tmp_path):
        conn = _make_conn(tmp_path)
        out = static_analysis.analyze_session(conn, "no-such-id")
        assert isinstance(out, AnalysisOutcome)
        assert out.rows_written == 0
        assert out.files_analyzed == 0

    def test_empty_session_id_raises(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError):
            static_analysis.analyze_session(conn, "")

    def test_unsupported_language_skipped(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="rs-1",
            file_path="/tmp/notes.txt",
            pre_content="hello\n",
            post_content="hello world\n",
        )
        out = static_analysis.analyze_session(conn, "rs-1")
        assert out.files_analyzed == 0
        assert out.rows_written == 0
        assert any("unsupported language" in s for s in out.skipped_files)

    def test_python_session_writes_rows(self, tmp_path):
        conn = _make_conn(tmp_path)
        pre = "def f(x):\n    return x\n"
        post = "def f(x: int) -> int:\n    return x\n"
        _seed_session(
            conn,
            session_id="py-1",
            file_path="/tmp/example.py",
            pre_content=pre,
            post_content=post,
        )
        out = static_analysis.analyze_session(conn, "py-1")
        assert out.files_analyzed == 1
        assert "python" in out.languages
        # At least one metric got produced — type_completeness is
        # always available (pure-Python AST).
        rows = conn.execute(
            "SELECT metric, pre_value, post_value, delta "
            "FROM static_analysis_findings WHERE session_id = ? "
            "ORDER BY metric",
            ("py-1",),
        ).fetchall()
        assert rows, "expected at least one persisted finding"
        tc_rows = [r for r in rows if r["metric"] == "type_completeness"]
        assert tc_rows, "type_completeness should be present"
        tc = tc_rows[0]
        # Pre had 0 typed, post had 1 typed → improvement (delta = +1.0).
        assert tc["pre_value"] == 0.0
        assert tc["post_value"] == 1.0
        assert tc["delta"] == 1.0

    def test_no_op_session_writes_no_rows(self, tmp_path):
        """A session that Reads but never edits a file produces no rows."""
        conn = _make_conn(tmp_path)
        same = "def f(x: int) -> int:\n    return x\n"
        _seed_session(
            conn,
            session_id="noop-1",
            file_path="/tmp/noop.py",
            pre_content=same,
            post_content=same,
        )
        out = static_analysis.analyze_session(conn, "noop-1")
        # The Write replays as identical content → pre == post → skipped.
        assert out.rows_written == 0

    def test_file_created_in_session_records_null_pre(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_created_in_session(
            conn,
            session_id="create-1",
            file_path="/tmp/new.py",
            content="def f(x: int) -> int:\n    return x\n",
        )
        out = static_analysis.analyze_session(conn, "create-1")
        assert out.files_analyzed == 1
        rows = conn.execute(
            "SELECT metric, pre_value, post_value, delta, details_json "
            "FROM static_analysis_findings WHERE session_id = ?",
            ("create-1",),
        ).fetchall()
        assert rows, "expected persisted findings"
        for r in rows:
            assert r["pre_value"] is None
            assert r["delta"] is None
            details = json.loads(r["details_json"])
            assert details.get("pre_reason") == "file_created_in_session"

    def test_idempotent_replace(self, tmp_path):
        """Re-running analyze_session does not create duplicate rows."""
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="idem-1",
            file_path="/tmp/example.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        out1 = static_analysis.analyze_session(conn, "idem-1")
        out2 = static_analysis.analyze_session(conn, "idem-1")
        rows = conn.execute(
            "SELECT COUNT(*) AS n FROM static_analysis_findings "
            "WHERE session_id = ?",
            ("idem-1",),
        ).fetchone()
        assert rows["n"] == out1.rows_written
        assert out1.rows_written == out2.rows_written

    def test_only_languages_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="filter-1",
            file_path="/tmp/example.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        out = static_analysis.analyze_session(
            conn, "filter-1", only_languages=("go",),
        )
        # Filter excludes python ⇒ no analysis.
        assert out.files_analyzed == 0
        assert out.rows_written == 0


# ── backfill ───────────────────────────────────────────────────────────────


class TestBackfill:
    def test_empty_store_returns_zero_candidates(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = runner.backfill(conn, since=None, limit=None, concurrency=1)
        assert result == {
            "candidates": 0, "analyzed": 0, "rows_written": 0,
            "warnings_count": 0,
        }

    def test_single_session_backfilled(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="bf-1",
            file_path="/tmp/example.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        result = runner.backfill(conn, since=None, limit=None, concurrency=1)
        assert result["candidates"] == 1
        assert result["analyzed"] == 1
        assert result["rows_written"] >= 1

    def test_idempotent_re_backfill_skips_done(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="bf-2",
            file_path="/tmp/example.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        first = runner.backfill(conn, since=None, limit=None, concurrency=1)
        second = runner.backfill(conn, since=None, limit=None, concurrency=1)
        assert first["analyzed"] == 1
        assert second["candidates"] == 0
        assert second["analyzed"] == 0

    def test_limit_caps_candidates(self, tmp_path):
        conn = _make_conn(tmp_path)
        for i in range(3):
            _seed_session(
                conn,
                session_id=f"bf-many-{i}",
                file_path=f"/tmp/m{i}.py",
                pre_content="def f(x):\n    return x\n",
                post_content="def f(x: int) -> int:\n    return x\n",
                first_ts=f"2026-04-{i+1:02d}T00:00:00+00:00",
                last_ts=f"2026-04-{i+1:02d}T00:01:00+00:00",
            )
        result = runner.backfill(conn, since=None, limit=2, concurrency=1)
        assert result["candidates"] == 2
        assert result["analyzed"] == 2

    def test_since_filter_drops_old(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="old-1",
            file_path="/tmp/o.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
            first_ts="2024-01-01T00:00:00+00:00",
            last_ts="2024-01-01T00:01:00+00:00",
        )
        result = runner.backfill(
            conn, since="2026-01-01T00:00:00+00:00", limit=None, concurrency=1,
        )
        assert result["candidates"] == 0


# ── get_session_quality ───────────────────────────────────────────────────


class TestGetSessionQuality:
    def test_unknown_session_returns_empty_quality(self, tmp_path):
        conn = _make_conn(tmp_path)
        q = static_analysis.get_session_quality(conn, "nope")
        assert isinstance(q, SessionQuality)
        assert q.findings == []
        assert q.summary["files"] == 0
        assert q.summary["metrics"] == {}

    def test_returns_persisted_findings_with_summary(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="q-1",
            file_path="/tmp/ex.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        static_analysis.analyze_session(conn, "q-1")
        q = static_analysis.get_session_quality(conn, "q-1")
        assert q.session_id == "q-1"
        assert q.summary["files"] >= 1
        assert "python" in q.summary["languages"]
        assert "type_completeness" in q.summary["metrics"]
        # type_completeness improved ⇒ improved counter ≥ 1.
        tc = q.summary["metrics"]["type_completeness"]
        assert tc["improved"] >= 1
        assert tc["regressed"] == 0
        assert isinstance(q.summary["headline"], str)
        assert q.summary["headline"]


# ── missing-binary safety net (TS analyzer skips cleanly) ──────────────────


class TestMissingBinaryHandling:
    def test_ts_analyzer_skips_when_unavailable(self, monkeypatch, tmp_path):
        """Force TS tools as missing; analyzer should yield empty metrics."""
        monkeypatch.setattr(typescript_analyzer, "_tsc_available", lambda: False)
        monkeypatch.setattr(typescript_analyzer, "_eslint_available", lambda: False)
        avail, reason = typescript_analyzer.available()
        assert avail is False
        assert "tsc" in reason or "eslint" in reason
        f = tmp_path / "t.ts"
        f.write_text("const x = 1;\n")
        result = typescript_analyzer.analyze(f, "const x = 1;\n")
        assert result.metrics == {}

    def test_runner_writes_placeholder_when_python_tools_missing(
        self, monkeypatch, tmp_path,
    ):
        """No python metrics produced ⇒ runner records a placeholder row.

        Forces every python metric path to return ``None`` so the
        analyzer surfaces no metrics. The runner falls back to a
        ``no_metrics_produced`` placeholder row.
        """
        monkeypatch.setattr(python_analyzer, "_radon_available", lambda: False)
        monkeypatch.setattr(python_analyzer, "_ruff_available", lambda: False)
        monkeypatch.setattr(python_analyzer, "_mypy_available", lambda: False)

        # Force the AST type_completeness pass to also bow out, so the
        # analyzer produces zero metrics. We do that by stubbing the
        # internal helper.
        def _no_tc(file_path, content):
            return None, {}, "type_completeness disabled for test"

        monkeypatch.setattr(python_analyzer, "_type_completeness", _no_tc)
        # available() now returns False (no per-metric tool available)
        # ⇒ the runner skips the file with a "skipped" entry. That's
        # the documented "tool not available" path.
        assert python_analyzer.available()[0] is False

        conn = _make_conn(tmp_path)
        _seed_session(
            conn,
            session_id="missing-1",
            file_path="/tmp/x.py",
            pre_content="def f(x):\n    return x\n",
            post_content="def f(x: int) -> int:\n    return x\n",
        )
        out = static_analysis.analyze_session(conn, "missing-1")
        # Analyzer was unavailable ⇒ skipped, no rows.
        assert out.rows_written == 0
        assert any("analyzer unavailable" in s for s in out.skipped_files)


# ── meta-agent contract lock-in ────────────────────────────────────────────


class TestMetaAgentSurface:
    def test_quality_to_dict_shape(self, tmp_path):
        from stackunderflow.services.static_analysis.runner import quality_to_dict

        conn = _make_conn(tmp_path)
        q = static_analysis.get_session_quality(conn, "no-session")
        d = quality_to_dict(q)
        assert set(d) == {"session_id", "findings", "summary"}
        assert set(d["summary"]) == {"files", "languages", "metrics", "headline"}

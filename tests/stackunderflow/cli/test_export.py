"""End-to-end tests for ``stackunderflow export``.

The CLI command writes to a file path the user supplies, so tests
operate against a temp directory and then parse the output back with
the stdlib ``csv`` / ``json`` modules to verify shape.
"""

from __future__ import annotations

import csv
import io
import json
import os

import pytest
from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.reports.export import ACTIVITY_HEADERS, DAILY_HEADERS
from stackunderflow.store import db, schema


# ── shared helpers ───────────────────────────────────────────────────────────

def _seed_store(store_db, *, projects=None, messages=None) -> None:
    """Populate a fresh store with test rows.

    ``projects``: list of (provider, slug) tuples.
    ``messages``: list of dicts with project_slug, session_id, timestamp,
                  role, model, in_tok, out_tok, cache_r, cache_w.
    """
    conn = db.connect(store_db)
    schema.apply(conn)

    project_id_for: dict[tuple[str, str], int] = {}
    for prov, slug in (projects or []):
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_id_for[(prov, slug)] = cur.lastrowid

    session_id_for: dict[tuple[int, str], int] = {}
    seq_counter: dict[int, int] = {}
    for m in (messages or []):
        provider = m.get("provider", "claude")
        slug = m["project_slug"]
        proj_pk = project_id_for[(provider, slug)]
        sid = m["session_id"]
        sk = (proj_pk, sid)
        if sk not in session_id_for:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (proj_pk, sid, m["timestamp"], m["timestamp"], 0),
            )
            session_id_for[sk] = cur.lastrowid
        sess_fk = session_id_for[sk]
        seq = seq_counter.get(sess_fk, 0)
        seq_counter[sess_fk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sess_fk, seq, m["timestamp"], m["role"], m.get("model"),
                m.get("in_tok", 0), m.get("out_tok", 0),
                m.get("cache_w", 0), m.get("cache_r", 0),
                m.get("content", ""),
                m.get("tools_json", "[]"),
                m.get("raw_json", "{}"),
                0, None, None,
            ),
        )
    conn.commit()
    conn.close()


def _fixture_store(tmp_path):
    """A small but realistic store: 2 projects, 2 providers, 2 days."""
    store_db = tmp_path / "store.db"
    _seed_store(
        store_db,
        projects=[
            ("claude", "alpha"),
            ("claude", "beta"),
            ("codex", "gamma"),
        ],
        messages=[
            # alpha (claude) — 2025-01-01
            {"project_slug": "alpha", "session_id": "s1",
             "timestamp": "2025-01-01T10:00:00Z", "role": "assistant",
             "model": "claude-sonnet-4-5-20250929",
             "in_tok": 1000, "out_tok": 200, "cache_r": 50, "cache_w": 30},
            {"project_slug": "alpha", "session_id": "s1",
             "timestamp": "2025-01-01T10:05:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "s2",
             "timestamp": "2025-01-02T11:00:00Z", "role": "assistant",
             "model": "claude-sonnet-4-5-20250929",
             "in_tok": 500, "out_tok": 100},
            # beta (claude) — 2025-01-02
            {"project_slug": "beta", "session_id": "s3",
             "timestamp": "2025-01-02T12:00:00Z", "role": "assistant",
             "model": "claude-sonnet-4-5-20250929",
             "in_tok": 2000, "out_tok": 300},
            # gamma (codex) — 2025-01-02
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "s4",
             "timestamp": "2025-01-02T13:00:00Z", "role": "assistant",
             "model": "gpt-5",
             "in_tok": 800, "out_tok": 150},
        ],
    )
    return store_db


def _invoke(runner, args, store_db, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    return runner.invoke(cli, args)


# ── CSV format tests ─────────────────────────────────────────────────────────

class TestExportCsv:
    def test_csv_period_all_writes_full_rollup(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert out.exists()

        text = out.read_text()
        # Daily section headers
        for h in DAILY_HEADERS:
            assert h in text, f"missing column {h} in: {text}"

        # Parse the daily section back: first non-comment row is the header.
        rows = list(csv.reader(io.StringIO(text)))
        # Locate header row
        header_idx = next(
            i for i, r in enumerate(rows) if r and r[0] == "date"
        )
        header = rows[header_idx]
        assert header == DAILY_HEADERS

        data_rows = []
        for r2 in rows[header_idx + 1:]:
            if not r2 or r2[0].startswith("#"):
                break
            data_rows.append(r2)

        # 4 distinct (date, provider, project) tuples expected
        seen = {(r[0], r[1], r[2]) for r in data_rows}
        assert ("2025-01-01", "claude", "alpha") in seen
        assert ("2025-01-02", "claude", "alpha") in seen
        assert ("2025-01-02", "claude", "beta") in seen
        assert ("2025-01-02", "codex", "gamma") in seen

        # Token columns should parse as ints, cost as float
        for r2 in data_rows:
            float(r2[3])  # cost_usd
            int(r2[4])    # calls
            int(r2[5])    # sessions
            int(r2[6])    # input_tokens
            int(r2[7])    # output_tokens

        # Activity section header is present
        assert "# activity" in text or "# activity —" in text
        assert "activity,calls,share_pct" in text

    def test_csv_default_period_is_multi_period_rollup(self, tmp_path, monkeypatch):
        """Omitting --period gives today + last_7d + last_30d sections."""
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "rollup.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out),
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        # Three period section markers
        assert text.count("# period:") == 3
        for label in ("today", "last 7 days", "last 30 days"):
            assert label in text, f"expected '{label}' in: {text}"

    def test_csv_provider_filter(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "claude.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out),
            "-p", "all", "--provider", "claude",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        assert "alpha" in text
        assert "beta" in text
        assert "gamma" not in text  # codex project filtered out

    def test_csv_project_include_filter(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "alpha-only.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out),
            "-p", "all", "--project", "alpha",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        assert "alpha" in text
        assert "beta" not in text
        assert "gamma" not in text

    def test_csv_exclude_filter(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "no-beta.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out),
            "-p", "all", "--exclude", "beta",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        assert "alpha" in text
        assert "gamma" in text
        assert "beta" not in text


# ── JSON format tests ────────────────────────────────────────────────────────

class TestExportJson:
    def test_json_period_all_top_level_shape(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.json"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "json", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        data = json.loads(out.read_text())
        # Single-period shape
        for key in (
            "label", "since", "until",
            "totals", "daily", "projects", "models",
            "activities", "tools", "mcp", "shell",
        ):
            assert key in data, f"missing key {key}"
        assert isinstance(data["daily"], list)
        assert isinstance(data["projects"], list)
        assert isinstance(data["models"], dict)

    def test_json_default_is_multi_period(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.json"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "json", "-o", str(out),
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        data = json.loads(out.read_text())
        # Multi-period shape
        for key in ("today", "last_7d", "last_30d"):
            assert key in data, f"missing top-level period key {key}"
        for key in ("schema", "generated", "filters"):
            assert key in data
        # Each period has the full single-period shape
        for sub_key in ("today", "last_7d", "last_30d"):
            sub = data[sub_key]
            for inner in ("daily", "projects", "models", "totals"):
                assert inner in sub

    def test_json_models_block_includes_seeded_models(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.json"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "json", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        data = json.loads(out.read_text())
        models = data["models"]
        assert "claude-sonnet-4-5-20250929" in models
        assert "gpt-5" in models


# ── period variants ──────────────────────────────────────────────────────────

class TestPeriodVariants:
    @pytest.mark.parametrize("period", ["today", "week", "month", "all"])
    def test_explicit_periods_succeed(self, tmp_path, monkeypatch, period):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / f"{period}.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", period,
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert out.exists()

    def test_invalid_period_rejected(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "yesterday",
        ], store_db, monkeypatch)
        assert r.exit_code != 0


# ── safe-write semantics ─────────────────────────────────────────────────────

class TestSafeWrite:
    def test_existing_file_without_force_fails(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        out.write_text("preexisting content\n")
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code != 0
        # File untouched
        assert out.read_text() == "preexisting content\n"

    def test_existing_file_with_force_overwrites(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        out.write_text("preexisting content\n")
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all", "--force",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "preexisting content" not in out.read_text()
        assert "date,provider" in out.read_text()

    def test_symlink_target_rejected(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        target = tmp_path / "real.csv"
        target.write_text("real\n")
        link = tmp_path / "link.csv"
        os.symlink(target, link)
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(link), "-p", "all", "--force",
        ], store_db, monkeypatch)
        assert r.exit_code != 0
        # Real file untouched
        assert target.read_text() == "real\n"

    def test_creates_parent_dir(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "deeper" / "subdir" / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert out.exists()


# ── empty database ───────────────────────────────────────────────────────────

class TestEmptyDatabase:
    def test_csv_empty_db_writes_headers_only(self, tmp_path, monkeypatch):
        store_db = tmp_path / "empty.db"
        # Apply schema with no rows
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()

        out = tmp_path / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        # Headers present, no data rows.
        assert "date,provider,project,cost_usd" in text
        assert "activity,calls,share_pct" in text
        # First non-header column joined as "date" should be the only date row
        rows = list(csv.reader(io.StringIO(text)))
        header_idx = next(i for i, r in enumerate(rows) if r and r[0] == "date")
        # Next row is either the activity comment or empty — no data row.
        next_row = rows[header_idx + 1] if len(rows) > header_idx + 1 else []
        assert (not next_row) or next_row[0].startswith("#")

    def test_json_empty_db_writes_zero_totals(self, tmp_path, monkeypatch):
        store_db = tmp_path / "empty.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()

        out = tmp_path / "out.json"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "json", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        data = json.loads(out.read_text())
        assert data["daily"] == []
        assert data["projects"] == []
        assert data["totals"]["calls"] == 0
        assert data["totals"]["cost_usd"] == 0.0


# ── activity/csv shape sanity ─────────────────────────────────────────────────

class TestActivitySection:
    def test_activity_headers_match_constant(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        text = out.read_text()
        for col in ACTIVITY_HEADERS:
            assert col in text


# ── format requirement ───────────────────────────────────────────────────────

class TestRequiredFlags:
    def test_format_required(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        out = tmp_path / "out.csv"
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-o", str(out), "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code != 0

    def test_output_required(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(runner, [
            "export", "-f", "csv", "-p", "all",
        ], store_db, monkeypatch)
        assert r.exit_code != 0

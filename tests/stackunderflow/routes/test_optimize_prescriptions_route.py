"""Route contract for the campaign-#7 prescriptions endpoints.

* ``GET  /api/optimize/prescriptions`` — routing recommendations (real
  ``compute_cost`` black box, real manifest models) + CLAUDE.md slim
  previews sourced from the same read-only ``~/.claude`` discovery the
  bloat detector has always used (home is monkeypatched to ``tmp_path`` —
  rule 3: tests never touch the real ``~/.claude``).
* ``POST /api/optimize/claudemd-preview`` — client-supplied text in,
  preview out, no filesystem involved at all.

Every asserted dollar figure is recomputed in the test through the same
public ``compute_cost`` entry point — nothing hardcoded, nothing invented.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi import HTTPException

from stackunderflow.infra.costs import compute_cost
from stackunderflow.routes import optimize as optimize_route
from stackunderflow.routes.optimize import ClaudeMdPreviewBody
from stackunderflow.store import db, schema

# ── seeding helpers ──────────────────────────────────────────────────────────


def _seed_store(store_db, *, slug="demo", provider="anthropic"):
    """Store with one project + a low-reasoning opus workload over 2 days.

    Token shape: 1M input + 200K output, 5K reasoning (share 0.025 → the
    low-reasoning downshift rule). ``cost_usd`` is stored as the real
    ``compute_cost`` price of that shape on the model, so the route's
    "actual" figure is the same black-box number the test recomputes.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0, 0)",
        (provider, slug, slug),
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)", (pid, f"{slug}-s1")
    )
    sfk = conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (f"{slug}-s1",)
    ).fetchone()["id"]

    shape_per_event = {"input": 500_000, "output": 100_000, "cache_creation": 0, "cache_read": 0}
    per_event_cost = float(
        compute_cost(shape_per_event, "claude-opus-4-8", provider="anthropic")["total_cost"]
    )
    base = conn.execute("SELECT COALESCE(MAX(id), 0) FROM messages").fetchone()[0]
    for i, day in enumerate(("2026-06-01", "2026-06-02")):
        msg_id = base + i + 1
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, tools_json, "
            " content_text, raw_json) VALUES (?, ?, ?, ?, 'assistant', '[]', '', '{}')",
            (msg_id, sfk, msg_id, f"{day}T10:00:00Z"),
        )
        conn.execute(
            "INSERT INTO usage_events ("
            " source_message_fk, provider, project_id, session_id, ts, day, model, speed,"
            " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,"
            " reasoning_tokens, cost_usd, cost_source, role"
            ") VALUES (?, 'anthropic', ?, ?, ?, ?, 'claude-opus-4-8', 'standard',"
            " 500000, 100000, 0, 0, 2500, ?, 'rate_card', 'assistant')",
            (msg_id, pid, f"{slug}-s1", f"{day}T10:00:00Z", day, per_event_cost),
        )
    conn.commit()
    conn.close()
    return per_event_cost * 2  # window actual


@pytest.fixture()
def fake_home(tmp_path, monkeypatch):
    """Redirect ``Path.home()`` to a tmp dir (rule 3: never the real ~)."""
    home = tmp_path / "home"
    (home / ".claude" / "projects").mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
    return home


def _bloated_claude_md(home: Path) -> Path:
    """A >5000-token CLAUDE.md with slimmable content (comment + dupes)."""
    md = home / ".claude" / "CLAUDE.md"
    dup = "This exact paragraph repeats hundreds of times and wastes context tokens."
    md.write_text(
        "# Global notes\n\n<!-- private -->\n\n" + (dup + "\n\n") * 400,
        encoding="utf-8",
    )
    return md


# ── GET /api/optimize/prescriptions ─────────────────────────────────────────


@pytest.mark.asyncio
async def test_prescriptions_payload_contract_and_routing_math(
    tmp_path, monkeypatch, fake_home
):
    store_db = tmp_path / "store.db"
    actual_window_cost = _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await optimize_route.get_prescriptions(period="all")

    assert body["scope"] == "all time"
    assert body["project"] is None
    assert "currency" in body and body["currency"]["code"]

    routing = body["routing"]
    assert routing["observed_days"] == 2
    assert routing["monthly_factor"] == 15.0

    recs = routing["recommendations"]
    assert len(recs) == 1
    rec = recs[0]
    assert rec["rec_id"] == "downshift_low_reasoning"
    assert rec["from_model"] == "claude-opus-4-8"
    # Cheapest same-provider candidate in the default set is Haiku 4.5.
    assert rec["to_model"] == "claude-haiku-4-5-20251001"

    # Dollar figures trace to compute_cost — recomputed here, not hardcoded.
    haiku_cost = float(
        compute_cost(
            {"input": 1_000_000, "output": 200_000, "cache_creation": 0, "cache_read": 0},
            "claude-haiku-4-5-20251001",
            provider="anthropic",
        )["total_cost"]
    )
    assert rec["window_cost_usd"] == pytest.approx(actual_window_cost)
    assert rec["candidate_window_cost_usd"] == pytest.approx(haiku_cost)
    assert rec["window_delta_usd"] == pytest.approx(haiku_cost - actual_window_cost)
    assert rec["estimated_monthly_delta_usd"] == pytest.approx(
        (haiku_cost - actual_window_cost) * 15.0
    )
    assert rec["window_delta_usd"] < 0  # a saving, by construction

    # No bloated CLAUDE.md in the fake home → no previews, no fabrication.
    assert body["claudemd_previews"] == []


@pytest.mark.asyncio
async def test_prescriptions_includes_claudemd_preview_for_bloated_file(
    tmp_path, monkeypatch, fake_home
):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    md_path = _bloated_claude_md(fake_home)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await optimize_route.get_prescriptions(period="all")

    previews = body["claudemd_previews"]
    assert len(previews) == 1
    p = previews[0]
    assert p["source_path"] == str(md_path)
    assert p["changed"] is True
    assert p["preview_diff"].startswith(f"--- {md_path}")
    assert p["tokens_saved"] > 0
    assert p["slimmed_text"]
    # The duplicate paragraphs are the dominant waste in the fixture.
    rules = [r["rule"] for r in p["rationale"]]
    assert "dedupe_paragraphs" in rules
    # Savings math is internally consistent (per-session × sessions = monthly).
    assert p["estimated_savings_usd_monthly"] == pytest.approx(
        p["estimated_savings_usd_per_session"] * p["sessions_per_month"]
    )
    # ...and the file on disk is untouched: preview-only, the server never
    # writes user files.
    assert md_path.read_text(encoding="utf-8").startswith("# Global notes")


@pytest.mark.asyncio
async def test_prescriptions_project_param_scopes_routing(
    tmp_path, monkeypatch, fake_home
):
    store_db = tmp_path / "store.db"
    _seed_store(store_db, slug="demo")
    # Second project with a different-model workload that must not leak in.
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('anthropic', 'other', 'other', 0, 0)"
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = 'other'").fetchone()["id"]
    conn.execute("INSERT INTO sessions (project_id, session_id) VALUES (?, 'o-s1')", (pid,))
    sfk = conn.execute("SELECT id FROM sessions WHERE session_id = 'o-s1'").fetchone()["id"]
    msg_id = conn.execute("SELECT MAX(id) + 1 FROM messages").fetchone()[0]
    conn.execute(
        "INSERT INTO messages (id, session_fk, seq, timestamp, role, tools_json, "
        " content_text, raw_json) VALUES (?, ?, ?, '2026-06-01T10:00:00Z', "
        " 'assistant', '[]', '', '{}')",
        (msg_id, sfk, msg_id),
    )
    conn.execute(
        "INSERT INTO usage_events ("
        " source_message_fk, provider, project_id, session_id, ts, day, model, speed,"
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,"
        " reasoning_tokens, cost_usd, cost_source, role"
        ") VALUES (?, 'anthropic', ?, 'o-s1', '2026-06-01T10:00:00Z', '2026-06-01',"
        " 'claude-sonnet-4-6', 'standard', 1000, 100, 0, 0, 0, 0.01, 'rate_card', 'assistant')",
        (msg_id, pid),
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await optimize_route.get_prescriptions(period="all", project="demo")
    assert body["project"] == "demo"
    assert [m["model"] for m in body["routing"]["models"]] == ["claude-opus-4-8"]

    # Unknown slug → empty scope, well-formed empty payload (not the whole store).
    empty = await optimize_route.get_prescriptions(period="all", project="nope")
    assert empty["project"] == "nope"
    assert empty["routing"]["recommendations"] == []
    assert empty["routing"]["models"] == []


@pytest.mark.asyncio
async def test_prescriptions_rejects_unknown_period(tmp_path, monkeypatch, fake_home):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    with pytest.raises(HTTPException) as exc:
        await optimize_route.get_prescriptions(period="fortnight")
    assert exc.value.status_code == 400


@pytest.mark.asyncio
async def test_prescriptions_empty_store_clean_payload(tmp_path, monkeypatch, fake_home):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await optimize_route.get_prescriptions(period="all")
    assert body["routing"]["recommendations"] == []
    assert body["routing"]["models"] == []
    assert body["routing"]["observed_days"] == 0
    assert body["claudemd_previews"] == []


# ── POST /api/optimize/claudemd-preview ─────────────────────────────────────


@pytest.mark.asyncio
async def test_post_claudemd_preview_pure_text_in_preview_out():
    dup = "A duplicated paragraph well over the sixty-character dedupe floor."
    text = f"# Doc\n\n<!-- note -->\n\n{dup}\n\n{dup}\n"
    body = await optimize_route.post_claudemd_preview(
        ClaudeMdPreviewBody(text=text, file_label="repo/CLAUDE.md")
    )
    p = body["preview"]
    assert p["file_label"] == "repo/CLAUDE.md"
    assert p["changed"] is True
    rules = [r["rule"] for r in p["rationale"]]
    assert rules == ["strip_html_comments", "dedupe_paragraphs"]
    assert p["preview_diff"].startswith("--- repo/CLAUDE.md")
    assert "currency" in body


@pytest.mark.asyncio
async def test_post_claudemd_preview_empty_text_clean_result():
    body = await optimize_route.post_claudemd_preview(ClaudeMdPreviewBody(text=""))
    p = body["preview"]
    assert p["changed"] is False
    assert p["preview_diff"] == ""
    assert p["estimated_savings_usd_monthly"] == 0.0


@pytest.mark.asyncio
async def test_post_claudemd_preview_rejects_oversized_body():
    huge = "x" * (optimize_route.MAX_CLAUDEMD_BYTES + 1)
    with pytest.raises(HTTPException) as exc:
        await optimize_route.post_claudemd_preview(ClaudeMdPreviewBody(text=huge))
    assert exc.value.status_code == 413


@pytest.mark.asyncio
async def test_post_claudemd_preview_sessions_per_month_scales():
    dup = "A duplicated paragraph well over the sixty-character dedupe floor."
    text = f"{dup}\n\n{dup}\n"
    b100 = await optimize_route.post_claudemd_preview(ClaudeMdPreviewBody(text=text))
    b50 = await optimize_route.post_claudemd_preview(
        ClaudeMdPreviewBody(text=text, sessions_per_month=50)
    )
    assert b50["preview"]["sessions_per_month"] == 50
    assert b50["preview"]["estimated_savings_usd_monthly"] == pytest.approx(
        b100["preview"]["estimated_savings_usd_per_session"] * 50
    )

"""Provider attribution in store cost rollups (audit fix #2).

`bulk_project_cost()` and `get_global_stats()` must price each model against
its project's ACTUAL provider. Defaulting to anthropic mispriced every
non-Anthropic model — e.g. a GPT model fell back to Sonnet 3.5 rates instead
of OpenAI rates — corrupting project-list and global cost totals.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.store import db, queries, schema

_TOK = {"input": 100_000, "output": 50_000, "cache_read": 0, "cache_creation": 0}
_MSG_SQL = (
    "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
    " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
    " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
)


def _seed(store_db):
    """One claude project (Opus) + one codex project (GPT), same token counts."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'alpha', 'Alpha', 1.0, 1.0)"
    )
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (2, 'codex', 'beta', 'Beta', 1.0, 1.0)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 's1', '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 1)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (2, 2, 's2', '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 1)"
    )
    conn.execute(_MSG_SQL, (1, 0, "2026-05-01T00:00:01Z", "assistant",
                            "claude-opus-4-8", 100_000, 50_000, 0, 0,
                            "", "[]", "{}", 0, None, None))
    conn.execute(_MSG_SQL, (2, 0, "2026-05-01T00:00:01Z", "assistant",
                            "gpt-5", 100_000, 50_000, 0, 0,
                            "", "[]", "{}", 0, None, None))
    conn.commit()
    return conn


def _seed_cross_vendor(store_db):
    """Projects whose adapter provider is NOT the model's vendor.

    These pairs are observed verbatim in a real store's ``daily_mart``:
    a ``pi`` project logging ``claude-opus-4-7``, an ``opencode`` project
    logging a model no pricer knows, and (the symmetric case) a ``claude``
    project logging ``gpt-5`` through an Anthropic-shape proxy.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    rows = [
        (1, "pi", "pi-proj", "claude-opus-4-7"),
        (2, "claude", "claude-proj", "gpt-5"),
        (3, "codex", "codex-proj", "claude-opus-4-8"),
        (4, "opencode", "oc-proj", "deepseek-v4-flash-free"),
    ]
    for pid, provider, slug, model in rows:
        conn.execute(
            "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, 1.0, 1.0)",
            (pid, provider, slug, slug),
        )
        conn.execute(
            "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 1)",
            (pid, pid, f"s{pid}"),
        )
        conn.execute(_MSG_SQL, (pid, 0, "2026-05-01T00:00:01Z", "assistant",
                                model, 100_000, 50_000, 0, 0,
                                "", "[]", "{}", 0, None, None))
    conn.commit()
    return conn


def test_bulk_project_cost_prices_by_project_provider(tmp_path: Path) -> None:
    conn = _seed(tmp_path / "store.db")
    try:
        costs = queries.bulk_project_cost(conn)
    finally:
        conn.close()
    openai_price = compute_cost(_TOK, "gpt-5", provider="openai")["total_cost"]
    anthropic_fallback = compute_cost(_TOK, "gpt-5", provider="anthropic")["total_cost"]
    # The codex project (pid 2) must price as OpenAI, not the anthropic fallback.
    assert costs[2] == pytest.approx(openai_price)
    # And the two genuinely differ — proves the provider is actually threaded.
    assert openai_price != pytest.approx(anthropic_fallback)


# ── residual misattribution: the adapter provider is not the model's vendor ──
#
# ``projects.provider`` records WHICH TOOL wrote the transcript (claude, codex,
# pi, opencode …), not which vendor's rate card applies. Handing that string
# straight to ``get_pricer`` makes a terminal single-vendor pricer price a
# foreign model against its own fallback family — silently, because neither
# ``AnthropicPricer`` nor ``OpenAIPricer`` ever returns ``None``.


def test_cross_vendor_model_prices_against_the_models_own_vendor(tmp_path: Path) -> None:
    """A ``pi``/``codex`` project logging a Claude model must bill Anthropic
    rates, and a ``claude`` project logging a GPT model must bill OpenAI rates.

    Before the fix ``pi`` + ``claude-opus-4-7`` fell through OpenAIPricer's
    ``_FALLBACK`` (GPT_5_CODEX, $1.25/$10) instead of Opus 4.7's $5/$25 — a
    2.7× undercount on real rows.
    """
    conn = _seed_cross_vendor(tmp_path / "store.db")
    try:
        costs = queries.bulk_project_cost(conn)
    finally:
        conn.close()

    opus_47 = compute_cost(_TOK, "claude-opus-4-7", provider="anthropic")["total_cost"]
    opus_48 = compute_cost(_TOK, "claude-opus-4-8", provider="anthropic")["total_cost"]
    gpt5 = compute_cost(_TOK, "gpt-5", provider="openai")["total_cost"]

    assert costs[1] == pytest.approx(opus_47)
    assert costs[2] == pytest.approx(gpt5)
    assert costs[3] == pytest.approx(opus_48)

    # …and each differs from what the raw adapter-provider string produced,
    # so the assertions above can't pass by coincidence.
    assert costs[1] != pytest.approx(compute_cost(_TOK, "claude-opus-4-7", provider="pi")["total_cost"])
    assert costs[2] != pytest.approx(compute_cost(_TOK, "gpt-5", provider="claude")["total_cost"])
    assert costs[3] != pytest.approx(compute_cost(_TOK, "claude-opus-4-8", provider="codex")["total_cost"])


def test_unknown_model_keeps_the_adapter_providers_verdict(tmp_path: Path) -> None:
    """A model no pricer claims must NOT be re-routed to Anthropic's fallback.

    ``opencode`` deliberately returns ``None`` for vendors it can't identify
    (0.0, "we don't know") rather than inventing Sonnet dollars. The vendor
    override must only fire on a *definite* model→vendor match, never on
    ``_provider_for_model``'s conservative ``anthropic`` default.
    """
    conn = _seed_cross_vendor(tmp_path / "store.db")
    try:
        costs = queries.bulk_project_cost(conn)
    finally:
        conn.close()
    assert costs[4] == pytest.approx(0.0)


def test_shell_provider_estimate_survives_when_the_vendor_cannot_price(tmp_path: Path) -> None:
    """Never trade a real number for "I don't know".

    ``cursor`` prices dated Gemini preview ids at its Sonnet-tier estimate;
    ``GeminiPricer`` returns ``None`` for the same id. The override must keep
    Cursor's estimate rather than zeroing the row.
    """
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'cursor', 'cur', 'Cur', 1.0, 1.0)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 's1', '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 1)"
    )
    conn.execute(_MSG_SQL, (1, 0, "2026-05-01T00:00:01Z", "assistant",
                            "gemini-2.5-pro-preview-05-06", 100_000, 50_000, 0, 0,
                            "", "[]", "{}", 0, None, None))
    conn.commit()
    try:
        costs = queries.bulk_project_cost(conn)
    finally:
        conn.close()
    cursor_estimate = compute_cost(
        _TOK, "gemini-2.5-pro-preview-05-06", provider="cursor"
    )["total_cost"]
    assert cursor_estimate > 0.0
    assert costs[1] == pytest.approx(cursor_estimate)


def test_global_stats_prices_by_provider(tmp_path: Path) -> None:
    conn = _seed(tmp_path / "store.db")
    try:
        stats = queries.get_global_stats(conn)
    finally:
        conn.close()
    openai_price = compute_cost(_TOK, "gpt-5", provider="openai")["total_cost"]
    assert stats["models"]["gpt-5"]["cost"] == pytest.approx(openai_price)

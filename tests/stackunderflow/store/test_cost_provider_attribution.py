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


def test_global_stats_prices_by_provider(tmp_path: Path) -> None:
    conn = _seed(tmp_path / "store.db")
    try:
        stats = queries.get_global_stats(conn)
    finally:
        conn.close()
    openai_price = compute_cost(_TOK, "gpt-5", provider="openai")["total_cost"]
    assert stats["models"]["gpt-5"]["cost"] == pytest.approx(openai_price)

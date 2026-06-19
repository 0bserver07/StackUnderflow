"""Tests for the v0.6.2 provider/model filter wiring.

Three surfaces gain `?provider=` (and ``?model=`` where applicable) in this
PR — verify each in isolation:

* ``GET /api/providers`` (new) — provider catalogue with project + session
  counts, used by the dashboard's `FilterBar` chip row.
* ``GET /api/projects?provider=cursor`` — narrows the project list to those
  providers; empty filter = preserve existing all-projects behaviour.
* ``GET /api/cost-data/by-provider?provider=cursor`` — narrows the per-
  provider rollup rows to the requested set.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes.cost import get_cost_by_provider
from stackunderflow.routes.projects import get_projects, get_providers
from stackunderflow.store import db, schema

# ── seeding helper ──────────────────────────────────────────────────────────


def _seed(store_db, *, projects, messages):
    """Mirror the seed helper used by ``test_compare.py`` / ``test_cost_by_provider.py``.

    Each ``messages[]`` entry is a dict with ``project_slug, session_id,
    timestamp, role`` plus optional ``provider``, ``model``, ``in_tok``,
    ``out_tok``, ``cache_w``, ``cache_r``.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    project_pk: dict[tuple[str, str], int] = {}
    for prov, slug in projects:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_pk[(prov, slug)] = cur.lastrowid
    sess_pk: dict = {}
    seq_counter: dict[int, int] = {}
    for m in messages:
        prov = m.get("provider", "claude")
        slug = m["project_slug"]
        ppk = project_pk[(prov, slug)]
        sk = (ppk, m["session_id"])
        if sk not in sess_pk:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (ppk, m["session_id"], m["timestamp"], m["timestamp"], 0),
            )
            sess_pk[sk] = cur.lastrowid
        sfk = sess_pk[sk]
        seq = seq_counter.get(sfk, 0)
        seq_counter[sfk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sfk, seq, m["timestamp"], m["role"], m.get("model"),
                m.get("in_tok", 0), m.get("out_tok", 0),
                m.get("cache_w", 0), m.get("cache_r", 0),
                "", "[]", "{}", 0, None, None,
            ),
        )
    conn.commit()
    conn.close()


# ── /api/providers ──────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_providers_returns_one_row_per_provider(tmp_path, monkeypatch):
    """Multi-provider store → one entry per provider with counts."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma"), ("cursor", "delta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A2",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "delta", "provider": "cursor",
             "session_id": "D1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_providers()
    payload = response.body.decode("utf-8")
    import json
    body = json.loads(payload)

    providers = {p["provider"]: p for p in body["providers"]}
    # 3 distinct providers, with the right session counts.
    assert set(providers) == {"claude", "codex", "cursor"}
    assert providers["claude"]["project_count"] == 1
    assert providers["claude"]["session_count"] == 2
    assert providers["codex"]["session_count"] == 1
    assert providers["cursor"]["session_count"] == 1


@pytest.mark.asyncio
async def test_providers_empty_store_returns_empty_array(tmp_path, monkeypatch):
    """No projects → empty list, never a 500."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_providers()
    import json
    body = json.loads(response.body.decode("utf-8"))
    assert body["providers"] == []


# ── /api/projects?provider= ─────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_projects_provider_filter_narrows_list(tmp_path, monkeypatch):
    """``?provider=cursor`` → only cursor projects in the response."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[
            ("claude", "alpha"),
            ("codex", "gamma"),
            ("cursor", "delta"),
        ],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "delta", "provider": "cursor",
             "session_id": "D1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    # Filtered → only cursor.
    response = await get_projects(provider=["cursor"])
    import json
    body = json.loads(response.body.decode("utf-8"))
    slugs = {p["dir_name"] for p in body["projects"]}
    assert slugs == {"delta"}


@pytest.mark.asyncio
async def test_projects_empty_provider_filter_preserves_all(tmp_path, monkeypatch):
    """Empty filter (None) → every project in the store. Backwards-compat."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    # No filter → both projects survive.
    response = await get_projects()
    import json
    body = json.loads(response.body.decode("utf-8"))
    slugs = {p["dir_name"] for p in body["projects"]}
    assert slugs == {"alpha", "gamma"}


@pytest.mark.asyncio
async def test_projects_provider_filter_is_case_insensitive(tmp_path, monkeypatch):
    """``?provider=Cursor`` accepted alongside the lowercase canonical form."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("cursor", "delta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "delta", "provider": "cursor",
             "session_id": "D1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    response = await get_projects(provider=["Cursor"])
    import json
    body = json.loads(response.body.decode("utf-8"))
    slugs = {p["dir_name"] for p in body["projects"]}
    assert slugs == {"delta"}


@pytest.mark.asyncio
async def test_projects_repeated_provider_filter_unions(tmp_path, monkeypatch):
    """Repeated ``?provider=`` → union semantics."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[
            ("claude", "alpha"),
            ("codex", "gamma"),
            ("cursor", "delta"),
        ],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "delta", "provider": "cursor",
             "session_id": "D1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    response = await get_projects(provider=["cursor", "codex"])
    import json
    body = json.loads(response.body.decode("utf-8"))
    slugs = {p["dir_name"] for p in body["projects"]}
    assert slugs == {"delta", "gamma"}


# ── /api/cost-data/by-provider?provider= ───────────────────────────────────


@pytest.mark.asyncio
async def test_cost_by_provider_filter_narrows_rows(tmp_path, monkeypatch):
    """Provider filter on the cost rollup → only the matching rows survive."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma"), ("cursor", "delta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-sonnet-4-5", "in_tok": 100, "out_tok": 50},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "assistant",
             "model": "gpt-5", "in_tok": 100, "out_tok": 50},
            {"project_slug": "delta", "provider": "cursor",
             "session_id": "D1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "assistant",
             "model": "claude-4.5-sonnet", "in_tok": 50, "out_tok": 25},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    payload = await get_cost_by_provider(period="all", provider=["cursor"])
    rows = payload["rows"]
    # Only one row, scoped to cursor.
    assert len(rows) == 1
    assert rows[0]["provider"] == "cursor"


@pytest.mark.asyncio
async def test_cost_by_provider_empty_filter_preserves_all_rows(tmp_path, monkeypatch):
    """``provider=[]``-equivalent (None) → original "one row per provider"."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-sonnet-4-5", "in_tok": 100, "out_tok": 50},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:01Z", "role": "assistant",
             "model": "gpt-5", "in_tok": 100, "out_tok": 50},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    payload = await get_cost_by_provider(period="all")
    providers_in_response = {r["provider"] for r in payload["rows"]}
    assert providers_in_response == {"claude", "codex"}


# ── multi-provider SAME-slug filter (audit fix #8 / #3) ──────────────────────


def test_filtered_project_ids_checks_every_provider_row(tmp_path):
    """The helper must narrow by provider across ALL rows for a slug, not the
    first. Regression for the get_project()/fetchone first-row-wins bug."""
    from stackunderflow.routes.data import _filtered_project_ids

    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "shared"), ("codex", "shared")],  # same slug, 2 providers
        messages=[
            {"project_slug": "shared", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "shared", "provider": "codex", "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
        ],
    )
    conn = db.connect(store_db)
    try:
        claude_ids = _filtered_project_ids(conn, "/x/shared", {"claude"})
        codex_ids = _filtered_project_ids(conn, "/x/shared", {"codex"})
        all_ids = _filtered_project_ids(conn, "/x/shared", None)
        none_ids = _filtered_project_ids(conn, "/x/shared", {"cursor"})
    finally:
        conn.close()
    assert len(claude_ids) == 1 and len(codex_ids) == 1
    assert claude_ids != codex_ids
    assert set(all_ids) == set(claude_ids) | set(codex_ids)
    assert none_ids == []  # excluded → empty list, NOT a 404


@pytest.mark.asyncio
async def test_messages_multi_provider_same_slug(tmp_path, monkeypatch):
    """A slug shared across providers: ?provider= returns THAT provider's
    messages — not empty because a different provider's row sorted first."""
    from stackunderflow.routes.data import get_messages

    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "shared"), ("codex", "shared")],
        messages=[
            {"project_slug": "shared", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-opus-4-8", "in_tok": 100, "out_tok": 50},
            {"project_slug": "shared", "provider": "codex", "session_id": "C1",
             "timestamp": "2026-04-02T10:00:01Z", "role": "assistant",
             "model": "gpt-5", "in_tok": 100, "out_tok": 50},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/x/shared")

    # codex is NOT the first-inserted row (claude is) — the old bug returned empty.
    codex = await get_messages(provider=["codex"])
    assert codex["total"] >= 1
    claude = await get_messages(provider=["claude"])
    assert claude["total"] >= 1
    # A provider not present for this slug → shape-stable empty page.
    absent = await get_messages(provider=["cursor"])
    assert absent["total"] == 0

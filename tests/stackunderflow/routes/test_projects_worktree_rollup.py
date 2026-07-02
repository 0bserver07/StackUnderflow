"""Campaign #8 — worktree attribution roll-up on ``GET /api/projects``.

Sessions run inside git worktrees log under phantom sibling slugs
(``<parent>--worktrees-<x>``, ``<parent>--claude-worktrees-<x>``), which
fragment per-project analytics. The list endpoint now folds those fragments
into their parent row by default — the parent gains ``worktree_sessions`` /
``worktree_cost`` / ``worktree_count`` — while ``?include_worktrees=1``
returns the raw list with ``worktree_of`` annotations for frontend badging.

Attribution sources, in order: the v027 ``projects.worktree_of`` column
(feature-detected — simulated here with an ``ALTER TABLE`` on the tmp store)
and the slug-shape fallback. All stores are ``tmp_path`` — never the real one.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes.projects import get_projects, set_project_by_dir
from stackunderflow.store import db, schema

PARENT = "-Users-me-dev-repo"
FRAG_SHAPE = "-Users-me-dev-repo--claude-worktrees-todo-cleanup"
FRAG_SHAPE_2 = "-Users-me-dev-repo--worktrees-all-issues"
FRAG_COLUMN = "-Users-me-dev-repo-agent-scratch"  # not worktree-shaped: column-attributed only


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _add_worktree_of_column(conn):
    """Ensure the v027 column exists (no-op post-v027, where schema.apply
    already created it — this helper predates the migration landing)."""
    cols = {row[1] for row in conn.execute("PRAGMA table_info(projects)").fetchall()}
    if "worktree_of" not in cols:
        conn.execute("ALTER TABLE projects ADD COLUMN worktree_of TEXT")


def _insert_project(conn, *, provider="claude", slug, last_modified=0.0, path=None, worktree_of=None):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (provider, slug, path, slug, 0.0, last_modified),
    )
    pid = int(cur.lastrowid)
    if worktree_of is not None:
        conn.execute("UPDATE projects SET worktree_of = ? WHERE id = ?", (worktree_of, pid))
    return pid


def _insert_sessions(conn, project_id, n):
    for i in range(n):
        conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
            (project_id, f"s-{project_id}-{i}"),
        )


def _insert_project_mart(conn, *, project_id, provider="claude", slug, total_cost_usd=0.0, **kw):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd, "
        " total_user_messages, total_assistant_messages, total_tool_use_messages, "
        " total_tool_result_messages, total_commands) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            project_id,
            provider,
            slug,
            slug,
            kw.get("first_ts"),
            kw.get("last_ts"),
            kw.get("total_messages", 0),
            kw.get("total_sessions", 0),
            kw.get("total_input_tokens", 0),
            kw.get("total_output_tokens", 0),
            kw.get("total_cache_read", 0),
            kw.get("total_cache_create", 0),
            total_cost_usd,
            kw.get("total_user_messages", 0),
            kw.get("total_assistant_messages", 0),
            kw.get("total_tool_use_messages", 0),
            kw.get("total_tool_result_messages", 0),
            kw.get("total_commands", 0),
        ),
    )


def _seed_parent_and_fragments(store_db, *, with_worktree_column=True):
    """Parent + one column-attributed fragment + one shape fragment + a bystander.

    Fragments carry 2 and 3 sessions and mart costs 0.5 and 0.25 USD, so the
    expected roll-up is worktree_sessions=5, worktree_cost=0.75, worktree_count=2.
    """
    conn = _connect(store_db)
    if with_worktree_column:
        _add_worktree_of_column(conn)
    parent_pid = _insert_project(conn, slug=PARENT, last_modified=10.0)
    frag_col_pid = _insert_project(
        conn,
        slug=FRAG_COLUMN,
        last_modified=9.0,
        worktree_of=PARENT if with_worktree_column else None,
    )
    frag_shape_pid = _insert_project(conn, slug=FRAG_SHAPE, last_modified=8.0)
    _insert_project(conn, slug="-Users-me-dev-unrelated", last_modified=7.0)
    _insert_sessions(conn, parent_pid, 4)
    _insert_sessions(conn, frag_col_pid, 2)
    _insert_sessions(conn, frag_shape_pid, 3)
    _insert_project_mart(conn, project_id=parent_pid, slug=PARENT, total_cost_usd=2.0)
    _insert_project_mart(conn, project_id=frag_col_pid, slug=FRAG_COLUMN, total_cost_usd=0.5)
    _insert_project_mart(conn, project_id=frag_shape_pid, slug=FRAG_SHAPE, total_cost_usd=0.25)
    conn.commit()
    conn.close()


async def _call(**kw):
    response = await get_projects(**kw)
    return json.loads(response.body.decode("utf-8"))


# ── default: fragments fold into the parent row ───────────────────────────────


@pytest.mark.asyncio
async def test_fold_rolls_both_fragment_kinds_into_parent(tmp_path, monkeypatch):
    """One fragment via the v027 column, one via slug shape → both fold."""
    store_db = tmp_path / "store.db"
    _seed_parent_and_fragments(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call()

    slugs = [p["dir_name"] for p in body["projects"]]
    assert FRAG_COLUMN not in slugs
    assert FRAG_SHAPE not in slugs
    assert body["total_count"] == 2  # parent + bystander, fragments folded

    parent = next(p for p in body["projects"] if p["dir_name"] == PARENT)
    assert parent["worktree_count"] == 2
    assert parent["worktree_sessions"] == 5  # 2 (column frag) + 3 (shape frag)
    assert parent["worktree_cost"] == pytest.approx(0.75)  # 0.5 + 0.25 USD
    # The parent's own numbers are untouched by the fold.
    assert parent["file_count"] == 4

    bystander = next(p for p in body["projects"] if p["dir_name"] == "-Users-me-dev-unrelated")
    for key in ("worktree_count", "worktree_sessions", "worktree_cost", "worktree_of"):
        assert key not in bystander


@pytest.mark.asyncio
async def test_fold_with_include_stats_keeps_mart_costs_and_stats(tmp_path, monkeypatch):
    """include_stats=true reuses the already-loaded mart rows for the roll-up."""
    store_db = tmp_path / "store.db"
    _seed_parent_and_fragments(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(include_stats=True)

    parent = next(p for p in body["projects"] if p["dir_name"] == PARENT)
    assert parent["worktree_cost"] == pytest.approx(0.75)
    assert parent["worktree_sessions"] == 5
    # Parent's own stats stay its own — the fragments' cost lives in the
    # explicit worktree_cost breakout, not silently merged into total_cost.
    assert parent["stats"]["total_cost"] == pytest.approx(2.0)


# ── ?include_worktrees=1: raw list with annotations ──────────────────────────


@pytest.mark.asyncio
async def test_include_worktrees_returns_raw_list_with_annotations(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_parent_and_fragments(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(include_worktrees=True)

    by_slug = {p["dir_name"]: p for p in body["projects"]}
    assert body["total_count"] == 4  # nothing folded
    assert by_slug[FRAG_COLUMN]["worktree_of"] == PARENT
    assert by_slug[FRAG_SHAPE]["worktree_of"] == PARENT
    # Parent is not badged and carries no roll-up fields in raw mode.
    for key in ("worktree_of", "worktree_count", "worktree_sessions", "worktree_cost"):
        assert key not in by_slug[PARENT]
    assert "worktree_of" not in by_slug["-Users-me-dev-unrelated"]


# ── unmatched fragments: never fold into a parent that isn't listed ──────────


@pytest.mark.asyncio
async def test_orphan_fragments_stay_visible_unfolded(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _add_worktree_of_column(conn)
    # Shape-orphan: worktree-shaped slug whose parent slug is not in the store.
    _insert_project(conn, slug="-Users-me-ghost--worktrees-z", last_modified=2.0)
    # Column-orphan: v027 attribution pointing at a parent that is not listed.
    _insert_project(conn, slug="frag-col-orphan", last_modified=1.0, worktree_of="-Users-me-ghost2")
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call()

    slugs = {p["dir_name"] for p in body["projects"]}
    assert slugs == {"-Users-me-ghost--worktrees-z", "frag-col-orphan"}
    assert body["total_count"] == 2
    for proj in body["projects"]:
        for key in ("worktree_count", "worktree_sessions", "worktree_cost"):
            assert key not in proj


@pytest.mark.asyncio
async def test_raw_mode_annotates_column_orphan_but_not_shape_orphan(tmp_path, monkeypatch):
    """The v027 column is authoritative → badge even without a listed parent;
    a shape-derived match needs its parent in the listing universe."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _add_worktree_of_column(conn)
    _insert_project(conn, slug="-Users-me-ghost--worktrees-z", last_modified=2.0)
    _insert_project(conn, slug="frag-col-orphan", last_modified=1.0, worktree_of="-Users-me-ghost2")
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(include_worktrees=True)

    by_slug = {p["dir_name"]: p for p in body["projects"]}
    assert by_slug["frag-col-orphan"]["worktree_of"] == "-Users-me-ghost2"
    assert "worktree_of" not in by_slug["-Users-me-ghost--worktrees-z"]


@pytest.mark.asyncio
async def test_provider_filter_scopes_the_listing_universe(tmp_path, monkeypatch):
    """A fragment whose parent is filtered out by ?provider= stays visible —
    folding only targets parents in the SAME (filtered) listing universe."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_project(conn, provider="codex", slug=PARENT, last_modified=2.0)
    _insert_project(conn, provider="claude", slug=FRAG_SHAPE, last_modified=1.0)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    scoped = await _call(provider=["claude"])
    assert [p["dir_name"] for p in scoped["projects"]] == [FRAG_SHAPE]
    assert scoped["total_count"] == 1
    assert "worktree_count" not in scoped["projects"][0]

    unscoped = await _call()
    assert [p["dir_name"] for p in unscoped["projects"]] == [PARENT]
    assert unscoped["projects"][0]["worktree_count"] == 1


# ── ordering proof: fold happens BEFORE the pagination slice ──────────────────


@pytest.mark.asyncio
async def test_fold_happens_before_pagination_slice(tmp_path, monkeypatch):
    """total_count / has_more / page walks count FOLDED rows. If the fold ran
    after the slice, total_count would be 5 and fragments would leak into
    pages — both assertions below would fail."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_project(conn, slug="proj-a", last_modified=5.0)
    _insert_project(conn, slug="proj-b", last_modified=4.0)
    _insert_project(conn, slug=PARENT, last_modified=3.0)
    _insert_project(conn, slug=FRAG_SHAPE, last_modified=2.0)
    _insert_project(conn, slug=FRAG_SHAPE_2, last_modified=1.0)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    page1 = await _call(limit=2)
    assert page1["total_count"] == 3  # 5 slugs, 2 folded away
    assert page1["has_more"] is True
    assert [p["dir_name"] for p in page1["projects"]] == ["proj-a", "proj-b"]

    page2 = await _call(limit=2, offset=2)
    assert [p["dir_name"] for p in page2["projects"]] == [PARENT]
    assert page2["projects"][0]["worktree_count"] == 2
    assert page2["has_more"] is False

    # Walking every page never surfaces a fragment slug.
    seen: list[str] = []
    offset = 0
    while True:
        body = await _call(limit=2, offset=offset)
        seen.extend(p["dir_name"] for p in body["projects"])
        if not body["has_more"]:
            break
        offset += body["limit"]
    assert sorted(seen) == [PARENT, "proj-a", "proj-b"]


# ── pre-migration store: no v027 column → shape fallback only, no error ──────


@pytest.mark.asyncio
async def test_pre_migration_store_folds_via_shape_fallback_only(tmp_path, monkeypatch):
    """schema v026 has no ``projects.worktree_of`` — the guarded reader must
    detect that and carry the classification on slug shape alone. Fragment
    cost resolves through the bulk messages fallback (no mart rows seeded)."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    # schema.apply is at v027+ now, so genuinely simulate a pre-migration
    # store by dropping the column it added (SQLite ≥3.35 supports this).
    conn.execute("ALTER TABLE projects DROP COLUMN worktree_of")
    parent_pid = _insert_project(conn, slug=PARENT, last_modified=2.0)
    frag_pid = _insert_project(conn, slug=FRAG_SHAPE, last_modified=1.0)
    _insert_sessions(conn, parent_pid, 1)
    sid = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, 'frag-s1')", (frag_pid,)
    ).lastrowid
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, raw_json) "
        "VALUES (?, 1, '2026-05-01T10:00:01+00:00', 'assistant', 'claude-sonnet-4-5', 1000, 500, '{}')",
        (sid,),
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call()

    assert "error" not in body
    slugs = [p["dir_name"] for p in body["projects"]]
    assert slugs == [PARENT]
    parent = body["projects"][0]
    assert parent["worktree_count"] == 1
    assert parent["worktree_sessions"] == 1
    assert parent["worktree_cost"] > 0.0  # priced via the bulk fallback path


# ── currency: worktree_cost converts like every other cost field ─────────────


@pytest.mark.asyncio
async def test_worktree_cost_converts_with_active_currency(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_parent_and_fragments(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr(
        "stackunderflow.routes.projects.active_currency_payload",
        lambda: {"currency": "GBP", "symbol": "£", "rate_from_usd": 2.0},
    )

    body = await _call()  # include_stats off — conversion must still apply

    parent = next(p for p in body["projects"] if p["dir_name"] == PARENT)
    assert parent["worktree_cost"] == pytest.approx(1.5)  # 0.75 USD × 2.0


# ── deep links: folding affects the LIST only ─────────────────────────────────


@pytest.mark.asyncio
async def test_fragment_deep_link_still_resolves(tmp_path, monkeypatch):
    """POST /api/project-by-dir with a fragment slug keeps working — the fold
    never hides a fragment from direct by-slug resolution."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _add_worktree_of_column(conn)
    _insert_project(conn, slug=PARENT, last_modified=2.0, path=str(tmp_path / "repo-logs"))
    _insert_project(
        conn,
        slug=FRAG_SHAPE,
        last_modified=1.0,
        path=str(tmp_path / "frag-logs"),
        worktree_of=PARENT,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_project_path", None)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    monkeypatch.setattr("stackunderflow.deps.search_service", None)

    # The fragment is folded out of the default list...
    body = await _call()
    assert FRAG_SHAPE not in [p["dir_name"] for p in body["projects"]]

    # ...but resolves directly by slug.
    response = await set_project_by_dir({"dir_name": FRAG_SHAPE})
    resolved = json.loads(response.body.decode("utf-8"))
    assert resolved["status"] == "success"
    assert resolved["log_dir_name"] == FRAG_SHAPE

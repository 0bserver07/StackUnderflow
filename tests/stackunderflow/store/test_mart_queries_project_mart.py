"""PROJ-4 — ``list_project_mart`` id scoping, and its empty-sequence contract.

``GET /api/projects`` narrows this read to the ids on the page it is about to
return. The dangerous edge is the *empty* scope: an offset past the end of the
list is a legitimate request whose page has no ids at all. If an empty sequence
were promoted to "all" (the trap ``queries._scoped_rows`` documents), that
request would quietly read the whole mart — the exact coupling the page scoping
exists to remove. ``None`` still means "every project" for callers that want it.

Rows are inserted directly so the reader is exercised in isolation from the
builder. All stores are ``tmp_path`` — never the real one.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from stackunderflow.store import db, mart_queries, schema


def _connect(tmp_path: Path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed(conn, rows: list[tuple[int, str, str]]) -> None:
    """rows = [(project_id, provider, slug), ...]."""
    conn.executemany(
        "INSERT INTO project_mart (project_id, provider, slug, display_name) "
        "VALUES (?, ?, ?, ?)",
        [(pid, provider, slug, slug) for pid, provider, slug in rows],
    )
    conn.commit()


def _ids(rows) -> set[int]:
    return {int(r["project_id"]) for r in rows}


@pytest.fixture()
def conn(tmp_path):
    c = _connect(tmp_path)
    _seed(c, [(1, "claude", "-a"), (2, "claude", "-b"), (3, "codex", "-c")])
    yield c
    c.close()


def test_none_means_every_project(conn):
    assert _ids(mart_queries.list_project_mart(conn)) == {1, 2, 3}
    assert _ids(mart_queries.list_project_mart(conn, project_ids=None)) == {1, 2, 3}


def test_empty_sequence_means_no_rows_never_all(conn):
    """The whole point of the parameter: [] is a scope, not an absence."""
    assert mart_queries.list_project_mart(conn, project_ids=[]) == []
    assert mart_queries.list_project_mart(conn, project_ids=()) == []
    assert mart_queries.list_project_mart(conn, project_ids=set()) == []
    # ...and it wins even when the provider filter would have matched rows.
    assert mart_queries.list_project_mart(conn, provider_filter={"claude"}, project_ids=[]) == []


def test_ids_scope_the_read(conn):
    assert _ids(mart_queries.list_project_mart(conn, project_ids=[2])) == {2}
    assert _ids(mart_queries.list_project_mart(conn, project_ids=[1, 3])) == {1, 3}
    # Unknown ids are simply absent — not an error, not a full read.
    assert mart_queries.list_project_mart(conn, project_ids=[999]) == []


def test_id_and_provider_filters_and_together(conn):
    """Both filters given → intersection, never one silently winning."""
    both = mart_queries.list_project_mart(
        conn, provider_filter={"claude"}, project_ids=[1, 3]
    )
    assert _ids(both) == {1}  # 3 is codex; 2 is out of scope
    assert (
        mart_queries.list_project_mart(conn, provider_filter={"codex"}, project_ids=[1, 2])
        == []
    )


def test_missing_table_returns_empty(tmp_path):
    conn = db.connect(tmp_path / "bare.db")
    try:
        assert mart_queries.list_project_mart(conn, project_ids=[1]) == []
    finally:
        conn.close()

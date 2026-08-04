"""``per_page`` is a divisor — ``/api/search`` and ``/api/qa`` refuse a bad one.

Both routes hand ``per_page`` to a service that computes the page count as
``(total + per_page - 1) // per_page``:

* ``?per_page=0`` raised ``ZeroDivisionError`` inside the handler's blanket
  ``except`` and came back as a **500** — a live crash on a query string.
* ``?per_page=-5`` never divided by zero but reached SQLite as a negative
  ``LIMIT``, which SQLite reads as *no limit at all*: the whole result set,
  with a nonsense ``total_pages`` from CPython's flooring ``//``.

Both are now refused by FastAPI's own validation (``Query(20, ge=1)``) before
the handler body runs, so the 422 body is pydantic's, not a hand-rolled
imitation. The ceiling is deliberately *not* a constraint: an over-large
``per_page`` has always been silently clamped to 100 and still is.
"""

from __future__ import annotations

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.qa import router as qa_router
from stackunderflow.routes.search import router as search_router


class _StubService:
    """Records the kwargs it was called with and returns an empty envelope."""

    def __init__(self) -> None:
        self.calls: list[dict] = []

    def _envelope(self, **kwargs) -> dict:
        self.calls.append(kwargs)
        return {
            "results": [],
            "total": 0,
            "page": kwargs.get("page", 1),
            "per_page": kwargs.get("per_page", 20),
            "total_pages": 0,
        }

    search = _envelope
    list_qa = _envelope


@pytest.fixture()
def client(monkeypatch):
    """Both routers mounted with stub services (no index, no store needed)."""
    stub = _StubService()
    monkeypatch.setattr(deps, "search_service", stub)
    monkeypatch.setattr(deps, "qa_service", stub)

    app = FastAPI()
    app.include_router(search_router)
    app.include_router(qa_router)
    return TestClient(app), stub


ENDPOINTS = ["/api/search", "/api/qa"]


class TestPerPageFloor:
    @pytest.mark.parametrize("path", ENDPOINTS)
    def test_zero_is_a_422_not_a_500(self, client, path):
        c, stub = client
        r = c.get(path, params={"per_page": 0})
        assert r.status_code == 422
        detail = r.json()["detail"]
        assert detail == [
            {
                "type": "greater_than_equal",
                "loc": ["query", "per_page"],
                "msg": "Input should be greater than or equal to 1",
                "input": "0",
                "ctx": {"ge": 1},
            }
        ]
        # Rejected at validation time — the service was never reached.
        assert stub.calls == []

    @pytest.mark.parametrize("path", ENDPOINTS)
    def test_negative_is_a_422(self, client, path):
        c, stub = client
        r = c.get(path, params={"per_page": -5})
        assert r.status_code == 422
        detail = r.json()["detail"]
        assert [d["type"] for d in detail] == ["greater_than_equal"]
        assert detail[0]["input"] == "-5"
        assert stub.calls == []

    @pytest.mark.parametrize("path", ENDPOINTS)
    def test_one_is_accepted(self, client, path):
        c, stub = client
        r = c.get(path, params={"per_page": 1})
        assert r.status_code == 200
        assert stub.calls[-1]["per_page"] == 1

    @pytest.mark.parametrize("path", ENDPOINTS)
    def test_default_is_twenty(self, client, path):
        c, stub = client
        assert c.get(path).status_code == 200
        assert stub.calls[-1]["per_page"] == 20

    @pytest.mark.parametrize("path", ENDPOINTS)
    def test_ceiling_still_clamps_silently(self, client, path):
        """The 100 cap is a clamp, not a constraint — 500 is served as 100."""
        c, stub = client
        r = c.get(path, params={"per_page": 500})
        assert r.status_code == 200
        assert stub.calls[-1]["per_page"] == 100

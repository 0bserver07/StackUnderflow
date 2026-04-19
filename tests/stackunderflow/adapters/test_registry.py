from pathlib import Path

from stackunderflow import adapters
from stackunderflow.adapters.base import Record, SessionRef


class _FakeAdapter:
    name = "fake"

    def enumerate(self):
        return []

    def read(self, ref, *, since_offset=0):
        return []


def test_register_and_list():
    before = len(adapters.registered())
    adapters.register(_FakeAdapter())
    after = adapters.registered()
    assert len(after) == before + 1
    assert after[-1].name == "fake"


def test_registered_returns_copy():
    snapshot = adapters.registered()
    snapshot.append(_FakeAdapter())  # mutation must not leak
    assert len(adapters.registered()) < len(snapshot)

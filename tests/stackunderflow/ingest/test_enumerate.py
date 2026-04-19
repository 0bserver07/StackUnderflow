from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest.enumerate import iter_refs


class _StubAdapter:
    name = "stub"

    def __init__(self, refs):
        self._refs = refs

    def enumerate(self):
        yield from self._refs

    def read(self, ref, *, since_offset=0):
        return []


def test_iter_refs_fans_out_adapters():
    a = _StubAdapter([
        SessionRef("stub", "-a", "s1", Path("/a"), 0, 0),
        SessionRef("stub", "-a", "s2", Path("/b"), 0, 0),
    ])
    b = _StubAdapter([
        SessionRef("stub", "-b", "s3", Path("/c"), 0, 0),
    ])
    out = list(iter_refs([a, b]))
    assert len(out) == 3
    assert {r.session_id for r in out} == {"s1", "s2", "s3"}


def test_iter_refs_empty_list():
    assert list(iter_refs([])) == []

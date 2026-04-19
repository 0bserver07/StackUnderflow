"""Reusable contract any adapter implementation must satisfy.

Subclass `AdapterContract` in a concrete test module, set `adapter` to
an instance under test, and the mixin runs a shared set of invariants.
"""

from __future__ import annotations

from stackunderflow.adapters.base import Record, SessionRef


class AdapterContract:
    """Mixin. Subclasses must set `self.adapter` in setUp/fixture."""

    adapter = None  # subclass must override

    def test_has_name(self):
        assert isinstance(self.adapter.name, str)
        assert self.adapter.name

    def test_enumerate_yields_session_refs(self):
        refs = list(self.adapter.enumerate())
        for r in refs:
            assert isinstance(r, SessionRef)
            assert r.provider == self.adapter.name

    def test_read_yields_records_with_monotonic_seq(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return  # empty fixture is acceptable for the contract
        prior = -1
        for rec in self.adapter.read(refs[0]):
            assert isinstance(rec, Record)
            assert rec.provider == self.adapter.name
            assert rec.seq > prior
            prior = rec.seq

    def test_read_records_have_non_negative_tokens(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        for rec in self.adapter.read(refs[0]):
            assert rec.input_tokens >= 0
            assert rec.output_tokens >= 0
            assert rec.cache_create_tokens >= 0
            assert rec.cache_read_tokens >= 0

    def test_read_records_have_iso_timestamps(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        from datetime import datetime
        for rec in self.adapter.read(refs[0]):
            # must parse as ISO 8601
            datetime.fromisoformat(rec.timestamp.replace("Z", "+00:00"))

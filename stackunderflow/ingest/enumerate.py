"""Fans every registered adapter's SessionRefs into one iterable."""

from __future__ import annotations

from typing import Iterable

from stackunderflow.adapters.base import SessionRef, SourceAdapter


def iter_refs(adapters: list[SourceAdapter]) -> Iterable[SessionRef]:
    for adapter in adapters:
        yield from adapter.enumerate()

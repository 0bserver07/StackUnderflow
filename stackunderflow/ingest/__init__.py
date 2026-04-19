"""Ingest engine: drives adapters into the store."""

from __future__ import annotations

import sqlite3

from stackunderflow.adapters.base import SourceAdapter

from .enumerate import iter_refs
from .writer import ingest_file

__all__ = ["iter_refs", "ingest_file", "run_ingest"]


def run_ingest(conn: sqlite3.Connection, adapters: list[SourceAdapter]) -> dict[str, int]:
    """Run one ingest pass across *adapters*.

    For each file, compare (mtime, size) against ingest_log and either
    skip, tail-read, or full-reparse. Returns per-provider new-record
    counts (handy for logging).
    """
    counts: dict[str, int] = {}
    for ref in iter_refs(adapters):
        prior = conn.execute(
            "SELECT mtime, size, processed_offset FROM ingest_log WHERE file_path = ?",
            (str(ref.file_path),),
        ).fetchone()

        if prior and prior["mtime"] == ref.file_mtime and prior["size"] == ref.file_size:
            continue  # unchanged

        if prior and ref.file_size < prior["size"]:
            # Truncation / rotation — full reparse from 0
            conn.execute("DELETE FROM ingest_log WHERE file_path = ?", (str(ref.file_path),))
            since = 0
        else:
            since = prior["processed_offset"] if prior else 0

        adapter = _lookup(adapters, ref.provider)
        pre = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        ingest_file(conn, adapter, ref, since_offset=since)
        post = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        counts[ref.provider] = counts.get(ref.provider, 0) + (post - pre)

    return counts


def _lookup(adapters: list[SourceAdapter], name: str) -> SourceAdapter:
    for a in adapters:
        if a.name == name:
            return a
    raise KeyError(f"No adapter registered for provider {name!r}")

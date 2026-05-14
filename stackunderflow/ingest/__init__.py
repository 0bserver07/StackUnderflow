"""Ingest engine: drives adapters into the store."""

from __future__ import annotations

import logging
import sqlite3

from stackunderflow.adapters.base import SourceAdapter

from .enumerate import iter_refs
from .writer import ingest_file

__all__ = ["iter_refs", "ingest_file", "run_ingest", "auto_reindex_touched"]

_logger = logging.getLogger(__name__)


def run_ingest(conn: sqlite3.Connection, adapters: list[SourceAdapter]) -> dict[str, int]:
    """Run one ingest pass across *adapters*.

    For each file, compare (mtime, size) against ingest_log and either
    skip, tail-read, or full-reparse. Returns per-provider new-record
    counts (handy for logging).

    After all files are processed, automatically refreshes the search,
    tag, and Q&A indexes for any project that gained new messages,
    unless the ``auto_reindex_on_ingest`` setting is disabled. Each
    service is called in its own try/except so a beta-feature failure
    cannot break ingest.
    """
    counts: dict[str, int] = {}
    touched_slugs: set[str] = set()
    for ref in iter_refs(adapters):
        if ref.source_kind == "database":
            prior = conn.execute(
                "SELECT mtime, size, last_rowid FROM ingest_log "
                "WHERE file_path = ? AND session_id = ?",
                (str(ref.file_path), ref.session_id),
            ).fetchone()

            if prior and prior["mtime"] == ref.file_mtime and prior["size"] == ref.file_size:
                continue  # unchanged

            since = prior["last_rowid"] if prior else 0
        else:
            prior = conn.execute(
                "SELECT mtime, size, processed_offset FROM ingest_log "
                "WHERE file_path = ? AND session_id IS NULL",
                (str(ref.file_path),),
            ).fetchone()

            if prior and prior["mtime"] == ref.file_mtime and prior["size"] == ref.file_size:
                continue  # unchanged

            if prior and ref.file_size < prior["size"]:
                # Truncation / rotation — full reparse from 0
                conn.execute(
                    "DELETE FROM ingest_log WHERE file_path = ? AND session_id IS NULL",
                    (str(ref.file_path),),
                )
                since = 0
            else:
                since = prior["processed_offset"] if prior else 0

        adapter = _lookup(adapters, ref.provider)
        pre = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        ingest_file(conn, adapter, ref, since_offset=since)
        post = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        added = post - pre
        counts[ref.provider] = counts.get(ref.provider, 0) + added
        if added:
            touched_slugs.add(ref.project_slug)

    # Per-adapter post-ingest hook. Claude uses it to materialise agent-team
    # metadata from ~/.claude/teams + ~/.claude/tasks into the schema (so the
    # Agents tab JOINs instead of re-parsing raw_json on every render). Each
    # call is fenced — a hook hiccup must never break the ingest pass.
    for adapter in adapters:
        hook = getattr(adapter, "materialize_metadata", None)
        if hook is None:
            continue
        try:
            hook(conn)
        except Exception as e:  # noqa: BLE001 — a metadata hook must never break ingest
            _logger.warning("materialize_metadata failed for %s: %s", adapter.name, e)

    if touched_slugs:
        auto_reindex_touched(conn, touched_slugs)

    return counts


def auto_reindex_touched(
    conn: sqlite3.Connection,
    slugs: set[str] | list[str],
) -> None:
    """Refresh search/tag/Q&A indexes for the given project slugs.

    Each service is invoked independently — a failure in one (e.g. the
    beta tag/qa services) must not block the others. No-op when the
    ``auto_reindex_on_ingest`` setting is disabled or the corresponding
    services are not initialised on ``deps``.
    """
    import stackunderflow.deps as deps
    from stackunderflow.store import queries

    if not deps.config.get("auto_reindex_on_ingest"):
        return

    slug_list = list(slugs)
    if not slug_list:
        return

    project_rows = queries.list_projects(conn)
    by_slug: dict[str, list[int]] = {}
    for prow in project_rows:
        if prow.slug in slug_list:
            by_slug.setdefault(prow.slug, []).append(prow.id)

    prior_flag = getattr(deps, "is_reindexing", False)
    deps.is_reindexing = True
    try:
        for slug in slug_list:
            ids = by_slug.get(slug, [])
            if not ids:
                continue
            # The schema has UNIQUE(provider, slug) so the same slug can map
            # to multiple project rows (claude + codex). Concatenate before
            # indexing — index_project does a DELETE-by-slug first, so naive
            # iteration would let pass 2 wipe pass 1.
            messages: list[dict] = []
            for pid in ids:
                messages.extend(queries.get_project_messages(conn, project_id=pid))

            for svc, name, mode in (
                (getattr(deps, "search_service", None), "search", "with_project"),
                (getattr(deps, "qa_service", None), "qa", "with_project"),
                (getattr(deps, "tag_service", None), "tags", "messages_only"),
            ):
                if svc is None:
                    continue
                try:
                    if mode == "messages_only":
                        svc.index_project(messages)
                    else:
                        svc.index_project(slug, messages)
                    _logger.info(
                        "auto-reindex %s ok: project=%s messages=%d",
                        name, slug, len(messages),
                    )
                except Exception as e:
                    _logger.warning(
                        "auto-reindex %s failed for %s: %s", name, slug, e,
                    )
    finally:
        deps.is_reindexing = prior_flag


def _lookup(adapters: list[SourceAdapter], name: str) -> SourceAdapter:
    for a in adapters:
        if a.name == name:
            return a
    raise KeyError(f"No adapter registered for provider {name!r}")

"""Filesystem watcher — Wave 2C of the ETL pipeline.

Watches every registered adapter's source paths via ``watchfiles`` (the
Rust-backed library, sub-100ms latency cross-platform). On any change:

  1. Find which adapter(s) the changed path belongs to (via
     :func:`stackunderflow.adapters.registered` plus each adapter's
     :meth:`watch_paths` method).
  2. Run ``adapter.read(ref, since_offset=ingest_log.processed_offset)``
     to pull only the new bytes — the existing ``run_ingest`` helper
     already implements the watermark lookup (see
     ``stackunderflow/ingest/__init__.py``).
  3. Insert the new messages via the existing transactional writer
     (``stackunderflow/ingest/writer.py``).
  4. Run the matching ``Normalizer`` from the registry over those new
     messages and insert into ``usage_events``.
  5. Call ``refresh_all_marts(conn)`` so each mart's watermark advances
     by the newly-inserted event ids.

Steps (4) and (5) depend on Wave 2A and 2B respectively. Both are
imported **lazily** and gracefully no-op on ``ImportError`` so this
module is useful in isolation while those waves are in flight — once
they land, the steps activate automatically with no code change here.

Debounced 200ms (default): a burst of file writes within the window
collapses into one refresh cycle, coalescing the JSONL-append spam an
active session generates.

Runs in a daemon thread. Never blocks the FastAPI request path; never
crashes on a bad event (every cycle is wrapped in a broad ``except`` so
one poison file cannot stop the whole loop).
"""

from __future__ import annotations

import logging
import sqlite3
import threading
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from stackunderflow.adapters import registered as _registered
from stackunderflow.adapters.base import SourceAdapter

_log = logging.getLogger(__name__)

# Public default: the spec calls out 200ms as "coalesces JSONL append
# bursts from active sessions" — short enough to feel live, long enough
# to absorb a session that flushes 5+ lines back-to-back.
DEFAULT_DEBOUNCE_MS = 200
# watchfiles polls every 50ms by default on macOS FSEvents; we override
# below to sit just under that so the worst-case detection latency is
# bounded.
DEFAULT_POLL_INTERVAL_MS = 50


@dataclass
class WatcherHandle:
    """Returned by :func:`start_watcher`. Call ``.stop()`` to halt cleanly."""

    thread: threading.Thread
    stop_event: threading.Event

    def stop(self, timeout: float = 5.0) -> None:
        """Signal the watcher loop to exit and join the thread.

        ``watchfiles`` honours an injected ``stop_event``, so the loop
        wakes within one ``rust_timeout`` cycle (5s by default in the
        library) when we set it. We pass a generous default ``timeout``
        on the join so a slow shutdown cycle is not silently swallowed.
        """
        self.stop_event.set()
        if self.thread.is_alive():
            self.thread.join(timeout=timeout)


def watch_paths_for(adapter: SourceAdapter) -> list[Path]:
    """Return the list of root paths to watch for *adapter*.

    Adapters opt in by defining their own ``watch_paths()`` method. If
    they don't, we treat them as "periodic-ingest only" and return an
    empty list — the watcher silently ignores them. This keeps the
    Protocol surface in ``adapters/base.py`` unchanged for the dozen
    beta adapters that haven't been validated for live-watching yet.
    """
    fn = getattr(adapter, "watch_paths", None)
    if fn is None:
        return []
    try:
        paths = list(fn())
    except Exception as exc:  # noqa: BLE001 — never let one adapter poison the registry
        _log.warning("watch_paths() failed for adapter %s: %s", adapter.name, exc)
        return []
    out: list[Path] = []
    for p in paths:
        if not isinstance(p, Path):
            p = Path(p)
        # Filter to paths that actually exist — the watcher otherwise
        # warns and exits when handed a missing root, taking the whole
        # daemon thread down with it. Adapters return canonical roots
        # uniformly; on a fresh machine most of them are absent.
        try:
            if p.exists():
                out.append(p)
        except OSError:
            continue
    return out


def _adapter_for_path(
    changed_path: str,
    adapter_paths: list[tuple[SourceAdapter, list[Path]]],
) -> SourceAdapter | None:
    """Return the adapter whose ``watch_paths`` covers *changed_path*.

    Match by string-prefix on the resolved path so symlink-equivalent
    paths still match. Returns the first hit; adapter roots don't
    overlap in the default-on registry (claude / codex / cursor / cline
    each own a distinct directory).
    """
    try:
        target = Path(changed_path).resolve()
    except OSError:
        return None
    target_str = str(target)
    for adapter, paths in adapter_paths:
        for root in paths:
            try:
                root_str = str(root.resolve())
            except OSError:
                continue
            # Equal or beneath; for a vscdb file the root *is* the file.
            if target_str == root_str or target_str.startswith(root_str + "/"):
                return adapter
    return None


def _run_cycle(
    conn_factory: Callable[[], sqlite3.Connection],
    touched: list[tuple[SourceAdapter, set[str]]],
) -> None:
    """One refresh cycle: ingest → normalize → marts.

    *touched* is a list of ``(adapter, changed_paths)`` pairs. The
    watcher narrows the ingest sweep to **only** the files the watcher
    saw change in this cycle — re-running the full ``run_ingest`` sweep
    would walk every project under every adapter root every time, which
    on a real machine with ~150 projects easily exceeds the 400ms
    end-to-end latency target the spec calls for.

    We re-use the existing per-file ``ingest_file`` writer (which
    handles the SessionRef → ingest_log → messages-table transaction
    atomically) but enumerate only the changed paths.

    Every step is wrapped in its own broad ``except`` because the
    watcher is on the read-side critical path: a single poisoned
    record must not be allowed to stop the whole pipeline.
    """
    if not touched:
        return
    start = time.perf_counter()

    conn = conn_factory()
    try:
        # Step 1+2+3: enumerate just the touched adapter, filter to the
        # SessionRefs whose file_path matches a changed path, and run
        # the existing ingest_file writer per matched ref.
        counts: dict[str, int] = {}
        try:
            counts = _ingest_changed_paths(conn, touched)
        except Exception as exc:  # noqa: BLE001 — keep the loop alive
            _log.warning("etl.watcher: ingest failed: %s", exc)
        events = sum(counts.values()) if counts else 0
        touched_adapters = [a for a, _ in touched]

        # Step 4: per-provider normalizer (Wave 2A — lazy import).
        # ``etl.normalize`` may not exist yet; treat ImportError as
        # "feature not landed yet" and continue with marts-attempt.
        events_normalised = 0
        try:
            from stackunderflow.etl.normalize import (
                get as _get_normalizer,  # type: ignore[import-not-found]
            )
        except ImportError:
            _get_normalizer = None  # type: ignore[assignment]

        if _get_normalizer is not None and counts:
            for adapter in touched_adapters:
                if not counts.get(adapter.name):
                    continue
                try:
                    normalizer = _get_normalizer(adapter.name)
                except Exception as exc:  # noqa: BLE001
                    _log.debug(
                        "etl.watcher: no normalizer for %s: %s",
                        adapter.name, exc,
                    )
                    continue
                try:
                    events_normalised += _normalize_recent(conn, adapter.name, normalizer)
                except Exception as exc:  # noqa: BLE001
                    _log.warning(
                        "etl.watcher: normalize failed for %s: %s",
                        adapter.name, exc,
                    )

        # Step 5: mart refresh (Wave 2B — lazy import).
        mart_counts: dict[str, int] = {}
        try:
            from stackunderflow.etl.watermark import (
                refresh_all_marts as _refresh_marts,  # type: ignore[import-not-found]
            )
        except ImportError:
            _refresh_marts = None  # type: ignore[assignment]

        if _refresh_marts is not None:
            try:
                mart_counts = _refresh_marts(conn) or {}
            except Exception as exc:  # noqa: BLE001
                _log.warning("etl.watcher: refresh_all_marts failed: %s", exc)

    finally:
        try:
            conn.close()
        except Exception as exc:  # noqa: BLE001 — swallow on shutdown
            _log.debug("etl.watcher: conn.close() raised: %s", exc)

    # Step 6: embed newly-indexed messages for hybrid (FTS + vector)
    # retrieval. Best-effort and fully decoupled — mirrors how the FTS
    # triggers keep the search index current. Gated on a local Ollama;
    # a single cheap reachability probe short-circuits when it is absent
    # (CI, most machines), so this step never blocks the cycle or raises.
    embedded = _embed_new_messages_best_effort()

    elapsed_ms = (time.perf_counter() - start) * 1000.0
    mart_summary = " ".join(
        f"{name}={n}" for name, n in sorted(mart_counts.items())
    )
    _log.info(
        "etl.watcher: refreshed marts in %dms — %d events%s%s%s",
        round(elapsed_ms),
        events,
        f", normalized={events_normalised}" if events_normalised else "",
        f" {mart_summary}" if mart_summary else "",
        f" embedded={embedded}" if embedded else "",
    )


def _embed_new_messages_best_effort() -> int:
    """Embed newly-indexed ``search_index.db`` messages; never raise.

    Opens its own ``SearchService`` connection (the vector store keys on
    the *search index's* message ids, not the store's) and hands it to
    :func:`stackunderflow.services.embeddings.embed_new_messages`, which
    is itself gated on Ollama reachability and swallows every error.

    Returns the number of vectors written (``0`` when Ollama is absent,
    the search index is empty, or anything at all goes wrong). Wrapped in
    a broad ``except`` so the watcher's critical path is untouched by a
    missing module, a locked db, or a slow Ollama.
    """
    try:
        from stackunderflow.services import embeddings as _emb
        from stackunderflow.services.search_service import SearchService

        # Cheap pre-check: no local Ollama → don't even open the index.
        if not _emb.ollama_reachable():
            return 0
        svc = SearchService()
        conn = svc._get_conn()
        try:
            return _emb.embed_new_messages(conn)
        finally:
            conn.close()
    except Exception as exc:  # noqa: BLE001 — never let embedding stall the loop
        _log.debug("etl.watcher: embed_new_messages step failed: %s", exc)
        return 0


def _ingest_changed_paths(
    conn: sqlite3.Connection,
    touched: list[tuple[SourceAdapter, set[str]]],
) -> dict[str, int]:
    """Re-run the per-file writer for the watcher's changed paths only.

    For each touched adapter we enumerate its SessionRefs once (cheap —
    enumerate is a directory listing or vscdb metadata query, not a
    full read), filter to the refs whose ``file_path`` is in the
    changed-paths set, and call ``ingest_file`` per match. This keeps
    the cycle's wall time bounded by the size of the changed file, not
    by the size of the full adapter root.

    Returns ``{provider: messages_added}`` so the cycle log can report
    real numbers.
    """
    from stackunderflow.ingest.writer import ingest_file

    counts: dict[str, int] = {}
    for adapter, changed_path_strs in touched:
        # Resolve every changed path once so prefix-matching against a
        # SessionRef's file_path is symlink-stable.
        changed_resolved: set[str] = set()
        for p in changed_path_strs:
            try:
                changed_resolved.add(str(Path(p).resolve()))
            except OSError:
                changed_resolved.add(p)

        try:
            refs = list(adapter.enumerate())
        except Exception as exc:  # noqa: BLE001
            _log.warning(
                "etl.watcher: enumerate failed for %s: %s", adapter.name, exc,
            )
            continue

        for ref in refs:
            try:
                ref_path_str = str(ref.file_path.resolve())
            except OSError:
                ref_path_str = str(ref.file_path)
            if ref_path_str not in changed_resolved:
                continue
            # Look up the existing watermark exactly the way run_ingest
            # does so we read only the new bytes / new rowids.
            since = _watermark_for(conn, ref)
            pre = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            try:
                ingest_file(conn, adapter, ref, since_offset=since)
            except Exception as exc:  # noqa: BLE001
                _log.warning(
                    "etl.watcher: ingest_file failed for %s (%s): %s",
                    ref.file_path, adapter.name, exc,
                )
                continue
            post = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            counts[adapter.name] = counts.get(adapter.name, 0) + (post - pre)
    return counts


def _watermark_for(conn: sqlite3.Connection, ref: Any) -> int:
    """Fetch the resume-offset for *ref* from ingest_log.

    Mirrors the dispatch in ``stackunderflow/ingest/__init__.py``: file
    refs key on ``(file_path, session_id IS NULL)``; database refs key
    on ``(file_path, session_id)``. Missing rows mean "fresh ingest"
    and resolve to 0.
    """
    if ref.source_kind == "database":
        row = conn.execute(
            "SELECT last_rowid FROM ingest_log "
            "WHERE file_path = ? AND session_id = ?",
            (str(ref.file_path), ref.session_id),
        ).fetchone()
        return int(row["last_rowid"]) if row else 0
    row = conn.execute(
        "SELECT processed_offset FROM ingest_log "
        "WHERE file_path = ? AND session_id IS NULL",
        (str(ref.file_path),),
    ).fetchone()
    return int(row["processed_offset"]) if row else 0


def _normalize_recent(
    conn: sqlite3.Connection, provider: str, normalizer: Any,
) -> int:
    """Run *normalizer* over the messages-rows the most recent ingest
    just inserted.

    Handed a Wave 2A ``Normalizer`` (any object exposing
    ``normalize(msg_row) -> Iterable[dict]``). Looks up messages whose
    provider matches and that don't yet have a corresponding row in
    ``usage_events`` (uniqueness via ``uniq_events_msg`` per spec). When
    ``usage_events`` does not yet exist (Wave 1 not merged) the function
    silently no-ops so the watcher keeps running.

    Returns the number of events inserted.
    """
    # Probe the schema — if Wave 1 hasn't landed yet, gracefully bail.
    has_events = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_events'"
    ).fetchone()
    if not has_events:
        return 0

    rows = conn.execute(
        """
        SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.model,
               m.input_tokens, m.output_tokens, m.cache_create_tokens,
               m.cache_read_tokens, m.content_text, m.tools_json,
               m.raw_json, m.is_sidechain, m.uuid, m.parent_uuid, m.speed,
               s.session_id AS session_id, s.project_id AS project_id,
               p.provider AS provider
          FROM messages m
          JOIN sessions s ON s.id = m.session_fk
          JOIN projects p ON p.id = s.project_id
     LEFT JOIN usage_events e ON e.source_message_fk = m.id
         WHERE p.provider = ?
           AND e.id IS NULL
        """,
        (provider,),
    ).fetchall()

    inserted = 0
    for row in rows:
        msg_row = dict(row)
        try:
            events = list(normalizer.normalize(msg_row))
        except Exception as exc:  # noqa: BLE001
            _log.debug(
                "etl.watcher: normalizer raised for msg %s: %s",
                msg_row.get("id"), exc,
            )
            continue
        for ev in events:
            try:
                conn.execute(
                    """
                    INSERT OR IGNORE INTO usage_events (
                        source_message_fk, provider, account, project_id,
                        session_id, ts, day, model, speed,
                        input_tokens, output_tokens,
                        cache_read_tokens, cache_create_tokens,
                        cost_usd, cost_source, role, raw_extras
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        msg_row["id"],
                        ev.get("provider", provider),
                        ev.get("account", "default"),
                        ev.get("project_id", msg_row["project_id"]),
                        ev.get("session_id", msg_row["session_id"]),
                        ev.get("ts", msg_row["timestamp"]),
                        ev.get("day", _day_of(msg_row.get("timestamp", ""))),
                        ev.get("model", msg_row.get("model") or ""),
                        ev.get("speed", msg_row.get("speed", "standard")),
                        int(ev.get("input_tokens", 0)),
                        int(ev.get("output_tokens", 0)),
                        int(ev.get("cache_read_tokens", 0)),
                        int(ev.get("cache_create_tokens", 0)),
                        float(ev.get("cost_usd", 0.0)),
                        ev.get("cost_source", "rate_card"),
                        ev.get("role", msg_row.get("role", "")),
                        ev.get("raw_extras"),
                    ),
                )
                inserted += 1
            except sqlite3.Error as exc:
                _log.debug(
                    "etl.watcher: insert into usage_events failed for msg %s: %s",
                    msg_row.get("id"), exc,
                )
    return inserted


def _day_of(ts: str) -> str:
    """Best-effort YYYY-MM-DD slice from an ISO 8601 timestamp."""
    if not ts:
        return ""
    return ts[:10] if len(ts) >= 10 else ""


def start_watcher(
    conn_factory: Callable[[], sqlite3.Connection],
    *,
    debounce_ms: int = DEFAULT_DEBOUNCE_MS,
    poll_interval_ms: int = DEFAULT_POLL_INTERVAL_MS,
    adapters: Iterable[SourceAdapter] | None = None,
) -> WatcherHandle:
    """Spawn a daemon thread that watches every registered adapter's roots.

    Parameters
    ----------
    conn_factory:
        Callable returning a fresh SQLite connection for each cycle.
        The watcher does not share a connection across cycles so a
        crash mid-write doesn't poison the next refresh.
    debounce_ms:
        Window over which a burst of changes collapses into one cycle.
        Default 200ms per spec — see ``docs/specs/etl-architecture.md``
        §"Watcher latency target".
    poll_interval_ms:
        Inner watchfiles step. 50ms keeps worst-case detection latency
        bounded on macOS FSEvents (which is event-driven anyway, so
        this is the timeout granularity for ``stop_event`` checks).
    adapters:
        Override the registry. Tests pass a single fake adapter pointed
        at a temp directory; production callers leave this ``None`` so
        every default-on adapter participates.

    Returns
    -------
    WatcherHandle
        Use ``handle.stop()`` for clean shutdown.
    """
    adapter_list = list(adapters) if adapters is not None else list(_registered())
    adapter_paths: list[tuple[SourceAdapter, list[Path]]] = []
    all_paths: list[Path] = []
    for a in adapter_list:
        paths = watch_paths_for(a)
        if not paths:
            continue
        adapter_paths.append((a, paths))
        all_paths.extend(paths)

    stop_event = threading.Event()

    if not all_paths:
        # Nothing to watch — return an inert handle so callers don't
        # have to special-case "no adapters had any roots". Set the
        # event up front so .stop() is a no-op join on a thread that
        # finished immediately.
        _log.info("etl.watcher: no adapter roots to watch; staying idle")
        thread = threading.Thread(
            target=lambda: None,
            name="stackunderflow-watcher-idle",
            daemon=True,
        )
        thread.start()
        return WatcherHandle(thread=thread, stop_event=stop_event)

    def _loop() -> None:
        # Lazy import — keep ``watchfiles`` out of the import path of
        # callers that never call start_watcher() (e.g. CLI subcommands).
        try:
            from watchfiles import watch
        except ImportError as exc:
            _log.warning("etl.watcher: watchfiles unavailable, watcher disabled: %s", exc)
            return

        _log.info(
            "etl.watcher: watching %d path(s) across %d adapter(s); "
            "debounce=%dms poll=%dms",
            len(all_paths), len(adapter_paths), debounce_ms, poll_interval_ms,
        )

        try:
            for changes in watch(
                *all_paths,
                debounce=debounce_ms,
                step=poll_interval_ms,
                stop_event=stop_event,
                rust_timeout=1000,        # wake every second to check stop_event
                yield_on_timeout=False,
                raise_interrupt=False,
            ):
                if stop_event.is_set():
                    return
                if not changes:
                    continue
                # Bucket changed paths by the adapter that owns them —
                # one bucket per provider so the cycle can scope each
                # ingest to "just the files this adapter saw change".
                # A 5-line burst on one JSONL still produces one bucket
                # with one path.
                buckets: dict[str, tuple[SourceAdapter, set[str]]] = {}
                for _change_type, path in changes:
                    adapter = _adapter_for_path(path, adapter_paths)
                    if adapter is None:
                        continue
                    bucket = buckets.setdefault(adapter.name, (adapter, set()))
                    bucket[1].add(path)
                if not buckets:
                    continue
                touched = [(adapter, paths) for adapter, paths in buckets.values()]
                try:
                    _run_cycle(conn_factory, touched)
                except Exception as exc:  # noqa: BLE001 — never crash the daemon
                    _log.warning("etl.watcher: cycle failed: %s", exc)
        except Exception as exc:  # noqa: BLE001 — never crash the daemon
            _log.error("etl.watcher: loop terminated unexpectedly: %s", exc)

    thread = threading.Thread(
        target=_loop,
        name="stackunderflow-watcher",
        daemon=True,
    )
    thread.start()
    return WatcherHandle(thread=thread, stop_event=stop_event)

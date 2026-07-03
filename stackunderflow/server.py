#!/usr/bin/env python3
"""
FastAPI application for StackUnderflow Local Mode
"""

import importlib.metadata
import logging
import os
from contextlib import asynccontextmanager

from dotenv import load_dotenv
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

import stackunderflow.deps as deps

# Route modules
from stackunderflow.routes import (
    agent_teams,
    benchmark,
    bookmarks,
    budgets,
    cfg,
    commands,
    compare,
    context_budget,
    context_replay,
    cost,
    data,
    etl,
    forks,
    live,
    meta_agent,
    misc,
    optimize,
    patterns,
    plan,
    playback,
    pricing,
    projects,
    qa,
    quality,
    search,
    sessions,
    tags,
    webhooks,
    whatif,
    worktrees,
    yield_route,
)
from stackunderflow.routes import (
    export as export_routes,
)
from stackunderflow.routes import (
    static_analysis as static_analysis_routes,
)
from stackunderflow.routes import (
    sync as sync_routes,
)
from stackunderflow.services.bookmark_service import BookmarkService
from stackunderflow.services.pricing_service import PricingService
from stackunderflow.services.qa_service import QAService
from stackunderflow.services.search_service import SearchService
from stackunderflow.services.tag_service import TagService

# Load environment variables
load_dotenv()

# Configure logging
log_level = os.getenv("LOG_LEVEL", "INFO").upper()
logging.basicConfig(
    level=getattr(logging, log_level, logging.INFO), format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
logger = logging.getLogger(__name__)
logger.info(f"Logging configured with level: {log_level}")

# Get version from package metadata
try:
    __version__ = importlib.metadata.version("stackunderflow")
except importlib.metadata.PackageNotFoundError:
    # Fallback for development mode
    from stackunderflow.__version__ import __version__

# Configuration (needed by lifespan)
config = deps.config
BASE_DIR = deps.BASE_DIR


@asynccontextmanager
async def _lifespan(_app: FastAPI):
    """Initialize services and start background tasks."""
    _svc_inits: list[tuple[str, type, dict]] = [
        ("search_service", SearchService, {}),
        ("tag_service", TagService, {}),
        ("qa_service", QAService, {}),
        ("bookmark_service", BookmarkService, {}),
        ("pricing_service", PricingService, {}),
    ]
    for name, cls, kw in _svc_inits:
        try:
            setattr(deps, name, cls(**kw))
        except Exception as e:
            logger.error(f"Failed to initialize {name}: {e}")

    active = [n for n, _, _ in _svc_inits if getattr(deps, n, None) is not None]
    failed = [n for n, _, _ in _svc_inits if getattr(deps, n, None) is None]
    logger.info(f"Services initialized: {len(active)} active, {len(failed)} failed")
    if failed:
        logger.warning(f"Failed services: {', '.join(failed)}")

    # Initialise the session store schema synchronously (cheap), then
    # run the ingest in a background thread so HTTP starts serving
    # immediately. Without this, the lifespan blocks the bind for the
    # full duration of the reindex (~90s on 7 small projects, 30+min
    # on a cold 188-project store) — and the "live at..." line that
    # already printed from the CLI wrapper is misleading because the
    # HTTP server hasn't actually started yet.
    import threading

    from stackunderflow.adapters import registered
    from stackunderflow.ingest import run_ingest
    from stackunderflow.store import db, schema

    try:
        _schema_conn = db.connect(deps.store_path)
        schema.apply(_schema_conn)
        _schema_conn.close()
    except Exception as e:
        logger.error("Schema apply failed at startup: %s", e)

    def _background_ingest() -> None:
        try:
            conn = db.connect(deps.store_path)
            # Activate the unified price book as the LIVE source for a running
            # server: backfill it from the in-code manifest + RATE_CARD
            # (idempotent UPSERT, gate-proven rate-equal to in-code), wire the
            # seam to this store, and prime the in-memory cache. A clean miss
            # still falls back to the in-code manifest, so this only ever
            # moves WHERE a rate lives, never WHAT it is. Runs here — not on
            # the lifespan thread — because ANY synchronous store touch before
            # ``yield`` delays the port bind; on a store with a pathological
            # WAL (observed: 1.5 GB after an interrupted bulk write) that
            # "cheap" touch kept the port dead for minutes. Best-effort: any
            # failure leaves the seam in its safe default (off ⇒ in-code
            # pricing).
            try:
                from stackunderflow.infra import model_manifest as _mm
                from stackunderflow.infra.costs import backfill_price_book

                backfill_price_book(conn)
                conn.commit()
                _mm.use_price_book_store(deps.store_path, enabled=True)
                _mm.prime_price_book_cache(conn)
                logger.info("Price book activated (cache primed from backfilled store)")
            except Exception as exc:  # noqa: BLE001 — never block ingest on pricing
                logger.warning("Price book activation skipped: %s", exc)
            counts = run_ingest(conn, registered())
            logger.info("Ingest complete: %s", counts)
            conn.close()
            _maybe_clean_cold_cache()
        except Exception as exc:  # noqa: BLE001 — top of background thread
            logger.error("Background ingest failed: %s", exc)

    threading.Thread(
        target=_background_ingest,
        name="stackunderflow-ingest",
        daemon=True,
    ).start()

    # Wave 2C ETL filesystem watcher — keeps the marts current as
    # JSONL/vscdb files change. Default-on; ``stackunderflow start
    # --no-watcher`` (or env ``STACKUNDERFLOW_DISABLE_WATCHER=1``) skips
    # the spawn for headless / debugging modes. Daemon thread, so it
    # dies with the process — explicit shutdown via the handle is
    # available if FastAPI ever surfaces a teardown hook.
    if not _watcher_disabled():
        # Single-watcher invariant: only one process at a time runs the
        # filesystem watcher. The lock at ``~/.stackunderflow/server.lock``
        # is acquired non-blockingly. If another live instance already
        # holds it, we log a clear warning and continue serving HTTP
        # without spawning a second watcher — the dashboard reads from
        # the store and is happy without a local watcher.
        # ``--no-lock`` (or ``STACKUNDERFLOW_DISABLE_LOCK=1``) skips the
        # fence for tests / headless scenarios.
        lock_handle = None
        if not _lock_disabled():
            from stackunderflow.etl.lock import (
                acquire_watcher_lock,
                read_lock_holder,
            )

            lock_handle = acquire_watcher_lock()
            if lock_handle is None:
                holder = read_lock_holder()
                logger.warning(
                    "Watcher lock held by PID %s; this instance will serve "
                    "HTTP but will not run the watcher",
                    holder if holder is not None else "<unknown>",
                )
            else:
                deps.watcher_lock_handle = lock_handle

        if lock_handle is not None or _lock_disabled():
            try:
                from stackunderflow.etl.watcher import start_watcher

                def _watcher_conn() -> "object":
                    # Each cycle gets its own short-lived connection so a
                    # crash mid-write can't poison the next refresh.
                    return db.connect(deps.store_path)

                handle = start_watcher(_watcher_conn)
                deps.watcher_handle = handle
                logger.info("ETL watcher started (Wave 2C)")
            except Exception as exc:  # noqa: BLE001 — never block server start on watcher
                logger.warning("ETL watcher failed to start: %s", exc)

    try:
        yield
    finally:
        # Lifespan shutdown — release the watcher lock so a follow-up
        # ``stackunderflow start`` can pick it up cleanly. The OS-level
        # flock would also drop on process exit, but explicit release
        # keeps the metadata file content honest in test scenarios that
        # bring the lifespan up and down inside one process.
        try:
            from stackunderflow.etl.lock import release_watcher_lock

            release_watcher_lock(deps.watcher_lock_handle)
            deps.watcher_lock_handle = None
        except Exception as exc:  # noqa: BLE001 — never raise from lifespan shutdown
            logger.debug("etl.lock: release on shutdown raised: %s", exc)


# Create FastAPI app
app = FastAPI(
    title="StackUnderflow - Local Mode",
    description="Analyze your Claude AI logs directly from your local machine",
    version=__version__,
    lifespan=_lifespan,
)

# Add CORS middleware — allow configured port and common dev-server ports
_server_port = config.get("port")
app.add_middleware(
    CORSMiddleware,
    allow_origins=[
        f"http://localhost:{_server_port}",
        f"http://127.0.0.1:{_server_port}",
        "http://localhost:5175",  # vite dev server
        "http://127.0.0.1:5175",
    ],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Add GZip compression middleware
# Disabled: GZip was actually increasing load time for large payloads
# app.add_middleware(GZipMiddleware, minimum_size=1000)

# Mount static files
app.mount("/static", StaticFiles(directory=os.path.join(BASE_DIR, "static")), name="static")

# Include all route modules
app.include_router(projects.router)
app.include_router(data.router)
app.include_router(cost.router)
app.include_router(commands.router)
app.include_router(sessions.router)
app.include_router(search.router)
app.include_router(qa.router)
app.include_router(tags.router)
app.include_router(bookmarks.router)
app.include_router(misc.router)
app.include_router(export_routes.router)
app.include_router(optimize.router)
app.include_router(plan.router)
app.include_router(compare.router)
app.include_router(yield_route.router)
app.include_router(context_budget.router)
app.include_router(context_replay.router)
app.include_router(cfg.router)
app.include_router(etl.router)
app.include_router(agent_teams.router)
app.include_router(playback.router)
app.include_router(meta_agent.router)
app.include_router(live.router)
app.include_router(webhooks.router)
app.include_router(static_analysis_routes.router)
app.include_router(quality.router)
app.include_router(pricing.router)
app.include_router(budgets.router)
app.include_router(whatif.router)
app.include_router(forks.router)
app.include_router(benchmark.router)
app.include_router(patterns.router)
app.include_router(worktrees.router)
app.include_router(sync_routes.router)  # #100 Phase 2 — opt-in multi-device sync read surface



@app.get("/")
async def root():
    """Serve the React app."""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


# SPA catch-all -- serve React index.html for client-side routing
@app.get("/project/{full_path:path}")
async def spa_catch_all_project(full_path: str):
    """Serve React SPA for client-side routes under /project/"""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


@app.get("/settings")
async def spa_settings():
    """Serve React SPA for /settings client-side route."""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


@app.get("/live")
async def spa_live():
    """Serve React SPA for /live client-side route."""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


from stackunderflow.routes.data import refresh_all_projects, refresh_data  # noqa: E402, F401


def _watcher_disabled() -> bool:
    """Return True when the env opts out of the Wave 2C watcher.

    The CLI's ``stackunderflow start --no-watcher`` sets
    ``STACKUNDERFLOW_DISABLE_WATCHER=1`` before invoking uvicorn, so the
    flag survives the spawn into the FastAPI lifespan. Useful for
    headless / profiling runs that want a deterministic ingest pass
    without the live-watcher wakeups.
    """
    val = os.environ.get("STACKUNDERFLOW_DISABLE_WATCHER", "").strip().lower()
    return val in ("1", "true", "yes", "on")


def _lock_disabled() -> bool:
    """Return True when the env opts out of the watcher lock fence.

    The CLI's ``stackunderflow start --no-lock`` sets
    ``STACKUNDERFLOW_DISABLE_LOCK=1`` so two instances against the same
    store can both run watchers — useful for the tests that exercise
    parallel watchers and for headless setups where the user has manual
    control of the singleton invariant.
    """
    val = os.environ.get("STACKUNDERFLOW_DISABLE_LOCK", "").strip().lower()
    return val in ("1", "true", "yes", "on")


def _maybe_clean_cold_cache() -> None:
    """Remove the old JSON cache once the store is populated."""
    import shutil
    from pathlib import Path

    cold = Path.home() / ".stackunderflow" / "cache"
    if cold.exists():
        shutil.rmtree(cold, ignore_errors=True)


def start_server_with_args(port=8081, host="localhost"):
    """Start the server with specified arguments"""
    import uvicorn

    uvicorn.run(app, host=host, port=port, log_level="warning", access_log=False)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=8081)

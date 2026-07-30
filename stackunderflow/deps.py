"""Shared application state accessed by route modules.

This module holds the singleton cache, config, services, and mutable
project state.  Route modules import what they need from here instead
of reaching into ``server`` globals.
"""

from __future__ import annotations

import logging
import os
from typing import TYPE_CHECKING

from stackunderflow.settings import Settings, app_dir

if TYPE_CHECKING:
    from stackunderflow.etl.lock import LockHandle
    from stackunderflow.etl.watcher import WatcherHandle
    from stackunderflow.services.bookmark_service import BookmarkService
    from stackunderflow.services.pricing_service import PricingService
    from stackunderflow.services.qa_service import QAService
    from stackunderflow.services.search_service import SearchService
    from stackunderflow.services.tag_service import TagService

logger = logging.getLogger("stackunderflow")

# ── configuration & cache ────────────────────────────────────────────────────

config = Settings()

BASE_DIR = os.path.dirname(os.path.abspath(__file__))

# Path to the unified session store (created on first use). Derived from
# ``settings.app_dir()`` so ``$STACKUNDERFLOW_HOME`` / ``--data-dir`` re-points
# it; tests monkeypatch this attribute directly.
store_path = app_dir() / "store.db"

# ── mutable project state ────────────────────────────────────────────────────

current_project_path: str | None = None
current_log_path: str | None = None
is_reindexing: bool = False

# ── services (populated at startup by server.py) ────────────────────────────

search_service: SearchService | None = None
tag_service: TagService | None = None
qa_service: QAService | None = None
bookmark_service: BookmarkService | None = None
pricing_service: PricingService | None = None

# Wave 2C: filesystem watcher handle, populated by ``server._lifespan``
# unless ``STACKUNDERFLOW_DISABLE_WATCHER=1`` (set by ``start
# --no-watcher``). Stays ``None`` for CLI subcommands that don't bring
# up the FastAPI app.
watcher_handle: WatcherHandle | None = None

# Single-watcher invariant lock handle (Wave 5 follow-up). Populated by
# ``server._lifespan`` when the current process owns the watcher lock at
# ``~/.stackunderflow/server.lock``. ``None`` means either (a) we're
# running with ``--no-lock`` / no FastAPI lifespan, or (b) another live
# instance already holds the lock and this process is HTTP-only.
watcher_lock_handle: LockHandle | None = None

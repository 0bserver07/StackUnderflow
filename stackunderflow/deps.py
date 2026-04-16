"""Shared application state accessed by route modules.

This module holds the singleton cache, config, services, and mutable
project state.  Route modules import what they need from here instead
of reaching into ``server`` globals.
"""

from __future__ import annotations

import logging
import os
from typing import TYPE_CHECKING

from stackunderflow.infra.cache import TieredCache
from stackunderflow.settings import Settings

if TYPE_CHECKING:
    from stackunderflow.services.bookmark_service import BookmarkService
    from stackunderflow.services.pricing_service import PricingService
    from stackunderflow.services.qa_service import QAService
    from stackunderflow.services.search_service import SearchService
    from stackunderflow.services.tag_service import TagService

logger = logging.getLogger("stackunderflow")

# ── configuration & cache ────────────────────────────────────────────────────

config = Settings()

cache = TieredCache(
    max_slots=config.get("cache_max_projects"),
    max_mb=config.get("cache_max_mb_per_project"),
)

BASE_DIR = os.path.dirname(os.path.abspath(__file__))

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

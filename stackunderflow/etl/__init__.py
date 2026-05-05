"""ETL layer — three-stage pipeline that turns raw ``messages`` rows into
analyst-friendly marts.

Layout::

    etl/
      normalize/   per-provider Normalizer ABC + plug-in registry
      marts/       per-mart MartBuilder ABC + plug-in registry
      watermark.py read/write/refresh helpers keyed on `mart_watermark`
      backfill.py  orchestrator: run every normalizer, refresh every mart

Wave 1 ships only the foundation — the registries are empty until Wave 2
registers the provider normalizers and mart builders. Calling
``backfill()`` on a Wave 1 install is a no-op (returns zero counts). See
``docs/specs/etl-architecture.md``.
"""

from __future__ import annotations

from .backfill import BackfillReport, backfill
from .watermark import get_watermark, refresh_all_marts, set_watermark

__all__ = [
    "BackfillReport",
    "backfill",
    "get_watermark",
    "set_watermark",
    "refresh_all_marts",
]

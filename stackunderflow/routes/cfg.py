"""Settings / configuration routes for the React UI.

Exposes a minimal HTTP surface so the dashboard can read and write the
same persistent settings that the ``stackunderflow cfg`` CLI manipulates.

Endpoints:
  - ``GET  /api/cfg``                       — full settings snapshot
  - ``GET  /api/cfg/currencies``            — list of suggested + supported codes
  - ``POST /api/cfg/currency``              — change the active currency
  - ``GET    /api/cfg/model-aliases``       — current alias map
  - ``POST   /api/cfg/model-aliases``       — add/update one alias
  - ``DELETE /api/cfg/model-aliases``       — remove one alias (?from=…)

The ``DELETE`` route takes the source id as a query string rather than a
path segment because alias keys often contain slashes (``openrouter/...``)
which would need double-encoding to round-trip a path component.

All writes go through ``Settings().persist`` so validators run and the
config file on disk stays the single source of truth. Writes that change
how data is *aggregated* (model aliases) invalidate BOTH aggregation caches
— the dashboard payload memo and the project-stats memo behind
``/api/cost-data`` / ``/api/stats`` / ``/api/tool-distribution`` — so the
next fetch on either surface reflects the new grouping. A currency change
does **not** flush either: cached payloads are stored in USD and the active
currency is re-applied per response, so switching currency is a cheap
rescale rather than a full re-aggregation (#31). It does drop the currency
memo, which caches the resolved code itself.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import JSONResponse

from stackunderflow.infra.currency import (
    active_currency_payload,
    clear_currency_memo,
    list_supported,
)
from stackunderflow.routes.cost import _invalidate_stats_cache
from stackunderflow.routes.data import invalidate_dashboard_cache
from stackunderflow.settings import Settings

router = APIRouter()


# Suggested ISO 4217 codes the UI can render in the dropdown without a
# network round-trip. The user can still type any 3-letter code in the
# "Other" input — the validator on Settings.currency enforces shape.
_COMMON_CURRENCIES: list[str] = [
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "CNY", "INR",
    "KRW", "MXN", "BRL", "SEK", "NOK", "DKK", "PLN", "RUB", "TRY",
    "ZAR", "AED", "SAR", "SGD", "HKD", "NZD",
]


@router.get("/api/cfg")
async def get_cfg() -> JSONResponse:
    """Return all user-facing settings + the active currency payload."""
    s = Settings()
    return JSONResponse({
        "settings": s.get_all(),
        "currency": active_currency_payload(),
    })


@router.get("/api/cfg/currencies")
async def get_currencies() -> JSONResponse:
    """Return the suggested + locally cached currency codes.

    ``common`` are the shortlist always shown in the UI dropdown.
    ``supported`` are anything Frankfurter has published (cached); these
    populate the autocomplete for "Other".
    """
    return JSONResponse({
        "common": _COMMON_CURRENCIES,
        "supported": list_supported(),
        "current": active_currency_payload(),
    })


@router.post("/api/cfg/currency")
async def set_currency(data: dict) -> JSONResponse:
    """Set the active currency (3-letter ISO code).

    Accepts either ``{"code": "EUR"}`` (preferred) or ``{"currency": "EUR"}``.
    """
    code = data.get("code") or data.get("currency")
    if not isinstance(code, str) or not code.strip():
        raise HTTPException(status_code=400, detail="Body must include a 'code' string.")
    try:
        Settings().persist("currency", code.strip())
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    # COST-7: ``active_currency_payload`` is memoized in-process for ~60s so the
    # hot read paths do no disk I/O. That memo caches the OLD code, so the write
    # path has to drop it — otherwise this very response (and every request for
    # the next minute) would report the currency the user just replaced.
    clear_currency_memo()
    # Deliberately NOT invalidating the dashboard cache here (#31). Cached
    # payloads are stored USD-denominated and every response re-applies the
    # active currency (routes/data._apply_currency_to_stats), so a currency
    # switch is a cheap per-request rescale — flushing would force a full
    # re-aggregation of every project for a change that never touches the
    # cached numbers. Aggregation-affecting writes (model aliases) still flush.
    return JSONResponse({
        "currency": active_currency_payload(),
    })


@router.get("/api/cfg/model-aliases")
async def get_model_aliases() -> JSONResponse:
    """Return the current proxy → canonical alias map."""
    aliases = Settings().get("model_aliases") or {}
    return JSONResponse({"aliases": dict(aliases)})


@router.post("/api/cfg/model-aliases")
async def set_model_alias(data: dict) -> JSONResponse:
    """Add or update one alias.

    Body: ``{"from": "<proxy>", "to": "<canonical>"}``. Both fields are
    required and must be non-empty strings. The pair is appended to
    ``model_aliases`` and persisted.
    """
    src = data.get("from")
    dst = data.get("to")
    if not isinstance(src, str) or not src.strip():
        raise HTTPException(status_code=400, detail="'from' must be a non-empty string.")
    if not isinstance(dst, str) or not dst.strip():
        raise HTTPException(status_code=400, detail="'to' must be a non-empty string.")
    s = Settings()
    aliases = dict(s.get("model_aliases") or {})
    aliases[src.strip()] = dst.strip()
    s.persist("model_aliases", aliases)
    invalidate_dashboard_cache()
    # The stats memo aggregates per-model too, and its sessions signature does
    # NOT move on a config edit — without this the Cost tab kept serving
    # pre-alias grouping until the next ingest while the dashboard already
    # showed the new one. Full clear: an alias is global, not per-slug.
    _invalidate_stats_cache()
    return JSONResponse({"aliases": aliases})


# ``from`` is a Python keyword so the parameter is named ``src`` and
# aliased back via FastAPI's Query alias.
_FROM_Q = Query("", alias="from", description="Proxy id to remove.")


@router.delete("/api/cfg/model-aliases")
async def delete_model_alias(src: str = _FROM_Q) -> JSONResponse:
    """Remove one alias. 404 if not present. Empty ``from`` is a 400."""
    if not src:
        raise HTTPException(status_code=400, detail="'from' query parameter is required.")
    s = Settings()
    aliases = dict(s.get("model_aliases") or {})
    if src not in aliases:
        raise HTTPException(status_code=404, detail=f"No alias for {src!r}.")
    aliases.pop(src)
    s.persist("model_aliases", aliases)
    invalidate_dashboard_cache()
    _invalidate_stats_cache()  # same blast radius as the set path above
    return JSONResponse({"aliases": aliases})

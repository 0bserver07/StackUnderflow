"""``/api/sync`` — opt-in multi-device sync status + the cross-device overview.

Phase 2 read surface for ``docs/specs/multi-device-sync.md``. Two endpoints, both
read-only and both safe on a core install (no ``pyrage`` / ``boto3`` needed — they
read the local store and the already-landed ``<mart>_remote`` tables; decryption
happened earlier, in ``sync pull``).

``GET /api/sync/status``
    Local sync config (device UUID, fingerprint, bucket, pending-upload count)
    plus the known peers and whether any cross-device data has been pulled. Pure
    local read; works whether sync is on or off.

``GET /api/sync/overview?scope=<this-device|all-devices>``
    **Default ``this-device``** — returns a tiny "not merged" stub and runs **no**
    union query, so this endpoint is off the mart ``<100ms`` fast-path and a store
    with sync off behaves as if the feature were absent. Only ``?scope=all-devices``
    (and sync enabled) computes :func:`stackunderflow.sync.merge.merged_overview`
    — the ``local UNION ALL <mart>_remote`` roll-up. Cost figures are pre-converted
    into the active currency, matching every other cost endpoint's contract.

This is a *new, additive* surface: no existing route or query changes, so the
default dashboard path stays byte-identical.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from fastapi import APIRouter, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.store import db
from stackunderflow.sync import merge, runner

router = APIRouter()

_SCOPE_QUERY = Query(
    "this-device",
    description="'all-devices' to union pulled peers; anything else = this-device-only (default)",
)


def _list_peers(conn: Any) -> list[dict]:
    """Known peer devices from ``sync_remote_devices`` (empty until first pull)."""
    rows = conn.execute(
        "SELECT remote_device_uuid, alias, key_fingerprint, "
        "       first_seen, last_seen, last_generation "
        "FROM sync_remote_devices ORDER BY remote_device_uuid"
    ).fetchall()
    return [dict(r) for r in rows]


def _apply_currency(payload: dict, currency: dict) -> None:
    """Pre-convert every USD cost field in *payload* into the active currency.

    No-op at rate 1.0 (the default), so the merged figures stay in USD unless the
    user configured a display currency — the frontend then never multiplies again.
    """
    rate = currency["rate_from_usd"]
    if rate == 1.0:
        return
    payload["totals"]["cost_usd"] = float(payload["totals"]["cost_usd"]) * rate
    for row in payload["by_day"]:
        row["cost_usd"] = float(row["cost_usd"]) * rate
    for row in payload["by_project"]:
        row["total_cost_usd"] = float(row["total_cost_usd"] or 0.0) * rate
    for row in payload["by_provider_day"]:
        row["cost_usd"] = float(row["cost_usd"] or 0.0) * rate
    for row in payload["devices"]:
        row["cost_usd"] = float(row["cost_usd"] or 0.0) * rate


@router.get("/api/sync/status")
async def get_sync_status() -> dict:
    """Local sync config + peers + whether cross-device data is available.

    Purely local; never hits the network or a bucket and needs no optional deps.
    """
    conn = db.connect(deps.store_path)
    try:
        status = runner.status(conn).as_dict()
        peers = _list_peers(conn)
        remote_rows = merge.remote_row_count(conn)
    finally:
        conn.close()

    status["peers"] = peers
    status["peer_count"] = len(peers)
    status["remote_rows"] = remote_rows
    # The FE shows the all-devices toggle only when there is something to merge.
    status["all_devices_available"] = bool(status["enabled"] and remote_rows > 0)
    status["scanned_at"] = datetime.now(UTC).isoformat()
    return status


@router.get("/api/sync/overview")
async def get_sync_overview(scope: str = _SCOPE_QUERY) -> dict:
    """This-device stub by default; the merged cross-device roll-up on opt-in.

    ``?scope=all-devices`` (with sync enabled) returns the ``local UNION ALL
    <mart>_remote`` overview — totals, per-day trend, per-project, per-provider-day,
    a per-device breakdown, and the ``merge_warnings`` count. Any other scope, or
    sync disabled, returns a minimal not-merged stub and runs no union.
    """
    scope_str = scope if isinstance(scope, str) else "this-device"

    conn = db.connect(deps.store_path)
    try:
        enabled = runner.is_enabled(conn)
        if scope_str != "all-devices" or not enabled:
            # DEFAULT this-device path: no union runs — off the fast-path, and a
            # sync-off store behaves exactly as if the feature were absent.
            return {
                "scope": "this-device",
                "merged": False,
                "sync_enabled": enabled,
                "hint": "pass ?scope=all-devices to union pulled peers",
            }
        payload = merge.merged_overview(conn)
    finally:
        conn.close()

    currency = active_currency_payload()
    _apply_currency(payload, currency)
    payload["scope"] = "all-devices"
    payload["merged"] = True
    payload["sync_enabled"] = True
    payload["currency"] = currency
    payload["generated_at"] = datetime.now(UTC).isoformat()
    return payload

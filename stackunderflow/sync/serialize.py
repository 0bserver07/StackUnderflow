"""Canonical, deterministic mart-shard serialization + re-keying.

A **shard** is one ``(mart family, month)`` — e.g. ``daily_mart`` for
``2026-07``. Each shard serializes to a canonical, deterministic byte form
(sorted rows, fixed field order, no wall-clock / random) so a given dataset
always hashes identically. The SHA-256 of those bytes is the shard's
**content-hash**, which drives push idempotency (§5.4 of the spec).

**Re-keying (§4.5).** ``projects.id`` is a machine-local autoincrement, so raw
mart rows are not cross-device-comparable. At export every shard is re-keyed
from the local ``project_id`` to the machine-stable identity ``(provider,
slug)`` via a ``JOIN projects`` at serialize time, and grouped/summed at that
stable grain. Two machines that assign different local ids to the same
``(provider, slug)`` therefore produce identical shard bytes — the property the
cross-device union relies on.

**What ships in the MVP** — the Overview/Cost core: ``daily_mart``,
``project_mart``, ``provider_day_mart``, ``model_day_mart``, ``session_mart``.
``message_tool_mart`` (carries ``file_path``) is excluded. ``usage_events`` and
``price_book`` are never read here — only the derived marts move, and
``session_mart.cwd`` (a filesystem path) is deliberately dropped.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
from collections import defaultdict
from dataclasses import dataclass


@dataclass(frozen=True)
class _MartSpec:
    """A mart family's export query, canonical columns, and month grouping."""

    family: str
    columns: tuple[str, ...]
    sql: str
    # Column whose value's ``YYYY-MM`` prefix buckets rows into monthly shards.
    # ``None`` means the whole mart is a single ``"all"`` shard.
    month_column: str | None


# The five Overview/Cost-core marts, each re-keyed to ``(provider, slug)`` where
# it carries a local ``project_id``. ``provider_day_mart`` / ``model_day_mart``
# carry no ``project_id`` and need no re-key. ``session_mart.cwd`` is dropped
# (path-shaped). SELECT column order MUST match ``columns``.
_SPECS: tuple[_MartSpec, ...] = (
    _MartSpec(
        family="daily_mart",
        columns=(
            "day", "provider", "slug", "model", "speed",
            "input_tokens", "output_tokens", "cache_read", "cache_create",
            "message_count", "session_count", "cost_usd",
        ),
        sql=(
            "SELECT d.day, d.provider, p.slug, d.model, d.speed, "
            "       SUM(d.input_tokens), SUM(d.output_tokens), "
            "       SUM(d.cache_read), SUM(d.cache_create), "
            "       SUM(d.message_count), SUM(d.session_count), SUM(d.cost_usd) "
            "FROM daily_mart d JOIN projects p ON p.id = d.project_id "
            "GROUP BY d.day, d.provider, p.slug, d.model, d.speed "
            "ORDER BY d.day, d.provider, p.slug, d.model, d.speed"
        ),
        month_column="day",
    ),
    _MartSpec(
        family="provider_day_mart",
        columns=(
            "day", "provider", "cost_usd",
            "message_count", "session_count", "project_count",
        ),
        sql=(
            "SELECT day, provider, cost_usd, message_count, session_count, project_count "
            "FROM provider_day_mart "
            "ORDER BY day, provider"
        ),
        month_column="day",
    ),
    _MartSpec(
        family="model_day_mart",
        columns=(
            "day", "model", "speed", "cost_usd",
            "input_tokens", "output_tokens", "cache_read", "cache_create",
            "message_count", "session_count",
        ),
        sql=(
            "SELECT day, model, speed, cost_usd, input_tokens, output_tokens, "
            "       cache_read, cache_create, message_count, session_count "
            "FROM model_day_mart "
            "ORDER BY day, model, speed"
        ),
        month_column="day",
    ),
    _MartSpec(
        family="project_mart",
        columns=(
            "provider", "slug", "display_name", "first_ts", "last_ts",
            "total_messages", "total_sessions", "total_input_tokens",
            "total_output_tokens", "total_cache_read", "total_cache_create",
            "total_cost_usd",
        ),
        sql=(
            "SELECT provider, slug, display_name, first_ts, last_ts, "
            "       SUM(total_messages), SUM(total_sessions), SUM(total_input_tokens), "
            "       SUM(total_output_tokens), SUM(total_cache_read), "
            "       SUM(total_cache_create), SUM(total_cost_usd) "
            "FROM project_mart "
            "GROUP BY provider, slug, display_name, first_ts, last_ts "
            "ORDER BY provider, slug"
        ),
        month_column=None,
    ),
    _MartSpec(
        family="session_mart",
        columns=(
            "session_id", "provider", "slug", "primary_model", "first_ts", "last_ts",
            "message_count", "user_message_count", "assistant_message_count",
            "input_tokens", "output_tokens", "cache_read", "cache_create",
            "cost_usd", "is_one_shot",
        ),
        sql=(
            "SELECT s.session_id, s.provider, p.slug, s.primary_model, "
            "       s.first_ts, s.last_ts, s.message_count, s.user_message_count, "
            "       s.assistant_message_count, s.input_tokens, s.output_tokens, "
            "       s.cache_read, s.cache_create, s.cost_usd, s.is_one_shot "
            "FROM session_mart s JOIN projects p ON p.id = s.project_id "
            "ORDER BY s.session_id"
        ),
        month_column="first_ts",
    ),
)

#: The mart families the MVP syncs, in a stable order.
MART_FAMILIES: tuple[str, ...] = tuple(spec.family for spec in _SPECS)

#: Serialization format version, embedded in each shard's canonical bytes.
FORMAT_VERSION = 1


@dataclass(frozen=True)
class Shard:
    """One ``(family, month)`` shard of re-keyed, canonically-ordered mart rows."""

    family: str
    month: str  # "YYYY-MM", or "all" for month-less marts (project_mart)
    columns: tuple[str, ...]
    rows: tuple[tuple, ...]

    @property
    def shard_key(self) -> str:
        """Stable logical key, e.g. ``daily_mart.2026-07`` / ``project_mart.all``."""
        return f"{self.family}.{self.month}"

    def to_bytes(self) -> bytes:
        """Canonical, deterministic serialization (drives the content hash)."""
        payload = {
            "v": FORMAT_VERSION,
            "family": self.family,
            "month": self.month,
            "columns": list(self.columns),
            "rows": [list(row) for row in self.rows],
        }
        return json.dumps(
            payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")

    @property
    def content_hash(self) -> str:
        """SHA-256 hex digest of the canonical bytes."""
        return hashlib.sha256(self.to_bytes()).hexdigest()


def _month_of(value: object) -> str:
    """``YYYY-MM`` prefix of a date/timestamp string (``day`` or ``first_ts``)."""
    text = str(value)
    return text[:7] if len(text) >= 7 else "unknown"


def build_shards(conn: sqlite3.Connection) -> list[Shard]:
    """Build every current mart shard from *conn*, re-keyed to ``(provider, slug)``.

    Read-only: touches only the mart tables (and ``projects`` for the re-key).
    Never reads or writes ``usage_events`` / ``price_book`` / raw transcripts.
    """
    shards: list[Shard] = []
    for spec in _SPECS:
        rows = [tuple(r) for r in conn.execute(spec.sql).fetchall()]
        if spec.month_column is None:
            if rows:
                shards.append(Shard(spec.family, "all", spec.columns, tuple(rows)))
            continue
        month_idx = spec.columns.index(spec.month_column)
        by_month: dict[str, list[tuple]] = defaultdict(list)
        for row in rows:
            by_month[_month_of(row[month_idx])].append(row)
        for month in sorted(by_month):
            shards.append(Shard(spec.family, month, spec.columns, tuple(by_month[month])))
    return shards


def shard_from_bytes(data: bytes) -> Shard:
    """Inverse of :meth:`Shard.to_bytes` — reconstruct a shard from canonical bytes."""
    obj = json.loads(data)
    return Shard(
        family=obj["family"],
        month=obj["month"],
        columns=tuple(obj["columns"]),
        rows=tuple(tuple(row) for row in obj["rows"]),
    )

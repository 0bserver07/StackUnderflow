"""Convert enriched records back to the dict format the REST API serves."""

from __future__ import annotations

from .enricher import EnrichedDataset, Interaction, Record


def to_dicts(
    ds: EnrichedDataset,
    *,
    limit: int | None = None,
) -> list[dict]:
    """Return a list of message dicts matching the REST API contract.

    The list is sorted by timestamp. Interaction metadata is stamped onto
    user-command messages so that the frontend / stats code can read
    ``interaction_tool_count``, ``interaction_model``, and
    ``interaction_assistant_steps``.
    """
    # build lookup: command record id → interaction
    ix_by_cmd: dict[int, Interaction] = {}
    for ix in ds.interactions:
        ix_by_cmd[id(ix.command)] = ix

    dicts: list[dict] = []
    for rec in ds.records:
        d = _record_to_dict(rec)
        # stamp interaction metadata onto user commands
        ix = ix_by_cmd.get(id(rec))
        if ix is not None:
            d["interaction_tool_count"] = ix.tool_count
            d["interaction_model"] = ix.model
            d["interaction_assistant_steps"] = ix.assistant_steps
        dicts.append(d)

    dicts.sort(key=lambda m: m["timestamp"] if m["timestamp"] else "")

    if limit is not None:
        dicts = dicts[:limit]

    return dicts


def _record_to_dict(rec: Record) -> dict:
    return {
        "session_id": rec.session_id,
        "type": rec.kind,
        "timestamp": rec.timestamp,
        "model": rec.model,
        "content": rec.content,
        "tools": rec.tools,
        "tokens": rec.tokens,
        "cwd": rec.cwd,
        "uuid": rec.uuid,
        "parent_uuid": rec.parent_uuid,
        "is_sidechain": rec.is_sidechain,
        "has_tool_result": rec.has_tool_result,
        "error": rec.is_error,
        "is_interruption": rec.is_interruption,
        "message_id": rec.message_id,
    }

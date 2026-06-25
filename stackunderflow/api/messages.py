"""
Messages API endpoint with pagination support.
"""


def page_bounds(total: int, page: int, per_page: int) -> tuple[int, int, int, int]:
    """Clamp ``page`` against ``total`` and compute slice bounds.

    Returns ``(page, total_pages, start_index, end_index)``. ``end_index`` is
    the raw stop index (``start_index + per_page``); callers that surface it
    take ``min(end_index, total)``. This is the single definition of the
    pagination math shared by :func:`get_paginated_messages` (which slices an
    in-memory list) and :func:`build_messages_page` (which wraps a page that
    was already sliced in SQL), so both stay byte-for-byte compatible.
    """
    total_pages = (total + per_page - 1) // per_page
    if page < 1:
        page = 1
    elif page > total_pages:
        page = total_pages
    start_idx = (page - 1) * per_page
    end_idx = start_idx + per_page
    return page, total_pages, start_idx, end_idx


def build_messages_page(page_messages: list[dict], *, total: int, page: int, per_page: int) -> dict:
    """Wrap an already-SQL-paginated page in the canonical envelope.

    ``page_messages`` are the rows for ``page`` (length ``<= per_page``) and
    ``total`` is the full (post-filter) row count. Produces the exact envelope
    :func:`get_paginated_messages` would have for the same slice, so callers
    that push ``LIMIT``/``OFFSET`` into SQL keep the historical contract
    (``messages, total, page, per_page, total_pages, start_index, end_index``)
    without materialising the whole list.
    """
    page, total_pages, start_idx, end_idx = page_bounds(total, page, per_page)
    return {
        "messages": page_messages,
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
        "start_index": start_idx,
        "end_index": min(end_idx, total),
    }


def get_paginated_messages(messages: list[dict], page: int = 1, per_page: int = 100, include_all: bool = False) -> dict:
    """
    Return paginated messages or all messages based on flag.

    Args:
        messages: Full list of messages
        page: Page number (1-indexed)
        per_page: Items per page
        include_all: If True, return all messages (for backwards compatibility)

    Returns:
        Dictionary with messages and pagination info
    """
    if include_all:
        # Return all messages for charts and full analysis
        return {"messages": messages, "total": len(messages), "page": 1, "per_page": len(messages), "total_pages": 1}

    total = len(messages)
    page, total_pages, start_idx, end_idx = page_bounds(total, page, per_page)
    page_messages = messages[start_idx:end_idx]

    return {
        "messages": page_messages,
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
        "start_index": start_idx,
        "end_index": min(end_idx, total),
    }


def get_messages_summary(messages: list[dict]) -> dict:
    """
    Get summary statistics about messages without returning all data.

    Args:
        messages: Full list of messages

    Returns:
        Summary statistics
    """
    if not messages:
        return {"total": 0, "by_type": {}, "by_model": {}, "total_tokens": 0}

    by_type = {}
    by_model = {}
    total_tokens = 0

    for msg in messages:
        # Count by type
        msg_type = msg.get("type", "unknown")
        by_type[msg_type] = by_type.get(msg_type, 0) + 1

        # Count by model
        model = msg.get("model", "unknown")
        by_model[model] = by_model.get(model, 0) + 1

        # Sum tokens
        tokens = msg.get("tokens", {})
        if isinstance(tokens, dict):
            total_tokens += tokens.get("input", 0) + tokens.get("output", 0)

    return {"total": len(messages), "by_type": by_type, "by_model": by_model, "total_tokens": total_tokens}

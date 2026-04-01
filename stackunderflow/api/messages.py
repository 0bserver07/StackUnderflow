"""
Messages API endpoint with pagination support.
"""


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

    # Calculate pagination
    total = len(messages)
    total_pages = (total + per_page - 1) // per_page

    # Validate page number
    if page < 1:
        page = 1
    elif page > total_pages:
        page = total_pages

    # Get page slice
    start_idx = (page - 1) * per_page
    end_idx = start_idx + per_page
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

"""Log-processing pipeline.

Entry point: ``process(log_dir)`` returns ``(messages, statistics)``
with the same JSON shapes the REST API has always served.
"""

from __future__ import annotations

from . import aggregator, classifier, dedup, enricher, formatter, reader


def process(
    log_dir: str,
    *,
    limit: int | None = None,
    tz_offset: int = 0,
) -> tuple[list[dict], dict]:
    """Run the full pipeline and return *(messages, statistics)*.

    Parameters
    ----------
    log_dir:
        Absolute path to a Claude-log directory (contains ``*.jsonl``).
    limit:
        Cap the number of returned messages (``None`` = all).
    tz_offset:
        Timezone offset in minutes from UTC sent by the frontend.
    """
    raw_entries = reader.scan(log_dir)
    merged = dedup.collapse(raw_entries)
    tagged = classifier.tag(merged)
    dataset = enricher.build(tagged, log_dir)
    messages = formatter.to_dicts(dataset, limit=limit)
    statistics = aggregator.summarise(dataset, log_dir, tz_offset=tz_offset)
    return messages, statistics

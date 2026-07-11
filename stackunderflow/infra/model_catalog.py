"""Model-candidate catalog — loaded from ``model_candidates.json``.

The what-if / routing model set is DATA shipped with the package, not
Python literals scattered per consumer (it previously lived as two
hand-synced copies in ``services/whatif.py`` and ``reports/prescribe.py``).
Entries name models only, never rates — ``compute_cost`` remains the single
source of every dollar figure.
"""

from __future__ import annotations

import json
from functools import lru_cache
from importlib import resources

# ``(pricer, model_id, label)`` triples, cheap → premium within a pricer.
Candidate = tuple[str, str, str]


@lru_cache(maxsize=1)
def _load() -> tuple[dict, ...]:
    raw = json.loads(
        resources.files("stackunderflow.infra")
        .joinpath("model_candidates.json")
        .read_text(encoding="utf-8")
    )
    # tuple: lru_cache must not hand out a shared mutable list
    return tuple(raw["candidates"])


def whatif_candidates() -> tuple[Candidate, ...]:
    """Every catalog entry — the what-if comparison set."""
    return tuple((e["pricer"], e["model"], e["label"]) for e in _load())


def routing_candidates() -> tuple[Candidate, ...]:
    """Entries a routing recommendation may name (directly pickable in the
    user's tool; proxy-priced entries are catalog-flagged out)."""
    return tuple(
        (e["pricer"], e["model"], e["label"])
        for e in _load()
        if e.get("routing_candidate", True)
    )

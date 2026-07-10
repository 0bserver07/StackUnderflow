"""Adapter ↔ normalizer parity — the guard against silently-stranded data.

``ingest/writer._normalize_new_messages`` returns 0 **silently** when a
provider has no registered normalizer, so an adapter without one loads base
rows that can never become ``usage_events`` — invisible to every mart, the
dashboard, and the backup-visible aggregates. This is exactly how codex
(model=None), antigravity (no normalizer), and cursor-agent (normalizer
registered under the wrong key) went dark while every unit suite stayed
green.

Exemptions are **data, not code**: ``adapters/capabilities.json`` marks a
provider whose source can never yield billable events with
``emits_usage_events: false`` and states the reason in ``notes``.
No agent name is hardcoded here.
"""

from stackunderflow.etl import normalize
from stackunderflow.services.support_matrix import _CAPABILITIES, discover_adapters

# NOTE: expectations derive from the package walk (discover_adapters), not
# from adapters.registered() — the registry is a mutable global that other
# tests inject doubles into mid-suite. The walk-equals-registry invariant is
# separately enforced by tests/stackunderflow/adapters/test_default_registry.py.


def test_every_event_emitting_adapter_has_a_normalizer():
    """An adapter that can emit billable events must have a normalizer
    registered under its exact provider name — a missing or mis-keyed
    normalizer strands that provider's rows in the base tables."""
    normalizer_keys = set(normalize.registered_providers())
    missing = []
    for name in discover_adapters():
        cap = _CAPABILITIES.get(name)
        assert cap is not None, f"{name!r} has no entry in capabilities.json"
        if not cap["emits_usage_events"]:
            continue
        if name not in normalizer_keys:
            missing.append(name)
    assert not missing, (
        f"providers whose data can NEVER reach usage_events: {missing} — "
        "register a normalizer under exactly this name in etl/normalize/"
    )


def test_exempt_providers_document_why():
    """An emits_usage_events=false exemption must say why in its notes —
    an undocumented exemption is just the old silent gap wearing a flag."""
    for name, cap in _CAPABILITIES.items():
        if not cap["emits_usage_events"]:
            assert cap["notes"].strip(), (
                f"{name!r} is exempt from usage_events but gives no reason"
            )


def test_capability_table_covers_every_discovered_adapter():
    """Every adapter in the package has a curated capabilities.json row
    (the inverse drift — table entries for adapters that don't exist — is
    covered by the support-matrix drift test)."""
    for name in discover_adapters():
        assert name in _CAPABILITIES, name

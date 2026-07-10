"""Plug-in registry contracts for normalize + marts.

Both registries follow the same shape: ``register(name, cls)`` /
``get(name)`` / ``all()`` with last-wins semantics on duplicate names.
These tests pin both registries against the same checklist so a future
divergence shows up immediately. Wave 2 dispatches in parallel against
this contract — the tests are the contract.
"""

from __future__ import annotations

from collections.abc import Iterable

from stackunderflow.etl import marts as marts_registry
from stackunderflow.etl import normalize as normalize_registry
from stackunderflow.etl.marts.base import MartBuilder
from stackunderflow.etl.normalize.base import Normalizer

# ── normalizer fixtures ─────────────────────────────────────────────────────


class _DummyClaude(Normalizer):
    provider_name = "claude-test"

    def normalize(self, msg_row: dict) -> Iterable[dict]:  # pragma: no cover
        return iter(())


class _DummyClaudeAlt(Normalizer):
    """Second class with the same provider_name to exercise overwrite."""

    provider_name = "claude-test"

    def normalize(self, msg_row: dict) -> Iterable[dict]:  # pragma: no cover
        return iter(())


class _DummyCodex(Normalizer):
    provider_name = "codex-test"

    def normalize(self, msg_row: dict) -> Iterable[dict]:  # pragma: no cover
        return iter(())


# ── mart fixtures ───────────────────────────────────────────────────────────


class _DummyDailyMart(MartBuilder):
    name = "daily-test"

    def refresh(self, conn, since_event_id: int) -> int:  # pragma: no cover
        return since_event_id


class _DummyDailyMartAlt(MartBuilder):
    name = "daily-test"

    def refresh(self, conn, since_event_id: int) -> int:  # pragma: no cover
        return since_event_id


class _DummySessionMart(MartBuilder):
    name = "session-test"

    def refresh(self, conn, since_event_id: int) -> int:  # pragma: no cover
        return since_event_id


# ── normalize registry ──────────────────────────────────────────────────────


def test_normalize_register_get_all():
    normalize_registry._clear()

    normalize_registry.register("claude-test", _DummyClaude)
    normalize_registry.register("codex-test", _DummyCodex)

    assert normalize_registry.get("claude-test") is _DummyClaude
    assert normalize_registry.get("codex-test") is _DummyCodex
    assert normalize_registry.get("nope-not-here") is None

    snapshot = normalize_registry.all()
    assert snapshot == {"claude-test": _DummyClaude, "codex-test": _DummyCodex}


def test_normalize_all_returns_copy():
    """Mutating the returned dict must not affect the registry."""
    normalize_registry._clear()
    normalize_registry.register("claude-test", _DummyClaude)

    snap = normalize_registry.all()
    snap.clear()  # mutate the caller's copy

    # Live registry still has the entry.
    assert normalize_registry.get("claude-test") is _DummyClaude


def test_normalize_register_twice_overwrites_last_wins():
    """Re-registering the same provider replaces the prior class."""
    normalize_registry._clear()

    normalize_registry.register("claude-test", _DummyClaude)
    assert normalize_registry.get("claude-test") is _DummyClaude

    normalize_registry.register("claude-test", _DummyClaudeAlt)
    assert normalize_registry.get("claude-test") is _DummyClaudeAlt

    # Only one entry, the new one — not two.
    assert normalize_registry.all() == {"claude-test": _DummyClaudeAlt}


# ── marts registry ──────────────────────────────────────────────────────────


def test_marts_register_get_all():
    marts_registry._clear()

    marts_registry.register("daily-test", _DummyDailyMart)
    marts_registry.register("session-test", _DummySessionMart)

    assert marts_registry.get("daily-test") is _DummyDailyMart
    assert marts_registry.get("session-test") is _DummySessionMart
    assert marts_registry.get("nope-not-here") is None

    snapshot = marts_registry.all()
    assert snapshot == {
        "daily-test": _DummyDailyMart,
        "session-test": _DummySessionMart,
    }


def test_marts_all_returns_copy():
    marts_registry._clear()
    marts_registry.register("daily-test", _DummyDailyMart)

    snap = marts_registry.all()
    snap.clear()

    assert marts_registry.get("daily-test") is _DummyDailyMart


def test_marts_register_twice_overwrites_last_wins():
    marts_registry._clear()

    marts_registry.register("daily-test", _DummyDailyMart)
    assert marts_registry.get("daily-test") is _DummyDailyMart

    marts_registry.register("daily-test", _DummyDailyMartAlt)
    assert marts_registry.get("daily-test") is _DummyDailyMartAlt

    assert marts_registry.all() == {"daily-test": _DummyDailyMartAlt}


def test_clear_helpers_reset_state():
    """``_clear`` is the test-only escape hatch — wipes both registries."""
    normalize_registry.register("provider-x", _DummyClaude)
    marts_registry.register("mart-x", _DummyDailyMart)

    normalize_registry._clear()
    marts_registry._clear()

    assert normalize_registry.all() == {}
    assert marts_registry.all() == {}


# ── normalize registry self-discovery contract ──────────────────────────────


def test_normalize_registry_matches_package_walk():
    """The live registry equals an independent walk of the package: every
    concrete Normalizer with a provider_name (plus its aliases) is
    registered — nothing more, nothing less, no hardcoded names here."""
    import importlib
    import inspect
    import pkgutil

    import stackunderflow.etl.normalize as norm
    from stackunderflow.etl.normalize.base import Normalizer

    expected: dict[str, type] = {}
    for mod_info in pkgutil.iter_modules(norm.__path__):
        if mod_info.name.startswith("_") or mod_info.name == "base":
            continue
        module = importlib.import_module(
            f"stackunderflow.etl.normalize.{mod_info.name}"
        )
        for cls_name, obj in vars(module).items():
            if (
                inspect.isclass(obj)
                and obj.__module__ == module.__name__
                and not cls_name.startswith("_")
                and issubclass(obj, Normalizer)
                and getattr(obj, "provider_name", "")
            ):
                expected[obj.provider_name] = obj
                for alias in getattr(obj, "provider_aliases", ()):
                    expected[alias] = obj

    live = importlib.reload(norm).all()
    assert live, "normalize registry must not be empty"
    assert live == expected


def test_every_normalize_module_contributes():
    """A normalizer file can't sit in the package registering nothing."""
    import importlib
    import inspect
    import pkgutil

    import stackunderflow.etl.normalize as norm
    from stackunderflow.etl.normalize.base import Normalizer

    for mod_info in pkgutil.iter_modules(norm.__path__):
        if mod_info.name.startswith("_") or mod_info.name == "base":
            continue
        module = importlib.import_module(
            f"stackunderflow.etl.normalize.{mod_info.name}"
        )
        assert any(
            inspect.isclass(obj)
            and obj.__module__ == module.__name__
            and issubclass(obj, Normalizer)
            and getattr(obj, "provider_name", "")
            for obj in vars(module).values()
        ), f"normalize/{mod_info.name}.py registers no normalizer"

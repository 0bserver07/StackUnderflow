"""Provider normalizer registry.

Each provider's adapter ships a :class:`Normalizer` subclass that turns a
``messages`` row into 0..N ``usage_events`` rows. Modules import this
package and call :func:`register` at import time so the orchestrator can
discover them via :func:`all`.

Registry is module-level state keyed on ``provider_name``. ``register``
is **last-wins**: re-registering the same name silently overwrites the
prior class. That makes hot-reload during development and registry-
overrides in tests trivial — no "already registered" error to navigate
around.
"""

from __future__ import annotations

from .base import Normalizer

_REGISTRY: dict[str, type[Normalizer]] = {}


def register(provider: str, normalizer_cls: type[Normalizer]) -> None:
    """Register *normalizer_cls* for *provider*.

    Last-wins: re-registering the same provider silently overwrites the
    prior class. Tests and hot-reload depend on this behaviour.
    """
    _REGISTRY[provider] = normalizer_cls


def get(provider: str) -> type[Normalizer] | None:
    """Return the registered class for *provider*, or ``None``."""
    return _REGISTRY.get(provider)


def all() -> dict[str, type[Normalizer]]:  # noqa: A001 — spec-defined name
    """Return a snapshot of the registry.

    Returns a *copy* so callers can iterate while other code registers
    without mutating the live dict mid-loop.

    Shadows the ``all`` builtin by design — the spec
    (``docs/specs/etl-architecture.md``) names this method ``all()`` to
    pair with ``register()``/``get()``. Callers either ``from ... import
    normalize`` then ``normalize.all()``, or never need the builtin.
    """
    return dict(_REGISTRY)


def _clear() -> None:
    """Test-only: reset the registry between tests.

    Public ``register`` is last-wins, so most tests don't need this. Use
    only when a test wants to assert the empty-registry path."""
    _REGISTRY.clear()


__all__ = ["Normalizer", "register", "get", "all"]

"""Mart builder registry.

Each mart ships a :class:`MartBuilder` subclass. Modules import this
package and call :func:`register` at import time so the orchestrator
can discover them via :func:`all`.

Registry is module-level state keyed on the mart ``name`` (e.g.
``"daily"``, ``"session"``). ``register`` is **last-wins** — re-registering
silently overwrites the prior class so hot-reload and test overrides
work without error gymnastics.
"""

from __future__ import annotations

from .base import MartBuilder

_REGISTRY: dict[str, type[MartBuilder]] = {}


def register(name: str, mart_cls: type[MartBuilder]) -> None:
    """Register *mart_cls* under *name*.

    Last-wins: re-registering the same name silently overwrites.
    """
    _REGISTRY[name] = mart_cls


def get(name: str) -> type[MartBuilder] | None:
    """Return the registered class for *name*, or ``None``."""
    return _REGISTRY.get(name)


def all() -> dict[str, type[MartBuilder]]:  # noqa: A001 — spec-defined name
    """Return a snapshot of the registry (a copy, safe to iterate).

    Shadows the ``all`` builtin by design — the spec
    (``docs/specs/etl-architecture.md``) names this method ``all()`` to
    pair with ``register()``/``get()``. Callers import the module
    (``from stackunderflow.etl import marts``) and call
    ``marts.all()`` so the builtin stays accessible.
    """
    return dict(_REGISTRY)


def _clear() -> None:
    """Test-only: reset the registry between tests."""
    _REGISTRY.clear()


__all__ = ["MartBuilder", "register", "get", "all"]

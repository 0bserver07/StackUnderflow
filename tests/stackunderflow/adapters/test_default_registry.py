"""Default-registry contract.

Locks in which adapters register themselves with no env vars set. As of
v0.7.0 this is Claude, Codex, Cursor, and Cline; the other 12 beta
adapters stay opt-in via ``STACKUNDERFLOW_BETA_<NAME>=1``.
"""

import importlib
import os

import stackunderflow.adapters as adapters_module


def _clear_beta_env(monkeypatch):
    for k in list(os.environ):
        if k.startswith("STACKUNDERFLOW_BETA_"):
            monkeypatch.delenv(k, raising=False)


def _reload() -> set[str]:
    reloaded = importlib.reload(adapters_module)
    return {a.name for a in reloaded.registered()}


def test_cursor_and_cline_default_on(monkeypatch):
    """With no env vars set, cursor, cline, openclaw, pi, and hermes must
    register alongside claude + codex."""
    _clear_beta_env(monkeypatch)
    names = _reload()
    assert "claude" in names
    assert "codex" in names
    assert "cursor" in names
    assert "cline" in names
    assert "openclaw" in names
    assert "pi" in names
    assert "hermes" in names


def test_other_betas_stay_off_by_default(monkeypatch):
    """The remaining beta adapters must NOT register without their
    env var. ``cursor-agent`` is the registered name for the Cursor
    Agent adapter (distinct from the default-on ``cursor`` adapter)."""
    _clear_beta_env(monkeypatch)
    names = _reload()
    for beta in (
        "kilocode",
        "roocode",
        "opencode",
        "cursor-agent",
        "qwen",
        "gemini",
        "copilot",
        "codeium",
        "continue",
        "droid",
        "kiro",
    ):
        assert beta not in names, f"{beta!r} must stay opt-in"


def test_default_registry_is_exactly_seven(monkeypatch):
    """No surprise registrations — the default contract is exactly seven
    adapters. New default-on additions should bump this number
    intentionally."""
    _clear_beta_env(monkeypatch)
    names = _reload()
    assert names == {"claude", "codex", "cursor", "cline", "openclaw", "pi", "hermes"}


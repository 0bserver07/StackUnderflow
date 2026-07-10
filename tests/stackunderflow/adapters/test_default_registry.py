"""Default-registry contract — structural, with no hardcoded agent list.

The registry in ``stackunderflow.adapters`` self-discovers: it walks the
package at import time and registers every public class satisfying the
``SourceAdapter`` shape. These tests re-derive the expected set from the
package **independently** (their own walk, written separately from the
implementation), so:

* a new adapter file is covered automatically — no test edit needed;
* an adapter file whose class silently fails to register fails loudly;
* no agent name is hardcoded anywhere in this file.
"""

import importlib
import inspect
import pkgutil
from pathlib import Path

import stackunderflow.adapters as adapters_module

# Package modules that are shared infrastructure, not agent adapters. If a
# module in this set ever *does* grow a conforming adapter class, the
# walk-vs-registry equality test below fails and forces a decision.
_INFRA_MODULES = frozenset({"base", "custom_jsonl", "custom_import", "claude_teams"})


def _independent_walk() -> dict[str, str]:
    """Re-derive {adapter_name: module_name} straight from the package."""
    found: dict[str, str] = {}
    for mod_info in pkgutil.iter_modules(adapters_module.__path__):
        if mod_info.name.startswith("_"):
            continue
        module = importlib.import_module(f"stackunderflow.adapters.{mod_info.name}")
        for cls_name, obj in vars(module).items():
            if not inspect.isclass(obj) or obj.__module__ != module.__name__:
                continue
            if cls_name.startswith("_"):
                continue
            name = getattr(obj, "name", None)
            if not isinstance(name, str) or not name:
                continue
            if not (
                callable(getattr(obj, "enumerate", None))
                and callable(getattr(obj, "read", None))
            ):
                continue
            found.setdefault(name, mod_info.name)
    return found


def _fresh_registry_names() -> set[str]:
    reloaded = importlib.reload(adapters_module)
    return {a.name for a in reloaded.registered()}


def test_registry_matches_the_package_exactly():
    """Everything adapter-shaped in the package registers — nothing more,
    nothing less. This is the anti-'file exists but its data goes dark'
    guard: an adapter can no longer ship without registering."""
    walked = set(_independent_walk())
    assert walked, "package walk found no adapters — walk logic is broken"
    assert _fresh_registry_names() == walked


def test_every_adapter_module_contributes_an_adapter():
    """Each non-infrastructure module yields ≥1 registered adapter, so a
    dead adapter file can't sit in the package looking implemented."""
    walked = _independent_walk()
    contributing = set(walked.values())
    for mod_info in pkgutil.iter_modules(adapters_module.__path__):
        if mod_info.name.startswith("_") or mod_info.name in _INFRA_MODULES:
            continue
        assert mod_info.name in contributing, (
            f"adapters/{mod_info.name}.py contains no registering adapter class"
        )


def test_registry_has_no_gating():
    """No opt-in env flags: the module that builds the registry must not
    reference the retired STACKUNDERFLOW_BETA_* mechanism."""
    init_src = Path(adapters_module.__file__).read_text()
    assert "STACKUNDERFLOW_BETA_" not in init_src


def test_legacy_beta_env_vars_change_nothing(monkeypatch):
    """Setting the old opt-in vars must not alter the registered set."""
    baseline = _fresh_registry_names()
    for suffix in ("GROK", "QWEN", "GEMINI", "ANTIGRAVITY", "COPILOT"):
        monkeypatch.setenv(f"STACKUNDERFLOW_BETA_{suffix}", "1")
    assert _fresh_registry_names() == baseline
    for suffix in ("GROK", "QWEN", "GEMINI", "ANTIGRAVITY", "COPILOT"):
        monkeypatch.delenv(f"STACKUNDERFLOW_BETA_{suffix}")
    assert _fresh_registry_names() == baseline


def test_adapter_names_are_unique():
    reloaded = importlib.reload(adapters_module)
    names = [a.name for a in reloaded.registered()]
    assert len(names) == len(set(names))

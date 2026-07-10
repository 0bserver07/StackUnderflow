"""The claude slug→dir fallback belongs to claude ONLY, derived from its
adapter (honoring CLAUDE_CONFIG_DIR) — never invented for other providers."""

from __future__ import annotations

from stackunderflow.routes.projects import _dir_size_mb, _resolve_log_dir


def test_stored_path_always_wins():
    assert _resolve_log_dir("/data/proj", "-slug", "codex") == "/data/proj"
    assert _resolve_log_dir("/data/proj", "-slug", "claude") == "/data/proj"


def test_claude_without_path_derives_from_adapter(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "relocated"))
    got = _resolve_log_dir(None, "-my-slug", "claude")
    # Derivation honors CLAUDE_CONFIG_DIR — a hardcoded ~/.claude would not.
    assert got == str(tmp_path / "relocated" / "projects" / "-my-slug")


def test_non_claude_without_path_gets_no_invented_dir():
    for provider in ("codex", "cursor", "grok", "opencode"):
        assert _resolve_log_dir(None, "-slug", provider) == ""


def test_legacy_null_provider_keeps_claude_shim():
    """Pre-multi-provider rows have provider=NULL and are claude data."""
    assert _resolve_log_dir(None, "-slug", None).endswith("/projects/-slug")


def test_dir_size_of_unknown_dir_is_zero_never_cwd():
    assert _dir_size_mb("") == 0.0

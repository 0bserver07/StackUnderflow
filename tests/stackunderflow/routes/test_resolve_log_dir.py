"""The claude slug→dir fallback belongs to claude ONLY, derived from its
adapter (honoring CLAUDE_CONFIG_DIR) — never invented for other providers."""

from __future__ import annotations

import stackunderflow.adapters.claude as claude_module
from stackunderflow.adapters.claude import default_projects_root
from stackunderflow.routes.projects import (
    _compute_projects_payload,
    _dir_size_mb,
    _resolve_log_dir,
)
from stackunderflow.store import db, schema


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


# ── PROJ-1: the per-request projects-root hoist ──────────────────────────────
# The list endpoint resolves one log dir per project row (306 per request on a
# real store) and used to re-derive claude's projects root inside every call.
# It now derives it once and passes it down. The param must be a pure
# short-circuit: same answers with it as without, and the env is still what
# decides when it is absent (an lru_cache here would freeze the first value).


def test_passed_projects_root_matches_the_derived_one(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "relocated"))
    hoisted = default_projects_root()
    assert _resolve_log_dir(None, "-my-slug", "claude", projects_root=hoisted) == _resolve_log_dir(
        None, "-my-slug", "claude"
    )


def test_passed_projects_root_is_used_verbatim(monkeypatch, tmp_path):
    """Explicit root wins over the env — that's what makes the hoist a hoist."""
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "ignored"))
    got = _resolve_log_dir(None, "-my-slug", "claude", projects_root=tmp_path / "hoisted")
    assert got == str(tmp_path / "hoisted" / "-my-slug")


def test_hoist_does_not_freeze_the_env_across_requests(monkeypatch, tmp_path):
    """Re-deriving per request (not caching) is the point: relocate the config
    dir and the very next derivation must follow it."""
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "first"))
    first = _resolve_log_dir(None, "-slug", "claude", projects_root=default_projects_root())
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "second"))
    second = _resolve_log_dir(None, "-slug", "claude", projects_root=default_projects_root())
    assert first == str(tmp_path / "first" / "projects" / "-slug")
    assert second == str(tmp_path / "second" / "projects" / "-slug")


def test_hoisted_root_never_invents_a_dir_for_other_providers(monkeypatch, tmp_path):
    """The claude-only rule and "stored path wins" both survive the param."""
    root = tmp_path / "hoisted"
    for provider in ("codex", "cursor", "grok", "opencode"):
        assert _resolve_log_dir(None, "-slug", provider, projects_root=root) == ""
    assert _resolve_log_dir("/data/proj", "-slug", "claude", projects_root=root) == "/data/proj"
    assert _resolve_log_dir("/data/proj", "-slug", "codex", projects_root=root) == "/data/proj"


def test_list_payload_resolves_log_dirs_through_the_hoisted_root(monkeypatch, tmp_path):
    """End-to-end: the payload builder derives the root once and every row's
    ``log_path`` still honours ``CLAUDE_CONFIG_DIR``."""
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "relocated"))
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    for slug in ("-alpha", "-beta"):
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 0.0, 0.0)",
            (slug, slug),
        )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    derived = 0
    real_root = claude_module.default_projects_root

    def counting_root():
        nonlocal derived
        derived += 1
        return real_root()

    monkeypatch.setattr(claude_module, "default_projects_root", counting_root)
    payload = _compute_projects_payload(
        include_stats=False,
        sort_by="name",
        limit=None,
        offset=0,
        provider_filter=None,
    )

    log_paths = {p["dir_name"]: p["log_path"] for p in payload["projects"]}
    assert log_paths["-alpha"] == str(tmp_path / "relocated" / "projects" / "-alpha")
    assert log_paths["-beta"] == str(tmp_path / "relocated" / "projects" / "-beta")
    # Once per request, not once per project row.
    assert derived == 1

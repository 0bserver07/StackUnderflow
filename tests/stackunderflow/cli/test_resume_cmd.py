"""Tests for ``stackunderflow resume`` — per-agent session/resume ids by path.

Pins the four things the feature exists for: bidirectional SLUG-space path
matching (decoding slugs to paths is lossy — ``dev_dev`` folds to
``dev-dev`` — so the query path is encoded instead), per-provider grouping
with recency ordering, resume-command rendering from the data templates in
``adapters/capabilities.json`` (session-scope vs latest-scope vs none), and
the ``--json`` envelope.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# (provider, slug, [(session_id, last_ts, message_count), ...])
_WS = [
    ("claude", "-Users-t-my-ws", [
        ("cl-ws-new", "2026-07-01T10:00:00Z", 142),
    ]),
    ("claude", "-Users-t-my-ws-child", [
        ("cl-child-old", "2026-06-19T10:00:00Z", 601),
    ]),
    ("codex", "-Users-t-my-ws-child", [
        ("cx-child-new", "2026-07-08T10:00:00Z", 151),
        ("cx-child-old", "2026-06-26T10:00:00Z", 62),
    ]),
    ("grok", "-Users-t-my-ws-child", [
        ("gr-child", "2026-07-09T10:00:00Z", 96),
    ]),
    ("mystery", "-Users-t-my-ws-child", [
        ("my-child", "2026-05-24T10:00:00Z", 82),
    ]),
    # Home-directory catch-all — an ANCESTOR of every query under /Users/t.
    ("claude", "-Users-t", [
        ("cl-home", "2026-05-27T10:00:00Z", 40),
    ]),
    # Unrelated project — must never match.
    ("claude", "-Users-t-other-proj", [
        ("cl-other", "2026-07-09T10:00:00Z", 10),
    ]),
]


def _seed(store: Path) -> None:
    conn = db.connect(store)
    schema.apply(conn)
    for provider, slug, sessions in _WS:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, "
            "last_modified) VALUES (?, ?, ?, 0.0, 0.0)",
            (provider, slug, slug),
        )
        pid = int(cur.lastrowid or 0)
        for sid, last_ts, count in sessions:
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, "
                "last_ts, message_count) VALUES (?, ?, ?, ?, ?)",
                (pid, sid, last_ts, last_ts, count),
            )
    conn.commit()
    conn.close()


def _invoke(args, store: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store)
    return CliRunner().invoke(cli, args)


def _payload(args, store: Path, monkeypatch) -> dict:
    r = _invoke([*args, "--json"], store, monkeypatch)
    assert r.exit_code == 0, r.output
    return json.loads(r.output)


def _block(payload: dict, provider: str) -> dict:
    return next(p for p in payload["providers"] if p["provider"] == provider)


# ── matching ─────────────────────────────────────────────────────────────────


def test_workspace_query_matches_children_despite_underscores(tmp_path, monkeypatch):
    """/Users/t/my_ws (real underscore) matches slugs folded to my-ws —
    encoding the query is lossless where decoding slugs is not."""
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws"], store, monkeypatch)
    providers = {p["provider"] for p in payload["providers"]}
    assert {"claude", "codex", "grok", "mystery"} <= providers
    codex_ids = [s["session_id"] for s in _block(payload, "codex")["sessions"]]
    assert codex_ids == ["cx-child-new", "cx-child-old"]  # newest first
    # The unrelated sibling project never leaks in.
    all_ids = {
        s["session_id"]
        for p in payload["providers"]
        for s in p["sessions"]
    }
    assert "cl-other" not in all_ids


def test_standing_inside_child_finds_child_and_nearest_ancestor(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws/child"], store, monkeypatch)
    claude_ids = [s["session_id"] for s in _block(payload, "claude")["sessions"]]
    # Child project (exact) + my-ws (the DEEPEST ancestor) — but never the
    # home catch-all, which a nearer ancestor shadows.
    assert "cl-child-old" in claude_ids
    assert "cl-ws-new" in claude_ids
    assert "cl-home" not in claude_ids


def test_home_catchall_matches_only_when_it_is_the_nearest_project(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/somewhere/random"], store, monkeypatch)
    claude_ids = [s["session_id"] for s in _block(payload, "claude")["sessions"]]
    assert claude_ids == ["cl-home"]


def test_limit_per_provider(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(
        ["resume", "/Users/t/my_ws", "--limit-per-provider", "1"],
        store, monkeypatch,
    )
    assert [s["session_id"] for s in _block(payload, "codex")["sessions"]] == [
        "cx-child-new"
    ]


# ── resume-command rendering (templates are DATA in capabilities.json) ──────


def test_session_scope_templates_render_real_commands(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws"], store, monkeypatch)
    codex = _block(payload, "codex")
    assert codex["resume"]["scope"] == "session"
    assert codex["sessions"][0]["resume_command"] == "codex resume cx-child-new"
    claude = _block(payload, "claude")
    assert claude["sessions"][0]["resume_command"].startswith("claude --resume ")


def test_latest_scope_renders_no_per_session_command(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws"], store, monkeypatch)
    grok = _block(payload, "grok")
    assert grok["resume"]["scope"] == "latest"
    assert all(s["resume_command"] is None for s in grok["sessions"])
    r = _invoke(["resume", "/Users/t/my_ws"], store, monkeypatch)
    assert "latest-only" in r.output


def test_unknown_provider_lists_ids_without_inventing_commands(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws"], store, monkeypatch)
    mystery = _block(payload, "mystery")
    assert mystery["resume"] is None
    assert mystery["sessions"][0]["resume_command"] is None
    r = _invoke(["resume", "/Users/t/my_ws"], store, monkeypatch)
    assert "no resume command known" in r.output
    assert "my-child" in r.output


# ── envelope + failure modes ─────────────────────────────────────────────────


def test_json_envelope_shape(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws"], store, monkeypatch)
    assert payload["schema"] == "stackunderflow.resume/1"
    assert payload["path"] == "/Users/t/my_ws"
    names = [p["provider"] for p in payload["providers"]]
    assert names == sorted(names)
    sess = _block(payload, "codex")["sessions"][0]
    for key in ("session_id", "first_ts", "last_ts", "message_count",
                "project", "project_path", "resume_command"):
        assert key in sess


def test_no_matches_says_so(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    r = _invoke(["resume", "/Elsewhere/entirely"], store, monkeypatch)
    assert r.exit_code == 0, r.output
    assert "no recorded sessions under" in r.output


def test_missing_store_is_a_clean_error(tmp_path, monkeypatch):
    r = _invoke(["resume", "/Users/t/my_ws"], tmp_path / "nope.db", monkeypatch)
    assert r.exit_code != 0
    assert "store not found" in r.output


# ── --provider narrowing ─────────────────────────────────────────────────────


def test_provider_filter_reduces_to_one_agent(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws", "-p", "codex"], store, monkeypatch)
    assert [p["provider"] for p in payload["providers"]] == ["codex"]
    assert payload["provider_filter"] == ["codex"]


def test_provider_filter_is_case_insensitive_and_repeatable(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(
        ["resume", "/Users/t/my_ws", "-p", "CODEX", "-p", "grok"],
        store, monkeypatch,
    )
    assert {p["provider"] for p in payload["providers"]} == {"codex", "grok"}


def test_provider_filter_accepts_unambiguous_prefix(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(["resume", "/Users/t/my_ws", "-p", "gr"], store, monkeypatch)
    assert [p["provider"] for p in payload["providers"]] == ["grok"]


def test_provider_filter_unknown_errors_with_available_list(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    r = _invoke(["resume", "/Users/t/my_ws", "-p", "agy"], store, monkeypatch)
    assert r.exit_code != 0
    assert "providers with sessions here:" in r.output
    assert "codex" in r.output


def test_provider_filter_partial_match_notes_the_misses(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store)
    payload = _payload(
        ["resume", "/Users/t/my_ws", "-p", "codex", "-p", "nope"],
        store, monkeypatch,
    )
    assert [p["provider"] for p in payload["providers"]] == ["codex"]
    assert payload["unmatched_providers"] == ["nope"]
    r = _invoke(["resume", "/Users/t/my_ws", "-p", "codex", "-p", "nope"],
                store, monkeypatch)
    assert "no sessions here for: nope" in r.output

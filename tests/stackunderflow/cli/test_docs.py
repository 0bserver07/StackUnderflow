"""Tests for ``stackunderflow docs`` and the embedded-docs module.

Proves the docs are served from the installed package (string constants, no
repo checkout), fully offline (no network), and audience-filterable.
"""

from __future__ import annotations

import json

from click.testing import CliRunner

from stackunderflow import embedded_docs as ed
from stackunderflow.cli import cli


# ── embedded_docs module ──────────────────────────────────────────────────────


def test_topics_are_registered():
    topics = ed.topics()
    assert "overview" in topics and "memory" in topics and "support-matrix" in topics
    # No duplicate slugs.
    assert len(topics) == len(set(topics))


def test_audience_filter_includes_all_plus_the_asked_for_tier():
    agent = {d["slug"] for d in ed.list_docs(audience="agent")}
    user = {d["slug"] for d in ed.list_docs(audience="user")}
    # "memory" is agent-tagged; "quickstart" is user-tagged.
    assert "memory" in agent and "memory" not in user
    assert "quickstart" in user and "quickstart" not in agent
    # "all"-tagged pages show up under every audience.
    assert "overview" in agent and "overview" in user


def test_list_docs_rejects_bad_audience():
    import pytest

    with pytest.raises(ValueError):
        ed.list_docs(audience="nobody")


def test_get_doc_returns_body_and_none_for_unknown():
    assert ed.get_doc("does-not-exist") is None
    doc = ed.get_doc("memory")
    assert doc["slug"] == "memory"
    assert doc["body"].endswith("\n")
    # The memory page documents the JSON envelope contract.
    assert "stackunderflow.memory/1" in doc["body"]


def test_support_matrix_topic_renders_live():
    body = ed.get_doc("support-matrix")["body"]
    # Live-rendered from the adapter set: provider names + fidelity vocabulary.
    assert "claude" in body
    assert "reasoning" in body
    assert "codeium" in body


def test_docs_render_without_network(monkeypatch):
    """Rendering any page — including the live matrix — touches no socket."""
    import socket

    def _boom(*a, **k):  # pragma: no cover - only fires on a regression
        raise AssertionError("embedded docs must not open a network connection")

    monkeypatch.setattr(socket, "socket", _boom)
    monkeypatch.setattr(socket, "create_connection", _boom)
    for slug in ed.topics():
        assert ed.get_doc(slug)["body"].strip()


# ── docs CLI ──────────────────────────────────────────────────────────────────


def test_docs_list_text():
    r = CliRunner().invoke(cli, ["docs", "list"])
    assert r.exit_code == 0, r.output
    for slug in ed.topics():
        assert slug in r.output


def test_docs_list_json():
    r = CliRunner().invoke(cli, ["docs", "list", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert {d["slug"] for d in payload} == set(ed.topics())
    assert all({"slug", "title", "audience", "summary"} <= set(d) for d in payload)


def test_docs_list_audience_filter():
    r = CliRunner().invoke(cli, ["docs", "list", "--audience", "agent", "--json"])
    assert r.exit_code == 0, r.output
    slugs = {d["slug"] for d in json.loads(r.output)}
    assert "memory" in slugs and "quickstart" not in slugs


def test_docs_list_bad_audience_is_clean_error():
    r = CliRunner().invoke(cli, ["docs", "list", "--audience", "bogus"])
    assert r.exit_code != 0
    assert r.exception is None or isinstance(r.exception, SystemExit)


def test_docs_show_text():
    r = CliRunner().invoke(cli, ["docs", "show", "doctor"])
    assert r.exit_code == 0, r.output
    assert r.output.startswith("# doctor")


def test_docs_show_json():
    r = CliRunner().invoke(cli, ["docs", "show", "memory", "--json"])
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    assert doc["slug"] == "memory"
    assert "stackunderflow.memory/1" in doc["body"]


def test_docs_show_unknown_topic_lists_available():
    r = CliRunner().invoke(cli, ["docs", "show", "no-such-topic"])
    assert r.exit_code != 0
    assert "Available topics" in r.output
    assert "overview" in r.output

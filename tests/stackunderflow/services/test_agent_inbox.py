"""Agent inbox — delivery, once-only injection, and the never-disrupt invariant."""

from __future__ import annotations

import json

import pytest

from stackunderflow.services import agent_inbox


@pytest.fixture
def home(tmp_path, monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_HOME", str(tmp_path))
    return tmp_path


def test_deliver_then_list_roundtrip(home):
    agent_inbox.deliver_local("wave 5 gated", sender="tmos-hq", root=home)
    msgs = agent_inbox.list_messages(root=home)
    assert len(msgs) == 1
    assert msgs[0].sender == "tmos-hq"
    assert msgs[0].text == "wave 5 gated"


def test_injection_surfaces_each_message_exactly_once(home):
    agent_inbox.deliver_local("first", sender="tmos-hq", root=home)
    block = agent_inbox.render_for_injection(root=home)
    assert "[StackUnderflow inbox]" in block and "first" in block
    # second fire: nothing new -> empty, no repeat spam
    assert agent_inbox.render_for_injection(root=home) == ""
    # but the message is retained, readable with include_seen
    seen = agent_inbox.list_messages(include_seen=True, root=home)
    assert len(seen) == 1 and seen[0].path.name.endswith(".seen.json")


def test_injection_caps_batch_and_reports_overflow(home):
    for i in range(agent_inbox.MAX_INJECT + 2):
        agent_inbox.deliver_local(f"m{i}", sender="mac", root=home)
    block = agent_inbox.render_for_injection(root=home)
    assert "more: run `stackunderflow msg inbox`" in block
    # the un-injected tail is still unseen
    assert len(agent_inbox.list_messages(root=home)) == 2


def test_corrupt_file_never_breaks_the_channel(home):
    agent_inbox.deliver_local("good", sender="mac", root=home)
    bad = agent_inbox.inbox_dir(home) / "mac" / "zzzz-corrupt.json"
    bad.write_text("{not json")
    msgs = agent_inbox.list_messages(root=home)
    assert [m.text for m in msgs if m.text] == ["good"]
    # and the hook path stays silent-on-error by construction
    assert "good" in agent_inbox.render_for_injection(root=home)


def test_message_payload_shape(home):
    key, body = agent_inbox.message_payload("hello", sender="yk-m2")
    assert key.startswith("inbox/yk-m2/") and key.endswith(".json")
    raw = json.loads(body)
    assert raw["from"] == "yk-m2" and raw["text"] == "hello" and raw["id"]


def test_missing_inbox_dir_is_empty_not_error(home):
    assert agent_inbox.list_messages(root=home) == []
    assert agent_inbox.render_for_injection(root=home) == ""

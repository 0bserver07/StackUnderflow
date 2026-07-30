"""``GET /api/jsonl-content`` elides inline base64 media payloads.

Screenshot-heavy transcripts embed images as
``{"type": "image", "source": {"type": "base64", "media_type": "image/png",
"data": "<~1.5MB string>"}}`` — and the same block is duplicated under
``toolUseResult``. On the worst session in the real store those strings are
93.8% of a 117.4MiB response, and the Sessions tab never reads them: it
renders text / tool_use / tool_result-text only.

This suite locks the fix:

* the payload string is replaced by a ``<elided: … , N bytes>`` stub while the
  block's shape (``type``, ``media_type``) survives,
* ``raw_media=1`` is a working escape hatch that returns the bytes,
* lines with no media are byte-identical to their stored ``raw_json``, and
* the envelope (``total_lines``, role counts, metadata) is unchanged.
"""
from __future__ import annotations

import json

import pytest

from stackunderflow.routes.sessions import get_jsonl_content
from stackunderflow.store import db, schema

# A stand-in for a real screenshot payload: long enough that the stub is
# unambiguously smaller, short enough to keep the fixture cheap.
_BLOB = "iVBORw0KGgoAAAANSUhEUg" + ("A" * 4000)
_JPEG_BLOB = "/9j/4AAQSkZJRgABAQ" + ("B" * 2000)


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-jc"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts="2026-05-01T00:00:00Z"):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, ts, ts),
    )
    return int(cur.lastrowid)


def _insert_raw(conn, *, session_fk, seq, ts, role, raw, content=""):
    """Insert a message carrying an explicit ``raw_json`` payload."""
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, content_text, tools_json, raw_json, "
        "is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, NULL, 0, 0, ?, '[]', ?, 0, ?, NULL)",
        (session_fk, seq, ts, role, content, json.dumps(raw), f"u{session_fk}-{seq}"),
    )


def _image_message_raw(ts, uuid):
    """The real Claude shape: an image block inside a ``tool_result``, with the
    identical block duplicated under ``toolUseResult``."""
    blocks = [
        {"type": "text", "text": "### Ran Playwright code\n// Screenshot"},
        {
            "type": "image",
            "source": {"data": _BLOB, "media_type": "image/png", "type": "base64"},
        },
    ]
    return {
        "type": "user",
        "timestamp": ts,
        "uuid": uuid,
        "cwd": "/work/proj",
        "message": {
            "role": "user",
            "content": [
                {"tool_use_id": "toolu_1", "type": "tool_result", "content": blocks},
            ],
        },
        "toolUseResult": blocks,
    }


def _top_level_image_raw(ts, uuid):
    """A second shape: the image block sits directly in ``message.content``,
    with a jpeg payload — proves the walk is shape-agnostic, not path-matched."""
    return {
        "type": "user",
        "timestamp": ts,
        "uuid": uuid,
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": "look at this"},
                {
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/jpeg", "data": _JPEG_BLOB},
                },
            ],
        },
    }


def _plain_raw(ts, uuid, text):
    """A message with no media at all — the byte-identity control. Includes a
    ``data`` key that is NOT a media payload, so the walk must leave it be."""
    return {
        "type": "assistant",
        "timestamp": ts,
        "uuid": uuid,
        "cwd": "/work/proj",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": text}],
        },
        "toolUseResult": {"data": "not-a-media-payload", "rows": [1, 2, 3]},
    }


@pytest.fixture
def seeded(tmp_path, monkeypatch):
    """One session, four messages: two carrying base64 media in different
    shapes, two carrying none (one of which has an innocent ``data`` key)."""
    store_db = tmp_path / "jc.db"
    slug = "-jc"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sid = _insert_session(conn, project_id=pid, session_id="sess-img")

    raws = [
        ("user", _plain_raw("2026-05-01T00:00:00Z", "u0", "first prompt")),
        ("user", _image_message_raw("2026-05-01T00:01:00Z", "u1")),
        ("assistant", _plain_raw("2026-05-01T00:02:00Z", "u2", "an answer")),
        ("user", _top_level_image_raw("2026-05-01T00:03:00Z", "u3")),
    ]
    for seq, (role, raw) in enumerate(raws):
        _insert_raw(
            conn,
            session_fk=sid,
            seq=seq,
            ts=raw["timestamp"],
            role=role,
            raw=raw,
            content=f"c{seq}",
        )

    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return slug, [r for _, r in raws]


def _sources(line):
    """Every ``source`` dict anywhere under ``line``."""
    found = []
    stack = [(line, None)]
    while stack:
        cur, key = stack.pop()
        if isinstance(cur, dict):
            if key == "source":
                found.append(cur)
            for k, v in cur.items():
                if isinstance(v, (dict, list)):
                    stack.append((v, k))
        elif isinstance(cur, list):
            for v in cur:
                if isinstance(v, (dict, list)):
                    stack.append((v, key))
    return found


@pytest.mark.asyncio
async def test_base64_image_is_elided_with_siblings_intact(seeded):
    """The blob is gone; the stub names the media type and the original size;
    ``type``/``media_type`` survive so the block keeps its shape."""
    slug, _ = seeded
    resp = await get_jsonl_content(file="sess-img.jsonl", project=slug)
    body = json.loads(resp.body)

    # Both copies of the png block (message.content and toolUseResult) plus the
    # jpeg on the fourth line: three source dicts carry stubs.
    srcs = [s for line in body["lines"] for s in _sources(line)]
    assert len(srcs) == 3, srcs
    for src in srcs:
        assert src["type"] == "base64"
        assert src["media_type"] in ("image/png", "image/jpeg")
        assert src["data"] == f"<elided: {src['media_type']} base64, " + (
            f"{len(_BLOB)} bytes>" if src["media_type"] == "image/png"
            else f"{len(_JPEG_BLOB)} bytes>"
        )

    # Not a byte of the payload survives anywhere in the response.
    raw_body = resp.body.decode()
    assert _BLOB not in raw_body
    assert _JPEG_BLOB not in raw_body
    # …and the surrounding record is otherwise untouched.
    img_line = body["lines"][1]
    assert img_line["cwd"] == "/work/proj"
    assert img_line["message"]["content"][0]["type"] == "tool_result"
    assert img_line["message"]["content"][0]["content"][0]["text"].startswith("### Ran")
    assert img_line["toolUseResult"][1]["type"] == "image"


@pytest.mark.asyncio
async def test_raw_media_returns_the_full_blob(seeded):
    """The escape hatch ships the bytes, byte-identical to what was stored."""
    slug, raws = seeded
    resp = await get_jsonl_content(file="sess-img.jsonl", project=slug, raw_media=True)
    body = json.loads(resp.body)

    assert body["lines"] == raws  # every line, media included, exactly as stored
    srcs = [s for line in body["lines"] for s in _sources(line)]
    assert [s["data"] for s in srcs].count(_BLOB) == 2
    assert [s["data"] for s in srcs].count(_JPEG_BLOB) == 1
    # And it is dramatically bigger than the elided default — the whole point.
    elided = await get_jsonl_content(file="sess-img.jsonl", project=slug)
    assert len(resp.body) > len(elided.body) * 5


@pytest.mark.asyncio
async def test_media_free_lines_are_untouched(seeded):
    """Lines with no base64 payload round-trip identically — including an
    unrelated ``data`` key, which must NOT be mistaken for a media payload."""
    slug, raws = seeded
    resp = await get_jsonl_content(file="sess-img.jsonl", project=slug)
    lines = json.loads(resp.body)["lines"]

    for idx in (0, 2):
        assert lines[idx] == raws[idx]
        assert lines[idx]["toolUseResult"]["data"] == "not-a-media-payload"


@pytest.mark.asyncio
async def test_envelope_unchanged_by_elision(seeded):
    """Elision touches payload strings only — never the counts or metadata."""
    slug, raws = seeded
    elided = json.loads((await get_jsonl_content(file="sess-img.jsonl", project=slug)).body)
    raw = json.loads(
        (await get_jsonl_content(file="sess-img.jsonl", project=slug, raw_media=True)).body
    )

    assert elided["total_lines"] == len(raws) == 4
    assert elided["user_count"] == 3
    assert elided["assistant_count"] == 1
    assert elided["total_lines"] == raw["total_lines"]
    assert elided["user_count"] == raw["user_count"]
    assert elided["assistant_count"] == raw["assistant_count"]
    assert elided["metadata"] == raw["metadata"]
    assert elided["metadata"]["session_id"] == "sess-img"
    assert elided["metadata"]["cwd"] == "/work/proj"

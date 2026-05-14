"""Tests for ``stackunderflow.services.playback_fs`` — virtual-FS replay.

Locks the reconstruction contract:

* an empty session → empty ``files`` map;
* a single ``Write`` → file present with full content + ``complete=True``;
* a ``Read → Edit → Edit`` chain → both substitutions applied,
  ``complete=True``, no warnings;
* an ``Edit`` without a prior ``Read`` → ``new_string`` adopted,
  ``complete=False``, warning fired;
* an ``Edit`` whose ``old_string`` doesn't match → substitution skipped,
  warning fired, prior content preserved;
* ``MultiEdit`` with three edits → all applied in order;
* the ``at`` cutoff is exclusive of edits *after* the timestamp;
* the ``paths`` filter restricts the returned files;
* the Claude Code Read-result line-number prefix is stripped before
  Edits replay against it;
* an unknown ``session_id`` raises :class:`UnknownSession`;
* an unparseable ``at`` raises :class:`FsReconstructionError`;
* ``include_content=False`` strips ``content`` but keeps the metadata.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.services import playback_fs
from stackunderflow.store import db, schema


# ── seed helpers ────────────────────────────────────────────────────────────


def _seed_project(conn, *, slug: str = "demo") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, 1.0)",
        ("claude", slug, slug),
    )
    return int(cur.lastrowid)


def _seed_session(
    conn,
    *,
    project_id: int,
    session_id: str,
    first_ts: str = "2026-05-01T00:00:00Z",
    last_ts: str = "2026-05-01T23:59:00Z",
) -> int:
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, first_ts, last_ts),
    )
    return int(
        conn.execute(
            "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
            (project_id, session_id),
        ).fetchone()["id"]
    )


def _seed_msg(conn, *, session_fk: int, seq: int, role: str, raw: dict, ts: str) -> int:
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
        " cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, '', '[]', ?, 0, ?, NULL)",
        (session_fk, seq, ts, role, json.dumps(raw), f"u{session_fk}-{seq}"),
    )
    return int(
        conn.execute(
            "SELECT id FROM messages WHERE session_fk = ? AND seq = ?",
            (session_fk, seq),
        ).fetchone()["id"]
    )


def _assistant(*tool_calls: tuple[str, str, dict]) -> dict:
    """``tool_calls`` = ``(tool_use_id, tool_name, input_dict)``."""
    return {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": tuid, "name": name, "input": inp}
                for tuid, name, inp in tool_calls
            ],
        },
    }


def _result(*results: tuple[str, str]) -> dict:
    """``results`` = ``(tool_use_id, content_text)``."""
    return {
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": tuid, "content": text,
                 "is_error": False}
                for tuid, text in results
            ],
        },
    }


def _seed_pairs(
    conn,
    *,
    project_id: int,
    session_id: str,
    pairs: list[tuple[str, dict, str]],
    start_minute: int = 0,
) -> int:
    """Seed (tool_call, tool_result) pairs. Each ``pairs`` entry is
    ``(tool_name, tool_input, result_text)``. Assistant @ minute N,
    result @ minute N+1, next assistant @ minute N+2."""
    sfk = _seed_session(conn, project_id=project_id, session_id=session_id)
    seq = 0
    minute = start_minute
    for i, (tname, tinp, restxt) in enumerate(pairs):
        tuid = f"tu-{session_id}-{i}"
        ts_a = f"2026-05-01T00:{minute:02d}:00Z"
        minute += 1
        ts_r = f"2026-05-01T00:{minute:02d}:00Z"
        minute += 1
        _seed_msg(conn, session_fk=sfk, seq=seq, role="assistant",
                  raw=_assistant((tuid, tname, tinp)), ts=ts_a)
        seq += 1
        _seed_msg(conn, session_fk=sfk, seq=seq, role="user",
                  raw=_result((tuid, restxt)), ts=ts_r)
        seq += 1
    conn.commit()
    return sfk


@pytest.fixture()
def conn(tmp_path):
    store_db = tmp_path / "store.db"
    c = db.connect(store_db)
    schema.apply(c)
    yield c
    c.close()


_LATE = "2026-05-02T00:00:00Z"  # well after all seeded events


# ── happy paths ─────────────────────────────────────────────────────────────


def test_empty_session_returns_empty_files(conn):
    pid = _seed_project(conn)
    _seed_session(conn, project_id=pid, session_id="empty")
    snap = playback_fs.reconstruct_fs_at(conn, "empty", at=_LATE)
    assert snap["session_id"] == "empty"
    assert snap["snapshot_ts"] == _LATE
    assert snap["files"] == {}
    assert snap["warnings"] == []


def test_single_write_yields_full_content_complete_true(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="w1", pairs=[
        ("Write", {"file_path": "src/a.py", "content": "print('hello')\n"}, "File created"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "w1", at=_LATE)
    assert set(snap["files"]) == {"src/a.py"}
    f = snap["files"]["src/a.py"]
    assert f["content"] == "print('hello')\n"
    assert f["byte_count"] == len("print('hello')\n".encode("utf-8"))
    assert f["reconstruction_complete"] is True
    assert f["operations_applied"] == ["Write#0"]
    assert snap["warnings"] == []


def test_read_then_two_edits_applies_in_order(conn):
    pid = _seed_project(conn)
    initial = "def foo():\n    return 1\n"
    _seed_pairs(conn, project_id=pid, session_id="re", pairs=[
        ("Read", {"file_path": "src/x.py"}, initial),
        ("Edit", {"file_path": "src/x.py", "old_string": "return 1",
                  "new_string": "return 2"}, "ok"),
        ("Edit", {"file_path": "src/x.py", "old_string": "def foo",
                  "new_string": "def bar"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "re", at=_LATE)
    f = snap["files"]["src/x.py"]
    assert f["content"] == "def bar():\n    return 2\n"
    assert f["reconstruction_complete"] is True
    assert f["operations_applied"] == ["Read#0", "Edit#0", "Edit#1"]
    assert snap["warnings"] == []


def test_edit_without_prior_read_marks_partial_and_warns(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="pe", pairs=[
        ("Edit", {"file_path": "src/y.py", "old_string": "old line",
                  "new_string": "new line"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "pe", at=_LATE)
    f = snap["files"]["src/y.py"]
    assert f["content"] == "new line"
    assert f["reconstruction_complete"] is False
    assert f["operations_applied"] == ["Edit#0"]
    assert any("no initial Read or Write" in w for w in snap["warnings"])


def test_edit_with_unmatched_old_string_is_skipped_with_warning(conn):
    pid = _seed_project(conn)
    initial = "alpha\nbeta\ngamma\n"
    _seed_pairs(conn, project_id=pid, session_id="mm", pairs=[
        ("Read", {"file_path": "f.txt"}, initial),
        ("Edit", {"file_path": "f.txt", "old_string": "delta",
                  "new_string": "epsilon"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "mm", at=_LATE)
    f = snap["files"]["f.txt"]
    # Content unchanged, op recorded, complete still True.
    assert f["content"] == initial
    assert f["reconstruction_complete"] is True
    assert f["operations_applied"] == ["Read#0", "Edit#0"]
    assert any("old_string did not match" in w for w in snap["warnings"])


def test_multi_edit_with_three_edits_all_applied_in_order(conn):
    pid = _seed_project(conn)
    initial = "A\nB\nC\n"
    _seed_pairs(conn, project_id=pid, session_id="mu", pairs=[
        ("Read", {"file_path": "abc.txt"}, initial),
        ("MultiEdit", {
            "file_path": "abc.txt",
            "edits": [
                {"old_string": "A", "new_string": "1"},
                {"old_string": "B", "new_string": "2"},
                {"old_string": "C", "new_string": "3"},
            ],
        }, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "mu", at=_LATE)
    f = snap["files"]["abc.txt"]
    assert f["content"] == "1\n2\n3\n"
    assert f["reconstruction_complete"] is True
    assert f["operations_applied"] == ["Read#0", "MultiEdit#0"]
    assert snap["warnings"] == []


def test_multi_edit_per_edit_skip_warns_per_edit(conn):
    """A single MultiEdit with one bad sub-edit warns for that one only —
    other sub-edits still apply."""
    pid = _seed_project(conn)
    initial = "alpha\nbeta\n"
    _seed_pairs(conn, project_id=pid, session_id="mu2", pairs=[
        ("Read", {"file_path": "f.txt"}, initial),
        ("MultiEdit", {
            "file_path": "f.txt",
            "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "nope", "new_string": "x"},      # miss
                {"old_string": "beta", "new_string": "BETA"},
            ],
        }, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "mu2", at=_LATE)
    f = snap["files"]["f.txt"]
    assert f["content"] == "ALPHA\nBETA\n"
    # Exactly one warning (the missed sub-edit).
    misses = [w for w in snap["warnings"] if "old_string did not match" in w]
    assert len(misses) == 1


# ── time cutoff ─────────────────────────────────────────────────────────────


def test_at_cutoff_ignores_edits_after_timestamp(conn):
    pid = _seed_project(conn)
    # Read @ 00:00, Edit#0 @ 00:02, Edit#1 @ 00:04
    _seed_pairs(conn, project_id=pid, session_id="ct", pairs=[
        ("Read", {"file_path": "g.py"}, "x = 1\n"),
        ("Edit", {"file_path": "g.py", "old_string": "x = 1",
                  "new_string": "x = 2"}, "ok"),
        ("Edit", {"file_path": "g.py", "old_string": "x = 2",
                  "new_string": "x = 3"}, "ok"),
    ])
    # Cutoff at 00:03 — captures Read and first Edit, but NOT second.
    snap = playback_fs.reconstruct_fs_at(conn, "ct", at="2026-05-01T00:03:00Z")
    f = snap["files"]["g.py"]
    assert f["content"] == "x = 2\n"
    assert f["operations_applied"] == ["Read#0", "Edit#0"]
    assert f["last_modified_ts"] == "2026-05-01T00:02:00Z"


def test_at_cutoff_before_any_event_returns_empty(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="cb", pairs=[
        ("Write", {"file_path": "f.py", "content": "x"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "cb", at="2025-01-01T00:00:00Z")
    assert snap["files"] == {}


# ── paths filter ────────────────────────────────────────────────────────────


def test_paths_filter_restricts_returned_files(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="pf", pairs=[
        ("Write", {"file_path": "a.py", "content": "aaa"}, "ok"),
        ("Write", {"file_path": "b.py", "content": "bbb"}, "ok"),
        ("Write", {"file_path": "c.py", "content": "ccc"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(
        conn, "pf", at=_LATE, paths=["a.py", "c.py"],
    )
    assert set(snap["files"]) == {"a.py", "c.py"}
    assert snap["files"]["a.py"]["content"] == "aaa"
    assert snap["files"]["c.py"]["content"] == "ccc"


def test_paths_filter_with_no_matches_returns_empty(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="pn", pairs=[
        ("Write", {"file_path": "a.py", "content": "aaa"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(
        conn, "pn", at=_LATE, paths=["other.py"],
    )
    assert snap["files"] == {}


# ── Read line-number stripping ──────────────────────────────────────────────


def test_read_strips_cat_n_line_numbers_so_edits_match_raw(conn):
    """The Read tool returns ``     N\\tcontent``-formatted text. The
    actual file bytes don't carry that prefix, so the Edit tool replaces
    against the raw text. Verify the reconstructor strips before Edit."""
    pid = _seed_project(conn)
    # ``cat -n``-style: <spaces>1<tab>line1\n<spaces>2<tab>line2\n
    numbered = "     1\timport os\n     2\tprint(os.getcwd())\n"
    _seed_pairs(conn, project_id=pid, session_id="cn", pairs=[
        ("Read", {"file_path": "h.py"}, numbered),
        ("Edit", {"file_path": "h.py", "old_string": "import os",
                  "new_string": "import sys"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "cn", at=_LATE)
    f = snap["files"]["h.py"]
    # After stripping the numbering, the Edit substitution finds and
    # replaces ``import os`` cleanly — no warning.
    assert "import sys" in f["content"]
    assert "     1\t" not in f["content"]  # numbering gone
    assert f["reconstruction_complete"] is True
    assert snap["warnings"] == []


# ── error paths ─────────────────────────────────────────────────────────────


def test_unknown_session_raises(conn):
    with pytest.raises(playback_fs.UnknownSession):
        playback_fs.reconstruct_fs_at(conn, "does-not-exist", at=_LATE)


def test_unparseable_at_raises(conn):
    pid = _seed_project(conn)
    _seed_session(conn, project_id=pid, session_id="ok")
    with pytest.raises(playback_fs.FsReconstructionError):
        playback_fs.reconstruct_fs_at(conn, "ok", at="not-a-timestamp")


# ── include_content toggle ──────────────────────────────────────────────────


def test_include_content_false_omits_content_but_keeps_metadata(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="mc", pairs=[
        ("Write", {"file_path": "k.py", "content": "x" * 100}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(
        conn, "mc", at=_LATE, include_content=False,
    )
    f = snap["files"]["k.py"]
    assert "content" not in f
    assert f["byte_count"] == 100
    assert f["operations_applied"] == ["Write#0"]
    assert f["reconstruction_complete"] is True


# ── notebook edit ───────────────────────────────────────────────────────────


def test_notebook_edit_accumulates_cells_as_partial(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="nb", pairs=[
        ("NotebookEdit", {
            "notebook_path": "n.ipynb",
            "cell_id": "c1",
            "new_source": "print('a')",
        }, "ok"),
        ("NotebookEdit", {
            "notebook_path": "n.ipynb",
            "cell_id": "c2",
            "new_source": "print('b')",
        }, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "nb", at=_LATE)
    f = snap["files"]["n.ipynb"]
    # Two cells captured; notebook reconstruction is never marked complete.
    cells = json.loads(f["content"])
    assert cells == {"c1": "print('a')", "c2": "print('b')"}
    assert f["reconstruction_complete"] is False
    assert f["operations_applied"] == ["NotebookEdit#0", "NotebookEdit#1"]


# ── write replaces prior content ────────────────────────────────────────────


def test_write_after_edit_resets_content_to_write_payload(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="we", pairs=[
        ("Read", {"file_path": "z.py"}, "a\nb\nc\n"),
        ("Edit", {"file_path": "z.py", "old_string": "a", "new_string": "AA"}, "ok"),
        ("Write", {"file_path": "z.py", "content": "fresh\n"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "we", at=_LATE)
    f = snap["files"]["z.py"]
    # Write blew away the edited state.
    assert f["content"] == "fresh\n"
    assert f["operations_applied"] == ["Read#0", "Edit#0", "Write#0"]
    assert f["reconstruction_complete"] is True


# ── replace_all flag ────────────────────────────────────────────────────────


def test_edit_replace_all_replaces_every_occurrence(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="ra", pairs=[
        ("Read", {"file_path": "r.py"}, "x = 1\ny = 1\nz = 1\n"),
        ("Edit", {"file_path": "r.py", "old_string": "1",
                  "new_string": "2", "replace_all": True}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "ra", at=_LATE)
    f = snap["files"]["r.py"]
    assert f["content"] == "x = 2\ny = 2\nz = 2\n"


def test_edit_default_replaces_only_first(conn):
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="r1", pairs=[
        ("Read", {"file_path": "r.py"}, "x = 1\ny = 1\n"),
        ("Edit", {"file_path": "r.py", "old_string": "1",
                  "new_string": "2"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(conn, "r1", at=_LATE)
    f = snap["files"]["r.py"]
    assert f["content"] == "x = 2\ny = 1\n"


# ── datetime input ──────────────────────────────────────────────────────────


def test_at_accepts_datetime_object(conn):
    from datetime import UTC, datetime
    pid = _seed_project(conn)
    _seed_pairs(conn, project_id=pid, session_id="dt", pairs=[
        ("Write", {"file_path": "a.py", "content": "ok"}, "ok"),
    ])
    snap = playback_fs.reconstruct_fs_at(
        conn, "dt", at=datetime(2026, 5, 2, tzinfo=UTC),
    )
    assert "a.py" in snap["files"]

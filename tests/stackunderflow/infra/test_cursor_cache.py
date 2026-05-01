"""Unit tests for the Cursor fingerprint cache.

Covers fingerprint computation, hit/miss based on ``(mtime, size)`` match,
silent fallback on corrupt JSON or schema-version mismatch, and the
``clear_cache()`` helper used by ``stackunderflow clear-cache``.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record
from stackunderflow.infra import cursor_cache


def _make_record(seq: int = 1, *, session_id: str = "s1") -> Record:
    """Build a minimally-populated Record for round-trip testing."""
    return Record(
        provider="cursor",
        session_id=session_id,
        seq=seq,
        timestamp="2026-04-30T00:00:00+00:00",
        role="user",
        model="claude-sonnet-4-6",
        input_tokens=10,
        output_tokens=20,
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text="hello",
        tools=("bash", "read"),
        cwd=None,
        is_sidechain=False,
        uuid=f"{session_id}:{seq}",
        parent_uuid=None,
        raw={"k": "v"},
    )


@pytest.fixture()
def db_file(tmp_path: Path) -> Path:
    """A non-empty stand-in for state.vscdb. Cache only cares about stat."""
    p = tmp_path / "state.vscdb"
    p.write_bytes(b"x" * 4096)
    return p


@pytest.fixture()
def cache_path(tmp_path: Path) -> Path:
    return tmp_path / "cursor-results.json"


# ── fingerprint() ────────────────────────────────────────────────────


def test_fingerprint_returns_path_mtime_size(db_file: Path) -> None:
    abs_path, mtime, size = cursor_cache.fingerprint(db_file)
    assert abs_path == str(db_file.resolve())
    assert mtime > 0
    assert size == 4096


def test_fingerprint_changes_when_size_changes(db_file: Path) -> None:
    _, _, size1 = cursor_cache.fingerprint(db_file)
    db_file.write_bytes(b"x" * 5000)
    _, _, size2 = cursor_cache.fingerprint(db_file)
    assert size1 != size2


def test_fingerprint_handles_missing_file(tmp_path: Path) -> None:
    """Missing file returns zeroed mtime/size; caller decides what to do."""
    missing = tmp_path / "nope.vscdb"
    abs_path, mtime, size = cursor_cache.fingerprint(missing)
    assert abs_path == str(missing.resolve())
    assert mtime == 0.0
    assert size == 0


# ── load_cached / save_cached round-trip ─────────────────────────────


def test_save_and_load_round_trips_records(db_file: Path, cache_path: Path) -> None:
    records = [_make_record(seq=1), _make_record(seq=2)]
    cursor_cache.save_cached(db_file, records, cache_path=cache_path)

    loaded = cursor_cache.load_cached(db_file, cache_path=cache_path)
    assert loaded is not None
    assert len(loaded) == 2
    assert loaded[0].seq == 1
    assert loaded[1].seq == 2
    # tools must round-trip as a tuple, not a list
    assert isinstance(loaded[0].tools, tuple)
    assert loaded[0].tools == ("bash", "read")


def test_load_cached_returns_none_when_no_file(db_file: Path, cache_path: Path) -> None:
    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_load_cached_misses_when_size_changes(db_file: Path, cache_path: Path) -> None:
    cursor_cache.save_cached(db_file, [_make_record()], cache_path=cache_path)

    # Change the file size — fingerprint no longer matches.
    db_file.write_bytes(b"x" * 8192)

    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_load_cached_misses_when_mtime_changes(db_file: Path, cache_path: Path) -> None:
    cursor_cache.save_cached(db_file, [_make_record()], cache_path=cache_path)

    # Bump the mtime without changing size.
    new_mtime = db_file.stat().st_mtime + 100.0
    os.utime(db_file, (new_mtime, new_mtime))

    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_load_cached_misses_for_unknown_db_path(db_file: Path, cache_path: Path, tmp_path: Path) -> None:
    cursor_cache.save_cached(db_file, [_make_record()], cache_path=cache_path)

    other = tmp_path / "other.vscdb"
    other.write_bytes(b"y" * 4096)
    assert cursor_cache.load_cached(other, cache_path=cache_path) is None


# ── corruption / schema fallback ─────────────────────────────────────


def test_corrupt_json_falls_back_silently(db_file: Path, cache_path: Path) -> None:
    cache_path.write_text("{not valid json", encoding="utf-8")
    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_wrong_schema_version_falls_back_silently(db_file: Path, cache_path: Path) -> None:
    cache_path.write_text(
        json.dumps({"version": 999, "entries": {}}),
        encoding="utf-8",
    )
    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_missing_entries_block_falls_back_silently(db_file: Path, cache_path: Path) -> None:
    cache_path.write_text(
        json.dumps({"version": 1}),
        encoding="utf-8",
    )
    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_malformed_record_payload_falls_back(db_file: Path, cache_path: Path) -> None:
    """A record dict missing required keys must be treated as a miss."""
    abs_path, mtime, size = cursor_cache.fingerprint(db_file)
    cache_path.write_text(
        json.dumps(
            {
                "version": 1,
                "entries": {
                    abs_path: {
                        "fingerprint": {"mtime": mtime, "size": size},
                        "records": [{"this": "is", "wrong": "shape"}],
                    }
                },
            }
        ),
        encoding="utf-8",
    )
    assert cursor_cache.load_cached(db_file, cache_path=cache_path) is None


def test_save_cached_overwrites_previous_entry(db_file: Path, cache_path: Path) -> None:
    cursor_cache.save_cached(db_file, [_make_record(seq=1)], cache_path=cache_path)
    cursor_cache.save_cached(db_file, [_make_record(seq=2), _make_record(seq=3)], cache_path=cache_path)

    loaded = cursor_cache.load_cached(db_file, cache_path=cache_path)
    assert loaded is not None
    assert [r.seq for r in loaded] == [2, 3]


def test_save_cached_preserves_other_db_entries(tmp_path: Path, cache_path: Path) -> None:
    """Saving for one DB must not clobber a different DB's cached entry."""
    db_a = tmp_path / "a.vscdb"
    db_a.write_bytes(b"a" * 1024)
    db_b = tmp_path / "b.vscdb"
    db_b.write_bytes(b"b" * 2048)

    cursor_cache.save_cached(db_a, [_make_record(seq=1)], cache_path=cache_path)
    cursor_cache.save_cached(db_b, [_make_record(seq=2)], cache_path=cache_path)

    loaded_a = cursor_cache.load_cached(db_a, cache_path=cache_path)
    loaded_b = cursor_cache.load_cached(db_b, cache_path=cache_path)
    assert loaded_a is not None and len(loaded_a) == 1 and loaded_a[0].seq == 1
    assert loaded_b is not None and len(loaded_b) == 1 and loaded_b[0].seq == 2


def test_save_cached_with_missing_db_is_noop(tmp_path: Path, cache_path: Path) -> None:
    """If the DB file vanished before save, don't write a bogus entry."""
    missing = tmp_path / "gone.vscdb"
    cursor_cache.save_cached(missing, [_make_record()], cache_path=cache_path)
    # Either the file was never created, or it has no entry for `missing`.
    if cache_path.exists():
        data = json.loads(cache_path.read_text())
        assert str(missing.resolve()) not in data.get("entries", {})


# ── clear_cache() ─────────────────────────────────────────────────────


def test_clear_cache_removes_existing_file(db_file: Path, cache_path: Path) -> None:
    cursor_cache.save_cached(db_file, [_make_record()], cache_path=cache_path)
    assert cache_path.exists()

    removed = cursor_cache.clear_cache(cache_path=cache_path)
    assert removed is True
    assert not cache_path.exists()


def test_clear_cache_returns_false_when_no_file(cache_path: Path) -> None:
    """``clear_cache`` on a never-warmed cache reports nothing was removed."""
    removed = cursor_cache.clear_cache(cache_path=cache_path)
    assert removed is False

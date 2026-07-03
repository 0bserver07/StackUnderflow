"""Unit tests for the deterministic content-hash id helper (spec #16)."""

from __future__ import annotations

from stackunderflow.adapters.base import content_hash_id


def test_identical_content_yields_identical_id():
    a = content_hash_id("custom", "src", "sess", 0, "message", "user", "hi")
    b = content_hash_id("custom", "src", "sess", 0, "message", "user", "hi")
    assert a == b


def test_different_content_yields_different_id():
    a = content_hash_id("custom", "src", "sess", 0, "user", "hi")
    b = content_hash_id("custom", "src", "sess", 0, "user", "ho")
    assert a != b


def test_boundary_is_unambiguous():
    # ("a", "bc") must not hash the same as ("ab", "c").
    assert content_hash_id("a", "bc") != content_hash_id("ab", "c")


def test_none_distinct_from_empty_string():
    assert content_hash_id(None) != content_hash_id("")


def test_arity_is_bound():
    # A trailing None must not alias a shorter argument list.
    assert content_hash_id("a") != content_hash_id("a", None)
    assert content_hash_id("a", "b") != content_hash_id("a", "b", None)


def test_order_sensitive():
    assert content_hash_id("a", "b") != content_hash_id("b", "a")


def test_prefix_is_prepended_verbatim():
    out = content_hash_id("x", prefix="c-")
    assert out.startswith("c-")
    assert content_hash_id("x") == out[len("c-"):]


def test_length_truncates_digest():
    short = content_hash_id("x", length=8)
    assert len(short) == 8
    full = content_hash_id("x", length=32)
    assert full.startswith(short)


def test_length_never_zero():
    # Defensive: a nonsensical length still returns at least one hex char.
    assert len(content_hash_id("x", length=0)) >= 1


def test_int_and_str_parts_are_canonical():
    # A caller passing an int seq and its str form should collapse to the same
    # id (str(0) == "0"), which keeps the mapping stable regardless of how the
    # caller typed the scalar.
    assert content_hash_id("s", 0) == content_hash_id("s", "0")


def test_hex_output():
    out = content_hash_id("anything", "here")
    assert all(c in "0123456789abcdef" for c in out)

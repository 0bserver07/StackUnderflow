"""ObjectStore fake + URL parsing — dependency-free (no boto3 needed)."""

from __future__ import annotations

import pytest

from stackunderflow.sync import bucket


def test_put_get_roundtrip():
    store = bucket.InMemoryObjectStore()
    store.put("a/b.age", b"hello")
    assert store.get("a/b.age") == b"hello"
    assert "a/b.age" in store
    assert len(store) == 1


def test_get_missing_raises_object_not_found():
    store = bucket.InMemoryObjectStore()
    with pytest.raises(bucket.ObjectNotFound):
        store.get("nope")


def test_list_filters_by_prefix_and_sorts():
    store = bucket.InMemoryObjectStore()
    store.put("p/1", b"1")
    store.put("p/3", b"3")
    store.put("p/2", b"2")
    store.put("other/x", b"x")
    assert store.list("p/") == ["p/1", "p/2", "p/3"]
    assert store.list("other/") == ["other/x"]
    assert store.list("zzz") == []


def test_delete_is_idempotent():
    store = bucket.InMemoryObjectStore()
    store.put("k", b"v")
    store.delete("k")
    store.delete("k")  # no error on a second delete
    assert "k" not in store


def test_call_counts_track_operations():
    store = bucket.InMemoryObjectStore()
    store.put("k", b"v")
    store.get("k")
    store.list("k")
    store.delete("k")
    assert store.put_calls == 1
    assert store.get_calls == 1
    assert store.list_calls == 1
    assert store.delete_calls == 1


def test_in_memory_satisfies_protocol():
    assert isinstance(bucket.InMemoryObjectStore(), bucket.ObjectStore)


@pytest.mark.parametrize(
    ("url", "expected"),
    [
        ("s3://my-bucket", ("my-bucket", "")),
        ("s3://my-bucket/", ("my-bucket", "")),
        ("s3://my-bucket/some/prefix", ("my-bucket", "some/prefix")),
        ("s3://my-bucket/some/prefix/", ("my-bucket", "some/prefix")),
    ],
)
def test_parse_bucket_url(url, expected):
    assert bucket.parse_bucket_url(url) == expected


@pytest.mark.parametrize("bad", ["my-bucket", "https://x", "s3://"])
def test_parse_bucket_url_rejects_bad(bad):
    with pytest.raises(ValueError):
        bucket.parse_bucket_url(bad)

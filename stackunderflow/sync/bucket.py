"""The narrow ``ObjectStore`` interface + a boto3 impl and an in-memory fake.

The sync logic depends only on ``put/get/list/delete`` so S3 quirks stay out of
the protocol and the test suite is hermetic (it runs against
:class:`InMemoryObjectStore`, never a real bucket). ``boto3`` is imported on
demand — a core install never pulls it.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable

from .keys import SyncDependencyError


class ObjectNotFound(KeyError):
    """Raised by :meth:`ObjectStore.get` when *key* is absent."""


@runtime_checkable
class ObjectStore(Protocol):
    """A minimal object store: bytes in, bytes out, list by prefix, delete."""

    def put(self, key: str, data: bytes) -> None: ...

    def get(self, key: str) -> bytes: ...

    def list(self, prefix: str) -> list[str]: ...  # noqa: A003 - part of the public interface name

    def delete(self, key: str) -> None: ...


class InMemoryObjectStore:
    """A dict-backed :class:`ObjectStore` for tests. Counts calls for assertions."""

    def __init__(self) -> None:
        self._data: dict[str, bytes] = {}
        self.put_calls = 0
        self.get_calls = 0
        self.list_calls = 0
        self.delete_calls = 0

    def put(self, key: str, data: bytes) -> None:
        self.put_calls += 1
        self._data[key] = bytes(data)

    def get(self, key: str) -> bytes:
        self.get_calls += 1
        try:
            return self._data[key]
        except KeyError as exc:
            raise ObjectNotFound(key) from exc

    def list(self, prefix: str) -> list[str]:  # noqa: A003 - part of the public interface name
        self.list_calls += 1
        return sorted(k for k in self._data if k.startswith(prefix))

    def delete(self, key: str) -> None:
        self.delete_calls += 1
        self._data.pop(key, None)

    # Test convenience — not part of the ObjectStore protocol.
    def __contains__(self, key: str) -> bool:
        return key in self._data

    def __len__(self) -> int:
        return len(self._data)


def parse_bucket_url(url: str) -> tuple[str, str]:
    """Split an ``s3://bucket[/prefix]`` URL into ``(bucket, key_prefix)``.

    *key_prefix* is ``""`` when the URL names only a bucket.
    """
    if not url.startswith("s3://"):
        raise ValueError(f"bucket URL must start with s3:// — got {url!r}")
    rest = url[len("s3://"):]
    parts = rest.split("/", 1)
    bucket = parts[0]
    if not bucket:
        raise ValueError(f"bucket URL has no bucket name — got {url!r}")
    prefix = parts[1].strip("/") if len(parts) > 1 else ""
    return bucket, prefix


class S3ObjectStore:
    """A ``boto3``-backed :class:`ObjectStore` (AWS S3, R2, B2, MinIO, …).

    Credentials come from the user's own standard AWS chain (``~/.aws``,
    ``AWS_*`` env) or the boto3 *client* passed in — never a StackUnderflow-issued
    key, and never persisted to ``store.db`` / ``config.json``.
    """

    def __init__(
        self,
        bucket: str,
        *,
        key_prefix: str = "",
        endpoint_url: str | None = None,
        client=None,
    ) -> None:
        self._bucket = bucket
        self._prefix = key_prefix.strip("/")
        if client is not None:
            self._client = client
        else:
            try:
                import boto3
            except ImportError as exc:  # pragma: no cover - exercised via CLI hint path
                raise SyncDependencyError(
                    "the 'boto3' package is required to talk to a bucket; "
                    "install with: pip install 'stackunderflow[sync]'"
                ) from exc
            self._client = boto3.client("s3", endpoint_url=endpoint_url)

    def _full(self, key: str) -> str:
        return f"{self._prefix}/{key}" if self._prefix else key

    def put(self, key: str, data: bytes) -> None:
        self._client.put_object(Bucket=self._bucket, Key=self._full(key), Body=data)

    def get(self, key: str) -> bytes:
        try:
            resp = self._client.get_object(Bucket=self._bucket, Key=self._full(key))
        except Exception as exc:  # botocore ClientError (NoSuchKey) et al.
            raise ObjectNotFound(key) from exc
        return resp["Body"].read()

    def list(self, prefix: str) -> list[str]:  # noqa: A003 - part of the public interface name
        full_prefix = self._full(prefix)
        strip = len(self._prefix) + 1 if self._prefix else 0
        keys: list[str] = []
        paginator = self._client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=self._bucket, Prefix=full_prefix):
            for obj in page.get("Contents", []):
                keys.append(obj["Key"][strip:] if strip else obj["Key"])
        return sorted(keys)

    def delete(self, key: str) -> None:
        self._client.delete_object(Bucket=self._bucket, Key=self._full(key))


def s3_store_from_url(bucket_url: str, endpoint_url: str | None = None) -> S3ObjectStore:
    """Build an :class:`S3ObjectStore` from an ``s3://…`` URL (imports boto3)."""
    bucket, prefix = parse_bucket_url(bucket_url)
    return S3ObjectStore(bucket, key_prefix=prefix, endpoint_url=endpoint_url)


SUPPORTED_SCHEMES = ("s3", "ssh")


def scheme_of(url: str) -> str:
    """The transport scheme of a sync destination URL (``""`` when absent)."""
    return url.split("://", 1)[0].lower() if "://" in url else ""


def requires_boto3(url: str) -> bool:
    """Whether *url* needs the optional ``boto3`` dependency to be usable.

    ``ssh://`` destinations go through the system ``ssh`` binary, so the
    ``[sync]`` extra's bucket dependency is not needed for them. An unknown
    scheme answers ``True`` so the caller still gets the install hint rather
    than a bare import error.
    """
    return scheme_of(url) != "ssh"


def store_from_url(url: str, endpoint_url: str | None = None):
    """Build the :class:`ObjectStore` for *url*, dispatching on its scheme.

    ``s3://`` → :class:`S3ObjectStore` (any S3-compatible endpoint).
    ``ssh://`` → :class:`~stackunderflow.sync.ssh_store.SSHObjectStore`, for
    syncing between machines you own without a bucket.

    The shard payload is identical either way: ``runner`` encrypts before the
    store sees it, so the transport never handles plaintext.
    """
    scheme = scheme_of(url)
    if scheme == "ssh":
        from .ssh_store import ssh_store_from_url

        return ssh_store_from_url(url)
    if scheme == "s3":
        return s3_store_from_url(url, endpoint_url)
    raise ValueError(
        f"unsupported sync destination {url!r} — expected one of: "
        + ", ".join(f"{s}://" for s in SUPPORTED_SCHEMES)
    )

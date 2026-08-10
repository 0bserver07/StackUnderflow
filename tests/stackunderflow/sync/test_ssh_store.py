"""``SSHObjectStore`` — URL parsing, key safety, and the ObjectStore contract.

The transport is exercised without a real host by faking the one seam that
touches the network: ``SSHObjectStore._run``. A tiny in-process shell stands in
for the remote, so these assert the *protocol* behaviour (missing object →
``ObjectNotFound``, atomic put, ``.part`` files hidden from ``list``) rather
than re-testing ssh itself. One test does shell out, to `ssh` against a
closed port, to prove a connection failure surfaces as ``SSHStoreError``.
"""

from __future__ import annotations

import shlex

import pytest

from stackunderflow.sync.bucket import (
    ObjectNotFound,
    requires_boto3,
    scheme_of,
    store_from_url,
)
from stackunderflow.sync.ssh_store import (
    _RC_NO_SUCH_OBJECT,
    _RC_NO_SUCH_ROOT,
    SSHObjectStore,
    SSHStoreError,
    SSHTarget,
    parse_ssh_url,
)

# Every fake response carries this on stderr. Real sshd installs emit banners
# and advisories (OpenSSH's post-quantum warning, "Last login", MOTD) on every
# connection, so any logic that infers meaning from stderr being empty is
# broken on a real host. Baking it into the fake keeps that regression caught.
_SSHD_BANNER = (
    b"** WARNING: connection is not using a post-quantum key exchange algorithm.\n"
    b"** This session may be vulnerable to \"store now, decrypt later\" attacks.\n"
)


class _FakeProc:
    def __init__(self, rc=0, stdout=b"", stderr=b""):
        self.returncode = rc
        self.stdout = stdout
        self.stderr = stderr


class _FakeRemote(SSHObjectStore):
    """An SSHObjectStore whose ``_run`` interprets the commands it is given.

    Only the handful of shell forms this module actually emits are understood;
    anything else fails loudly so a change in the emitted command can't pass
    silently.
    """

    def __init__(self, target: SSHTarget):
        super().__init__(target)
        self.files: dict[str, bytes] = {}
        self.commands: list[str] = []
        self.root_exists = True

    def _run(self, remote_cmd: str, *, stdin: bytes | None = None):  # type: ignore[override]
        self.commands.append(remote_cmd)
        # shlex.split, not string slicing: it unquotes exactly the way a real
        # shell would, so a path this fake can't recover is a path the real
        # remote would also have mis-split. That makes the quoting testable.
        tok = shlex.split(remote_cmd)

        if tok[:2] == ["mkdir", "-p"]:
            # mkdir -p <parent> && cat > <tmp> && mv -f <tmp> <final>
            self.files[tok[-1]] = stdin or b""
            self.root_exists = True
            return _FakeProc()

        # `if test -f P; then cat P; else exit 42; fi`
        if tok[:3] == ["if", "test", "-f"]:
            path = tok[3].rstrip(";")
            if path in self.files:
                return _FakeProc(stdout=self.files[path], stderr=_SSHD_BANNER)
            return _FakeProc(rc=_RC_NO_SUCH_OBJECT, stderr=_SSHD_BANNER)

        # `if test -d ROOT; then find ROOT -type f -print; else exit 43; fi`
        if tok[:3] == ["if", "test", "-d"]:
            if not self.root_exists:
                return _FakeProc(rc=_RC_NO_SUCH_ROOT, stderr=_SSHD_BANNER)
            return _FakeProc(
                stdout="\n".join(sorted(self.files)).encode(), stderr=_SSHD_BANNER
            )

        if tok[:2] == ["rm", "-f"]:
            self.files.pop(tok[2], None)
            return _FakeProc()

        raise AssertionError(f"unexpected remote command: {remote_cmd!r}")


def _store() -> _FakeRemote:
    return _FakeRemote(parse_ssh_url("ssh://user@host:22/srv/sync"))


# ── URL parsing ──────────────────────────────────────────────────────────────

def test_parses_user_host_port_and_path():
    t = parse_ssh_url("ssh://user@example.internal:22/srv/stackunderflow")
    assert t.host == "user@example.internal"
    assert t.port == 22
    assert t.root == "/srv/stackunderflow"
    assert t.ssh_argv()[-3:] == ["-p", "22", "user@example.internal"]


def test_user_and_port_are_optional():
    t = parse_ssh_url("ssh://example.internal/srv/x")
    assert t.host == "example.internal"
    assert t.port is None
    assert "-p" not in t.ssh_argv()


def test_trailing_slash_does_not_double_up_in_keys():
    assert parse_ssh_url("ssh://h/srv/x/").root == "/srv/x"


@pytest.mark.parametrize(
    "url",
    [
        "s3://bucket/prefix",            # wrong scheme
        "ssh://host",                    # no path
        "ssh://host/",                   # root only
        "ssh:///srv/x",                  # no host
    ],
)
def test_rejects_bad_urls(url):
    with pytest.raises(ValueError):
        parse_ssh_url(url)


# ── dispatch ─────────────────────────────────────────────────────────────────

def test_scheme_dispatch_picks_the_ssh_transport():
    store = store_from_url("ssh://host/srv/x")
    assert isinstance(store, SSHObjectStore)


def test_unsupported_scheme_is_rejected_with_guidance():
    with pytest.raises(ValueError, match="unsupported sync destination"):
        store_from_url("ftp://host/x")


def test_ssh_does_not_require_boto3_but_s3_does():
    assert requires_boto3("s3://bucket") is True
    assert requires_boto3("ssh://host/srv/x") is False
    # Unknown scheme stays conservative so the operator still gets the hint.
    assert requires_boto3("wat://x") is True
    assert scheme_of("ssh://host/x") == "ssh"


# ── ObjectStore contract ─────────────────────────────────────────────────────

def test_put_then_get_roundtrips():
    s = _store()
    s.put("dev-a/shard-1", b"ciphertext")
    assert s.get("dev-a/shard-1") == b"ciphertext"


def test_get_missing_raises_object_not_found():
    s = _store()
    with pytest.raises(ObjectNotFound):
        s.get("dev-a/nope")


def test_missing_object_is_detected_even_when_sshd_writes_to_stderr():
    """Regression: a chatty sshd must not turn a miss into a transport error.

    Found against a real host whose sshd prints a post-quantum advisory on every
    connection. The original implementation treated non-empty stderr as "this
    was a transport failure", so every absent object raised SSHStoreError —
    which would hard-fail the first `sync pull` against a fresh peer, where a
    missing manifest is the normal case.
    """
    s = _store()
    with pytest.raises(ObjectNotFound):
        s.get("dev-a/definitely-absent")
    # and the banner really was present on that response
    assert _SSHD_BANNER  # sanity: the fake is configured to emit it


def test_fresh_root_with_chatty_sshd_lists_empty_not_error():
    s = _store()
    s.root_exists = False
    assert s.list("") == []


def test_put_is_atomic_via_temp_then_rename():
    s = _store()
    s.put("k", b"v")
    cmd = s.commands[-1]
    assert ".part" in cmd and "mv -f" in cmd, "put must not write the final path directly"


def test_list_filters_by_prefix_and_hides_inflight_parts():
    s = _store()
    s.put("dev-a/one", b"1")
    s.put("dev-a/two", b"2")
    s.put("dev-b/three", b"3")
    s.files["/srv/sync/dev-a/four.part"] = b"partial"

    assert s.list("dev-a/") == ["dev-a/one", "dev-a/two"]
    assert s.list("") == ["dev-a/one", "dev-a/two", "dev-b/three"]


def test_list_on_a_fresh_destination_is_empty_not_an_error():
    s = _store()
    s.root_exists = False
    assert s.list("") == []


def test_delete_is_idempotent():
    s = _store()
    s.put("k", b"v")
    s.delete("k")
    s.delete("k")  # absent now; must not raise
    with pytest.raises(ObjectNotFound):
        s.get("k")


@pytest.mark.parametrize("key", ["/etc/passwd", "../outside", "a/../../b"])
def test_traversal_keys_are_refused(key):
    s = _store()
    with pytest.raises(ValueError, match="unsafe object key"):
        s.put(key, b"x")


def test_keys_with_spaces_are_quoted_not_split():
    s = _store()
    s.put("dev a/sh ard", b"v")
    assert s.get("dev a/sh ard") == b"v"


# ── one real ssh invocation ──────────────────────────────────────────────────

def test_connection_failure_surfaces_as_ssh_store_error():
    """A closed port must raise SSHStoreError, not hang or leak a CalledProcessError."""
    store = SSHObjectStore(
        SSHTarget(host="127.0.0.1", root="/tmp/nope", port=1), timeout=15
    )
    # BatchMode=yes means no prompt; port 1 refuses. Either a non-zero rc
    # (mapped to SSHStoreError) or a timeout (also SSHStoreError).
    with pytest.raises(SSHStoreError):
        store.put("k", b"v")

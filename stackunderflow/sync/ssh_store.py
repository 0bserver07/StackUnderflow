"""An :class:`~stackunderflow.sync.bucket.ObjectStore` backed by SSH.

Why this exists alongside the S3 store: syncing between machines you already
own does not need a cloud bucket, an account, or credentials to leak. If those
machines can reach each other over SSH (a tailnet, a LAN, a jump host), that is
already a private transport with authentication and encryption in transit.

The payload is unchanged — ``runner`` still encrypts every shard with ``age``
before it reaches ``put``, so the remote host sees only ciphertext, exactly as a
bucket would. This module is a *transport*, not a security boundary: it does not
weaken the zero-knowledge property, and it does not add to it either.

Deliberately dependency-free. Everything goes through the system ``ssh``
binary, so there is no paramiko/asyncssh to install and key handling, agents,
``~/.ssh/config`` aliases, ProxyJump and port forwarding all behave exactly as
they do in a normal shell. Auth is the operator's problem, as it should be:
supply a key, and this never prompts.
"""

from __future__ import annotations

import shlex
import subprocess
from dataclasses import dataclass
from urllib.parse import urlparse

from .bucket import ObjectNotFound

# ssh with a password prompt would hang a scripted push forever; fail instead.
_SSH_BASE_OPTS = (
    "-o", "BatchMode=yes",
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "ConnectTimeout=10",
)

_DEFAULT_TIMEOUT = 120

# Sentinel exit codes returned by the remote shell so "the object isn't there"
# and "the sync root doesn't exist yet" are distinguishable from a transport
# failure. Emptiness of stderr is NOT a usable signal: sshd banners and
# warnings (e.g. OpenSSH's post-quantum advisory) write to stderr on every
# single connection, which would make every miss look like an error.
_RC_NO_SUCH_OBJECT = 42
_RC_NO_SUCH_ROOT = 43


class SSHStoreError(RuntimeError):
    """An ssh invocation failed for a reason that is not a missing object."""


@dataclass(frozen=True)
class SSHTarget:
    """A parsed ``ssh://`` destination."""

    host: str                 # ``user@host`` or just ``host``
    root: str                 # absolute remote directory holding the shards
    port: int | None = None

    def ssh_argv(self) -> list[str]:
        argv = ["ssh", *_SSH_BASE_OPTS]
        if self.port is not None:
            argv += ["-p", str(self.port)]
        argv.append(self.host)
        return argv


def parse_ssh_url(url: str) -> SSHTarget:
    """Parse ``ssh://[user@]host[:port]/absolute/path`` into an :class:`SSHTarget`.

    The path is required and must be absolute: a relative remote path would
    resolve against whatever the login shell happens to cd into, which is not
    something a sync destination should depend on.
    """
    parsed = urlparse(url)
    if parsed.scheme != "ssh":
        raise ValueError(f"not an ssh:// URL: {url!r}")
    if not parsed.hostname:
        raise ValueError(f"ssh URL has no host: {url!r}")
    if not parsed.path or parsed.path == "/":
        raise ValueError(
            f"ssh URL needs an absolute remote directory, e.g. "
            f"ssh://host/srv/stackunderflow-sync (got {url!r})"
        )

    host = f"{parsed.username}@{parsed.hostname}" if parsed.username else parsed.hostname
    return SSHTarget(host=host, root=parsed.path.rstrip("/"), port=parsed.port)


class SSHObjectStore:
    """Key/value objects as files under a remote directory, moved over ssh.

    Satisfies the four-method ``ObjectStore`` protocol. Keys map to paths
    beneath :attr:`SSHTarget.root`; nested keys create their parent directories
    on ``put``.
    """

    def __init__(self, target: SSHTarget, *, timeout: int = _DEFAULT_TIMEOUT) -> None:
        self.target = target
        self.timeout = timeout

    # ── internals ────────────────────────────────────────────────────────────

    def _remote_path(self, key: str) -> str:
        """Resolve *key* to an absolute remote path, refusing escapes.

        Keys are generated internally (device uuids, shard names, manifests),
        but a traversal here would write outside the sync root, so it is
        checked rather than trusted.
        """
        if key.startswith("/") or ".." in key.split("/"):
            raise ValueError(f"unsafe object key: {key!r}")
        return f"{self.target.root}/{key}"

    def _run(self, remote_cmd: str, *, stdin: bytes | None = None) -> subprocess.CompletedProcess:
        """Run *remote_cmd* on the target host, capturing stdout/stderr."""
        argv = [*self.target.ssh_argv(), remote_cmd]
        try:
            return subprocess.run(  # noqa: S603 - argv is built here, never shell-parsed
                argv,
                input=stdin,
                capture_output=True,
                timeout=self.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise SSHStoreError(
                f"ssh timed out after {self.timeout}s against {self.target.host}"
            ) from exc
        except OSError as exc:
            raise SSHStoreError(f"could not run ssh: {exc}") from exc

    # ── ObjectStore protocol ─────────────────────────────────────────────────

    def put(self, key: str, data: bytes) -> None:
        path = shlex.quote(self._remote_path(key))
        parent = shlex.quote(self._remote_path(key).rsplit("/", 1)[0])
        # Write to a temp file and rename: a reader either sees the previous
        # object or the complete new one, never a half-written shard. The
        # manifest commit in `runner` depends on that.
        tmp = shlex.quote(self._remote_path(key) + ".part")
        cmd = f"mkdir -p {parent} && cat > {tmp} && mv -f {tmp} {path}"
        proc = self._run(cmd, stdin=data)
        if proc.returncode != 0:
            raise SSHStoreError(
                f"put {key!r} failed (rc={proc.returncode}): "
                f"{proc.stderr.decode(errors='replace').strip()}"
            )

    def get(self, key: str) -> bytes:
        path = shlex.quote(self._remote_path(key))
        # A missing object must be distinguishable from a transport failure —
        # `runner` treats "peer has no manifest yet" as normal and branches on
        # ObjectNotFound. Signal it with an exit code, never with stderr.
        proc = self._run(
            f"if test -f {path}; then cat {path}; else exit {_RC_NO_SUCH_OBJECT}; fi"
        )
        if proc.returncode == _RC_NO_SUCH_OBJECT:
            raise ObjectNotFound(key)
        if proc.returncode != 0:
            raise SSHStoreError(
                f"get {key!r} failed (rc={proc.returncode}): "
                f"{proc.stderr.decode(errors='replace').strip()}"
            )
        return proc.stdout

    def list(self, prefix: str) -> list[str]:  # noqa: A003 - part of the ObjectStore interface
        root = shlex.quote(self.target.root)
        # A missing root is an empty store, not an error: `sync push` to a fresh
        # destination must work without the operator pre-creating anything. A
        # find that fails for any OTHER reason (permissions) must still raise
        # rather than masquerade as an empty store.
        proc = self._run(
            f"if test -d {root}; then find {root} -type f -print; "
            f"else exit {_RC_NO_SUCH_ROOT}; fi"
        )
        if proc.returncode == _RC_NO_SUCH_ROOT:
            return []
        if proc.returncode != 0:
            raise SSHStoreError(
                f"list failed (rc={proc.returncode}): "
                f"{proc.stderr.decode(errors='replace').strip()}"
            )
        keys: list[str] = []
        for line in proc.stdout.decode(errors="replace").splitlines():
            line = line.strip()
            if not line or not line.startswith(self.target.root + "/"):
                continue
            key = line[len(self.target.root) + 1:]
            if key.endswith(".part"):
                continue  # an in-flight put, not an object
            if key.startswith(prefix):
                keys.append(key)
        return sorted(keys)

    def delete(self, key: str) -> None:
        path = shlex.quote(self._remote_path(key))
        # `rm -f` so deleting an absent object is a no-op, matching S3/in-memory.
        proc = self._run(f"rm -f {path}")
        if proc.returncode != 0:
            raise SSHStoreError(
                f"delete {key!r} failed (rc={proc.returncode}): "
                f"{proc.stderr.decode(errors='replace').strip()}"
            )


def ssh_store_from_url(url: str, *, timeout: int = _DEFAULT_TIMEOUT) -> SSHObjectStore:
    """Build an :class:`SSHObjectStore` from an ``ssh://…`` URL."""
    return SSHObjectStore(parse_ssh_url(url), timeout=timeout)

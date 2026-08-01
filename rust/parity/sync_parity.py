#!/usr/bin/env python3
"""The Python half of the wave-6 sync differ.

One process per case. `argv[1]` is the op, the rest are its arguments, and the
answer goes to stdout as ONE line of `json.dumps(obj, separators=(",", ":"))`.
`crates/stax-sync/src/bin/sync_parity.rs` is the other half: same ops, same
arguments, same writer (`pyjson::dumps_compact`), so `diff` over the two lines
is the whole comparison.

Why a driver rather than the CLI
--------------------------------
Most of `sync/` is not reachable from `stackunderflow sync` without a network.
`push`/`pull` take an injected encryptor and an injected `ObjectStore`
*precisely* so they can be exercised without one — that is the reference's own
design decision, and this driver takes it up. The four CLI verbs are diffed
separately, through the real binaries, by `sync-parity.sh`'s `cli/*` rows.

Two pieces of scaffolding live here and NOT in the product
----------------------------------------------------------
* ``FileObjectStore`` — a directory-backed ``ObjectStore``. It is how a `push`
  case and a `pull` case share a bucket across two processes. Both drivers
  implement it identically in ~20 lines; it is not a transport being tested.
* ``identity_encryptor`` — `lambda b: b`. age ciphertext is randomised (fresh
  ephemeral key per blob), so it can never be byte-compared; the *plaintext* is
  what push idempotency hashes, and that is what these cases compare. Real age
  is proven separately by the `crypto/*` interop rows, which round-trip a blob
  through the OTHER implementation.

No network. No ssh. `ssh_store` is exercised as ARGV: the exact list that would
reach `execve`, compared string for string.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import os
import shutil
import sqlite3
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from stackunderflow.infra import egress  # noqa: E402
from stackunderflow.sync import bucket as bucket_mod  # noqa: E402
from stackunderflow.sync import merge, runner, serialize  # noqa: E402
from stackunderflow.sync import keys as keys_mod  # noqa: E402
from stackunderflow.sync import ssh_store  # noqa: E402


# ── scaffolding ───────────────────────────────────────────────────────────────


class FileObjectStore:
    """A directory-backed ObjectStore, so two processes can share one bucket."""

    def __init__(self, root: Path) -> None:
        self.root = Path(root)
        self.put_calls = 0
        self.get_calls = 0
        self.list_calls = 0
        self.delete_calls = 0

    def _path(self, key: str) -> Path:
        return self.root / key

    def put(self, key: str, data: bytes) -> None:
        self.put_calls += 1
        path = self._path(key)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def get(self, key: str) -> bytes:
        self.get_calls += 1
        path = self._path(key)
        if not path.is_file():
            raise bucket_mod.ObjectNotFound(key)
        return path.read_bytes()

    def list(self, prefix: str) -> list[str]:  # noqa: A003
        self.list_calls += 1
        if not self.root.is_dir():
            return []
        keys = []
        for path in self.root.rglob("*"):
            if not path.is_file():
                continue
            key = path.relative_to(self.root).as_posix()
            if key.startswith(prefix):
                keys.append(key)
        return sorted(keys)

    def delete(self, key: str) -> None:
        self.delete_calls += 1
        path = self._path(key)
        if path.is_file():
            path.unlink()


def identity_encryptor(payload: bytes) -> bytes:
    return payload


def emit(obj) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.write("\n")


def b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode("ascii")


def unb64(text: str) -> bytes:
    return base64.b64decode(text)


def sha(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def connect(path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(path, isolation_level=None)
    conn.row_factory = sqlite3.Row
    return conn


def dash(value: str) -> str | None:
    """The corpus spells "absent" as a bare ``-``; argv cannot carry None."""
    return None if value == "-" else value


# ── store dumps (the differ's evidence, not the reference's API) ──────────────


def dump_table(conn: sqlite3.Connection, table: str, order: str) -> list[dict]:
    rows = conn.execute(f"SELECT * FROM {table} ORDER BY {order}").fetchall()
    return [dict(r) for r in rows]


def dump_bucket(root: Path) -> dict:
    out: dict[str, dict] = {}
    if root.is_dir():
        for path in sorted(root.rglob("*")):
            if path.is_file():
                body = path.read_bytes()
                key = path.relative_to(root).as_posix()
                out[key] = {"len": len(body), "sha256": sha(body)}
    return out


def decode_manifest(root: Path, device: str) -> object:
    path = root / runner.manifest_key(device)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_bytes())
    except Exception as exc:  # noqa: BLE001 — the dump reports it, never raises
        return {"_undecodable": str(exc)}


def dump_remote_tables(conn: sqlite3.Connection) -> dict:
    return {
        "daily_mart_remote": dump_table(
            conn, "daily_mart_remote", "device_uuid, day, provider, slug, model, speed"
        ),
        "provider_day_mart_remote": dump_table(
            conn, "provider_day_mart_remote", "device_uuid, day, provider"
        ),
        "model_day_mart_remote": dump_table(
            conn, "model_day_mart_remote", "device_uuid, day, model, speed"
        ),
        "project_mart_remote": dump_table(
            conn, "project_mart_remote", "device_uuid, provider, slug"
        ),
        "session_mart_remote": dump_table(
            conn, "session_mart_remote", "device_uuid, session_id"
        ),
    }


# ── bucket mutations (failure injection, identical on both sides) ─────────────


def first_shard_key(root: Path, device: str) -> str | None:
    shards = sorted(
        p.relative_to(root).as_posix()
        for p in (root / runner.DEFAULT_PREFIX / device / "shards").rglob("*.age")
    ) if (root / runner.DEFAULT_PREFIX / device / "shards").is_dir() else []
    return shards[0] if shards else None


def rewrite_manifest(root: Path, device: str, mutate) -> None:
    path = root / runner.manifest_key(device)
    manifest = json.loads(path.read_bytes())
    mutate(manifest)
    path.write_bytes(
        json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode("utf-8")
    )


def mutate_shard(root: Path, device: str, mutate) -> None:
    """Rewrite the first shard AND refresh its manifest hash/size to match.

    Without the manifest refresh the case would stop at `content-hash mismatch`
    and never reach the family / column checks it exists to exercise.
    """
    key = first_shard_key(root, device)
    if key is None:
        return
    path = root / key
    payload = json.loads(path.read_bytes())
    mutate(payload)
    body = json.dumps(
        payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    path.write_bytes(body)
    shard_key = Path(key).name[: -len(".age")]

    def fix(manifest):
        entry = manifest["shards"].get(shard_key)
        if entry is not None:
            entry["content_hash"] = sha(body)
            entry["bytes"] = len(body)

    rewrite_manifest(root, device, fix)


def apply_mode(mode: str, root: Path, peer: str, target: sqlite3.Connection) -> None:
    if mode in ("normal", "twice", "self-only"):
        return
    if mode == "corrupt-shard":
        key = first_shard_key(root, peer)
        if key:
            (root / key).write_bytes(bytes([0x00, 0x9F, 0x12, 0xFF]))
    elif mode == "missing-object":
        key = first_shard_key(root, peer)
        if key:
            (root / key).unlink()
    elif mode == "no-manifest":
        (root / runner.manifest_key(peer)).unlink()
    elif mode == "not-json-manifest":
        (root / runner.manifest_key(peer)).write_bytes(b"not json")
    elif mode == "empty-manifest":
        (root / runner.manifest_key(peer)).write_bytes(b"")
    elif mode == "bad-schema":
        rewrite_manifest(root, peer, lambda m: m.__setitem__("schema", "bogus/9"))
    elif mode == "no-schema":
        rewrite_manifest(root, peer, lambda m: m.pop("schema", None))
    elif mode == "stale-gen":
        target.execute(
            "INSERT INTO sync_remote_devices "
            "(remote_device_uuid, alias, key_fingerprint, first_seen, last_seen, last_generation) "
            "VALUES (?, NULL, 'seed', 'seed-ts', 'seed-ts', 99)",
            (peer,),
        )
    elif mode == "gen-string":
        rewrite_manifest(root, peer, lambda m: m.__setitem__("generation", "7"))
    elif mode == "gen-float":
        rewrite_manifest(root, peer, lambda m: m.__setitem__("generation", 2.9))
    elif mode == "gen-missing":
        rewrite_manifest(root, peer, lambda m: m.pop("generation", None))
    elif mode == "unknown-family":
        mutate_shard(root, peer, lambda p: p.__setitem__("family", "bogus_mart"))
    elif mode == "column-mismatch":
        mutate_shard(root, peer, lambda p: p.__setitem__("columns", p["columns"][:-1]))
    elif mode == "no-shards":
        rewrite_manifest(root, peer, lambda m: m.__setitem__("shards", {}))
    else:
        raise SystemExit(f"sync_parity: unknown pull mode {mode!r}")


# ── ops ───────────────────────────────────────────────────────────────────────


def op_keys_fingerprint(recipient: str) -> None:
    emit({"fingerprint": keys_mod.fingerprint(recipient)})


def op_keys_identity_path(state_dir: str) -> None:
    emit({"path": str(keys_mod.identity_path(state_dir))})


def op_keys_resolve(state_dir: str, env_value: str, keychain: str, file_value: str) -> None:
    env = {} if env_value == "-" else {keys_mod.ENV_KEY: env_value}
    reader = (lambda: None) if keychain == "-" else (lambda: keychain)
    if file_value != "-":
        keys_mod.store_secret_file(file_value, state_dir)
    emit(
        {
            "secret": keys_mod.resolve_secret(state_dir, env=env, keychain_reader=reader),
        }
    )


def op_keys_store_file(state_dir: str, secret: str) -> None:
    path = keys_mod.store_secret_file(secret, state_dir)
    emit(
        {
            "name": path.name,
            "mode": oct(path.stat().st_mode & 0o777),
            "content": path.read_text(),
        }
    )


def op_url_bucket(url: str) -> None:
    try:
        name, prefix = bucket_mod.parse_bucket_url(url)
    except ValueError as exc:
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "bucket": name, "prefix": prefix})


def op_url_scheme(url: str) -> None:
    emit(
        {
            "scheme": bucket_mod.scheme_of(url),
            "requires_boto3": bucket_mod.requires_boto3(url),
            "supported": list(bucket_mod.SUPPORTED_SCHEMES),
        }
    )


def op_url_store_from(url: str, endpoint: str) -> None:
    """`store_from_url`'s DISPATCH — which branch, and the error text.

    The s3 branch is not *constructed* here: it needs boto3, which is not
    installed on the parity host (DIV-213). What is compared is the decision and
    the message, which is all the Rust side resolves too.
    """
    scheme = bucket_mod.scheme_of(url)
    if scheme == "ssh":
        try:
            target = ssh_store.parse_ssh_url(url)
        except ValueError as exc:
            emit({"ok": False, "error": str(exc)})
            return
        emit({"ok": True, "kind": "ssh", "host": target.host, "root": target.root,
              "port": target.port})
        return
    if scheme == "s3":
        try:
            name, prefix = bucket_mod.parse_bucket_url(url)
        except ValueError as exc:
            emit({"ok": False, "error": str(exc)})
            return
        emit({"ok": True, "kind": "s3", "bucket": name, "prefix": prefix,
              "endpoint_url": dash(endpoint)})
        return
    emit(
        {
            "ok": False,
            "error": (
                f"unsupported sync destination {url!r} — expected one of: "
                + ", ".join(f"{s}://" for s in bucket_mod.SUPPORTED_SCHEMES)
            ),
        }
    )


def op_ssh_parse(url: str) -> None:
    try:
        target = ssh_store.parse_ssh_url(url)
    except ValueError as exc:
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "host": target.host, "root": target.root, "port": target.port,
          "argv": target.ssh_argv()})


def _invocation(url: str, build) -> None:
    """Capture the argv an SSHObjectStore method WOULD run, without running it."""
    try:
        target = ssh_store.parse_ssh_url(url)
    except ValueError as exc:
        emit({"ok": False, "error": str(exc)})
        return
    store = ssh_store.SSHObjectStore(target)
    captured: dict = {}

    def fake_run(remote_cmd, *, stdin=None):
        captured["argv"] = [*store.target.ssh_argv(), remote_cmd]
        captured["stdin"] = stdin is not None
        raise _Captured

    store._run = fake_run  # noqa: SLF001 — the whole point is to intercept the spawn
    try:
        build(store)
    except _Captured:
        pass
    except ValueError as exc:
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "argv": captured.get("argv"), "stdin": captured.get("stdin")})


class _Captured(Exception):
    """Unwinds out of a store method once its argv has been recorded."""


def op_ssh_put(url: str, key: str) -> None:
    _invocation(url, lambda s: s.put(key, b"BODY"))


def op_ssh_get(url: str, key: str) -> None:
    _invocation(url, lambda s: s.get(key))


def op_ssh_list(url: str, prefix: str) -> None:
    _invocation(url, lambda s: s.list(prefix))


def op_ssh_delete(url: str, key: str) -> None:
    _invocation(url, lambda s: s.delete(key))


def op_ssh_find(url: str, prefix: str, stdout_b64: str) -> None:
    """The LOCAL half of `list`: how `find` output becomes keys."""
    target = ssh_store.parse_ssh_url(url)
    store = ssh_store.SSHObjectStore(target)
    stdout = unb64(stdout_b64)

    class _Proc:
        returncode = 0

    proc = _Proc()
    proc.stdout = stdout
    proc.stderr = b""
    store._run = lambda *a, **k: proc  # noqa: SLF001
    emit({"keys": store.list(prefix)})


def op_shlex_quote(text_b64: str) -> None:
    import shlex

    emit({"quoted": shlex.quote(unb64(text_b64).decode("utf-8"))})


def op_object_keys(device: str, shard_key: str) -> None:
    emit(
        {
            "object_key": runner.object_key(device, shard_key),
            "manifest_key": runner.manifest_key(device),
            "prefix": runner.DEFAULT_PREFIX,
            "schema": runner.MANIFEST_SCHEMA,
        }
    )


def op_rsync_plan(dest_name: str, dest_path: str, to_url: str, previous: str) -> None:
    """`cli.py::_replicate_backup`'s two argvs, rebuilt from its own source.

    Transcribed rather than imported because the reference inlines it in a
    function that also echoes and spawns. Every line below is that function's,
    in its order — the differ's job is to catch a transcription that drifted.
    """
    import shlex

    try:
        target = ssh_store.parse_ssh_url(to_url)
    except ValueError as exc:
        emit({"ok": False, "error": str(exc)})
        return

    ssh_cmd = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
    if target.port is not None:
        ssh_cmd += f" -p {target.port}"

    remote_dir = f"{target.root}/{dest_name}"
    mkdir_argv = [*target.ssh_argv(), f"mkdir -p {shlex.quote(target.root)}"]

    cmd = ["rsync", "-a", "-e", ssh_cmd]
    if previous != "-":
        cmd.append(f"--link-dest={target.root}/{previous}")
    cmd += [f"{dest_path}/", f"{target.host}:{remote_dir}/"]

    emit({"ok": True, "remote_dir": remote_dir, "mkdir_argv": mkdir_argv,
          "rsync_argv": cmd, "ssh_cmd": ssh_cmd})


def op_rsync_outcome(returncode: str, stderr_b64: str, what: str) -> None:
    from stackunderflow.cli import _rsync_outcome, _rsync_reported

    stderr = unb64(stderr_b64).decode("utf-8")
    ok, message = _rsync_outcome(int(returncode), stderr, what=what)
    emit({"ok": ok, "message": message, "reported": _rsync_reported(stderr)})


def op_shards(store_path: str) -> None:
    conn = connect(store_path)
    try:
        shards = serialize.build_shards(conn)
    finally:
        conn.close()
    emit(
        {
            "count": len(shards),
            "families": list(serialize.MART_FAMILIES),
            "shards": [
                {
                    "shard_key": s.shard_key,
                    "family": s.family,
                    "month": s.month,
                    "columns": list(s.columns),
                    "row_count": len(s.rows),
                    "bytes": len(s.to_bytes()),
                    "content_hash": s.content_hash,
                    "canonical": s.to_bytes().decode("utf-8"),
                }
                for s in shards
            ],
        }
    )


def op_shard_roundtrip(store_path: str) -> None:
    conn = connect(store_path)
    try:
        shards = serialize.build_shards(conn)
    finally:
        conn.close()
    out = []
    for s in shards:
        restored = serialize.shard_from_bytes(s.to_bytes())
        out.append(
            {
                "shard_key": s.shard_key,
                "hash_before": s.content_hash,
                "hash_after": restored.content_hash,
                "stable": restored.content_hash == s.content_hash,
                "columns_equal": tuple(restored.columns) == tuple(s.columns),
            }
        )
    emit({"shards": out})


def op_month_of(raw: str) -> None:
    value = json.loads(raw)
    emit({"month": serialize._month_of(value)})  # noqa: SLF001


def op_json_loads(payload_b64: str) -> None:
    try:
        json.loads(unb64(payload_b64))
    except Exception as exc:  # noqa: BLE001
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "error": None})


def op_py_int(raw: str) -> None:
    value = json.loads(raw)
    try:
        emit({"ok": True, "value": int(value)})
    except Exception as exc:  # noqa: BLE001
        emit({"ok": False, "error": str(exc)})


def op_push(store_path: str, bucket_root: str, device: str, now: str, repeat: str) -> None:
    root = Path(bucket_root)
    store = FileObjectStore(root)
    conn = connect(store_path)
    try:
        results = []
        for _ in range(int(repeat)):
            results.append(
                runner.push(
                    conn,
                    store,
                    device_uuid=device,
                    key_fingerprint="fp0123456789abcd",
                    encryptor=identity_encryptor,
                    now=now,
                )
            )
        outbox = dump_table(conn, "sync_outbox", "shard_key")
    finally:
        conn.close()
    emit(
        {
            "results": [
                {
                    "uploaded": r.uploaded,
                    "skipped": r.skipped,
                    "bytes_uploaded": r.bytes_uploaded,
                    "generation": r.generation,
                    "manifest_written": r.manifest_written,
                    "shard_keys": list(r.shard_keys),
                }
                for r in results
            ],
            "counters": {
                "put": store.put_calls,
                "get": store.get_calls,
                "list": store.list_calls,
                "delete": store.delete_calls,
            },
            "bucket": dump_bucket(root),
            "manifest": decode_manifest(root, device),
            "outbox": outbox,
        }
    )


def op_pull(
    target_store: str,
    peer_store: str,
    bucket_root: str,
    peer_uuid: str,
    self_uuid: str,
    now: str,
    mode: str,
) -> None:
    root = Path(bucket_root)
    seeded = FileObjectStore(root)
    peer_conn = connect(peer_store)
    try:
        runner.push(
            peer_conn,
            seeded,
            device_uuid=peer_uuid if mode != "self-only" else self_uuid,
            key_fingerprint="fp0123456789abcd",
            encryptor=identity_encryptor,
            now="2026-07-01T00:00:00+00:00",
        )
    finally:
        peer_conn.close()

    conn = connect(target_store)
    try:
        apply_mode(mode, root, peer_uuid, conn)
        store = FileObjectStore(root)
        rounds = 2 if mode == "twice" else 1
        results = []
        for _ in range(rounds):
            results.append(
                runner.pull(
                    conn,
                    store,
                    self_device_uuid=self_uuid,
                    decryptor=identity_encryptor,
                    now=now,
                )
            )
        payload = {
            "results": [r.as_dict() for r in results],
            "counters": {
                "put": store.put_calls,
                "get": store.get_calls,
                "list": store.list_calls,
                "delete": store.delete_calls,
            },
            "cursors": dump_table(conn, "sync_cursors", "remote_device_uuid, shard_key"),
            "devices": dump_table(conn, "sync_remote_devices", "remote_device_uuid"),
            "remote": dump_remote_tables(conn),
            "remote_rows": merge.remote_row_count(conn),
        }
    except Exception as exc:  # noqa: BLE001 — a RAISE is a comparable answer too
        payload = {"raised": f"{type(exc).__name__}: {exc}"}
    finally:
        conn.close()
    emit(payload)


def op_status(store_path: str) -> None:
    conn = connect(store_path)
    try:
        emit(runner.status(conn).as_dict())
    finally:
        conn.close()


def op_merge_overview(store_path: str) -> None:
    conn = connect(store_path)
    try:
        emit(merge.merged_overview(conn))
    finally:
        conn.close()


def op_merge_parts(store_path: str) -> None:
    conn = connect(store_path)
    try:
        sessions, warnings = merge.unioned_sessions(conn)
        emit(
            {
                "daily": merge.unioned_daily(conn),
                "provider_day": merge.unioned_provider_day(conn),
                "model_day": merge.unioned_model_day(conn),
                "projects": merge.unioned_projects(conn),
                "sessions": sessions,
                "merge_warnings": warnings,
                "devices": merge.device_breakdown(conn),
                "remote_rows": merge.remote_row_count(conn),
            }
        )
    finally:
        conn.close()


_ALLOWLISTS = {
    "embed": egress.OLLAMA_EMBED_KEYS,
    "chat": egress.OLLAMA_CHAT_KEYS,
}


def op_egress_guard(kind: str, allow: str, body_b64: str) -> None:
    body = json.loads(unb64(body_b64))
    try:
        result = egress.guard_json_body(body, allow=_ALLOWLISTS[allow], kind=kind)
    except egress.EgressViolation as exc:
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "body": result})


def op_egress_serialize(body_b64: str) -> None:
    emit({"text": egress.serialize(json.loads(unb64(body_b64)))})


def op_egress_scan(body_b64: str, needles_b64: str) -> None:
    text = egress.serialize(json.loads(unb64(body_b64)))
    emit({"hits": egress.scan(text, json.loads(unb64(needles_b64)))})


def op_cipher_encrypt(recipient: str, plaintext_b64: str) -> None:
    from stackunderflow.sync import cipher

    emit({"ciphertext": b64(cipher.encrypt(unb64(plaintext_b64), recipient))})


def op_cipher_decrypt(secret: str, ciphertext_b64: str) -> None:
    from stackunderflow.sync import cipher

    try:
        plain = cipher.decrypt(unb64(ciphertext_b64), secret)
    except Exception as exc:  # noqa: BLE001
        emit({"ok": False, "error": str(exc)})
        return
    emit({"ok": True, "plaintext": b64(plain)})


def op_cipher_genkey() -> None:
    ident = keys_mod.generate_identity()
    emit(
        {
            "secret": ident.secret,
            "recipient": ident.recipient,
            "fingerprint": ident.fingerprint,
        }
    )


def op_cipher_recipient(secret: str) -> None:
    recipient = keys_mod.recipient_for(secret)
    emit({"recipient": recipient, "fingerprint": keys_mod.fingerprint(recipient)})


OPS = {
    "keys-fingerprint": op_keys_fingerprint,
    "keys-identity-path": op_keys_identity_path,
    "keys-resolve": op_keys_resolve,
    "keys-store-file": op_keys_store_file,
    "url-bucket": op_url_bucket,
    "url-scheme": op_url_scheme,
    "url-store-from": op_url_store_from,
    "ssh-parse": op_ssh_parse,
    "ssh-put": op_ssh_put,
    "ssh-get": op_ssh_get,
    "ssh-list": op_ssh_list,
    "ssh-delete": op_ssh_delete,
    "ssh-find": op_ssh_find,
    "shlex-quote": op_shlex_quote,
    "object-keys": op_object_keys,
    "rsync-plan": op_rsync_plan,
    "rsync-outcome": op_rsync_outcome,
    "shards": op_shards,
    "shard-roundtrip": op_shard_roundtrip,
    "month-of": op_month_of,
    "json-loads": op_json_loads,
    "py-int": op_py_int,
    "push": op_push,
    "pull": op_pull,
    "status": op_status,
    "merge-overview": op_merge_overview,
    "merge-parts": op_merge_parts,
    "egress-guard": op_egress_guard,
    "egress-serialize": op_egress_serialize,
    "egress-scan": op_egress_scan,
    "cipher-encrypt": op_cipher_encrypt,
    "cipher-decrypt": op_cipher_decrypt,
    "cipher-genkey": op_cipher_genkey,
    "cipher-recipient": op_cipher_recipient,
}


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] in ("-h", "--help"):
        sys.stderr.write("usage: sync_parity.py <op> [args…]\n")
        sys.stderr.write("ops: " + ", ".join(sorted(OPS)) + "\n")
        return 2
    op = argv[1]
    handler = OPS.get(op)
    if handler is None:
        sys.stderr.write(f"sync_parity: unknown op {op!r}\n")
        return 2
    handler(*argv[2:])
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

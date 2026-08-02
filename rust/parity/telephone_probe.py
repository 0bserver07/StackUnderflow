#!/usr/bin/env python3
"""The telephone differ's REFERENCE probe — the reference's own functions.

The twin of `crates/stax-cli/src/bin/telephone_parity.rs`. Every probe calls
into `stackunderflow` rather than re-deriving anything, so a change to the
reference moves this side automatically and the differ notices.

Which reference: whatever `$PYTHONPATH` names. The telephone lives on the
`feat/unified-python` branch, so `telephone-differ.sh` points
`STAX_PARITY_PY_PATH` at a checkout of it — see that script's header. Running
this against a tree without `services/agent_inbox.py` fails loudly at import,
which is the correct failure: a differ that silently skipped would be a differ
that agrees by accident.

Probes
------
payload <sender> <id> <ts> <text>
    `agent_inbox.message_payload` with both clocks injected. The reference
    reads them inside the function, so they are monkeypatched for the call —
    the ONLY thing this file overrides, and the whole reason the payload writer
    is byte-comparable at all.
ssh-put <url> <key>
    The argv `SSHObjectStore.put` would hand to `subprocess.run`, captured by
    intercepting `subprocess.run` itself. Nothing is spawned and no socket is
    opened; the interception raises so the call cannot continue past capture.
stamp <epoch-seconds>
    `time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(epoch))`.
id <millis> <6-hex>
    `agent_inbox.new_message_id` with `time.time` and `os.urandom` pinned.
sender-name
    `agent_inbox.sender_name()`.

Output is written to stdout as raw bytes, never through `print`'s encoder
guesswork: the payload body is `ensure_ascii=False`, so its bytes are the thing
under test.
"""

from __future__ import annotations

import os
import sys
import time


def _emit(data: bytes) -> None:
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


def probe_payload(sender: str, mid: str, ts: str, text: str) -> None:
    from stackunderflow.services import agent_inbox

    real_time, real_urandom, real_strftime = time.time, os.urandom, time.strftime
    # `new_message_id` is `f"{int(time.time()*1000):013x}-{os.urandom(3).hex()}"`
    # and `message_payload` calls it, so pinning the two sources pins the id.
    # The id we want is given, so the clock is bent to produce exactly it.
    ms_hex, rand_hex = mid.split("-", 1)
    time.time = lambda: int(ms_hex, 16) / 1000.0
    os.urandom = lambda n: bytes.fromhex(rand_hex)[:n]
    time.strftime = lambda fmt, *a: ts if "%z" in fmt else real_strftime(fmt, *a)
    try:
        key, body = agent_inbox.message_payload(text, sender)
    finally:
        time.time, os.urandom, time.strftime = real_time, real_urandom, real_strftime
    _emit(key.encode() + b"\n" + body + b"\n")


class _Captured(Exception):
    """Carries the argv out of the intercepted `subprocess.run`."""

    def __init__(self, argv: list[str], stdin: bytes | None) -> None:
        super().__init__("captured")
        self.argv = argv
        self.stdin = stdin


def probe_ssh_put(url: str, key: str) -> None:
    import subprocess

    from stackunderflow.sync.ssh_store import ssh_store_from_url

    def _intercept(argv, **kwargs):  # noqa: ANN001, ANN003
        raise _Captured(list(argv), kwargs.get("input"))

    # A URL `parse_ssh_url` rejects never reaches an argv, and the message it
    # rejects with is itself a contract (it is what `msg send` prints). Report
    # it on stdout rather than as a traceback, so the two probes compare the
    # message and not CPython's frame rendering.
    try:
        store = ssh_store_from_url(url)
    except ValueError as exc:
        _emit(b"error: " + str(exc).encode() + b"\n")
        return
    real_run = subprocess.run
    subprocess.run = _intercept
    try:
        store.put(key, b"body-under-test")
    except _Captured as captured:
        argv, stdin = captured.argv, captured.stdin
    else:  # pragma: no cover - the interception always fires
        raise SystemExit("ssh_store.put did not call subprocess.run")
    finally:
        subprocess.run = real_run
    out = b"".join(arg.encode() + b"\n" for arg in argv)
    out += b"stdin: yes\n" if stdin is not None else b"stdin: no\n"
    _emit(out)


def probe_stamp(epoch: str) -> None:
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(int(epoch)))
    _emit(stamp.encode() + b"\n")


def probe_id(millis: str, hexdigits: str) -> None:
    from stackunderflow.services import agent_inbox

    real_time, real_urandom = time.time, os.urandom
    time.time = lambda: int(millis) / 1000.0
    os.urandom = lambda n: bytes.fromhex(hexdigits)[:n]
    try:
        mid = agent_inbox.new_message_id()
    finally:
        time.time, os.urandom = real_time, real_urandom
    _emit(mid.encode() + b"\n")


def probe_sender_name() -> None:
    from stackunderflow.services import agent_inbox

    _emit(agent_inbox.sender_name().encode() + b"\n")


def main(argv: list[str]) -> int:
    if not argv:
        sys.stderr.write(__doc__ or "")
        return 2
    probe, rest = argv[0], argv[1:]
    table = {
        "payload": (4, probe_payload),
        "ssh-put": (2, probe_ssh_put),
        "stamp": (1, probe_stamp),
        "id": (2, probe_id),
        "sender-name": (0, probe_sender_name),
    }
    if probe not in table:
        sys.stderr.write(f"unknown probe: {probe}\n")
        return 2
    arity, handler = table[probe]
    if len(rest) != arity:
        sys.stderr.write(f"{probe} takes {arity} argument(s), got {len(rest)}\n")
        return 2
    handler(*rest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

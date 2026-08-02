#!/usr/bin/env python3
"""A raw-socket HTTP requester for the receiver differ. One client, two servers.

Campaign finding 12: "a byte-parity differ must not use an HTTP client
library." reqwest/httpx/curl decompress, follow redirects, retry and normalise
headers — each of which can turn a real divergence into a green tick. So this
writes the request bytes onto a socket and prints everything that comes back,
verbatim, until the peer closes.

It is the HARNESS, not an implementation, and it is the SAME client for both
sides — so anything it normalises it normalises symmetrically, and the only
thing it can hide is a difference in how the two servers treat this exact byte
sequence, which is nothing.

What is compared, and why exactly this much: **the status line, the
`content-type`, the `content-length`, and the body bytes.** That is the
campaign's existing endpoint gate ("diff status + content-type + BODY BYTES",
`endpoint-parity.sh`'s header) plus the length, which is free and catches a
body that differs only in trailing bytes.

What is deliberately NOT compared, and is reported as a masked class on every
run rather than dropped in silence:

  date:            a wall clock; both servers stamp it, neither can match
  server:          uvicorn names itself, axum sends no `server` header at all
  header ORDER     starlette and axum emit the same fields in different orders

The last two are framework identity, not behaviour, and they are identical on
every row — which is precisely why they must be stated: a mask that hides a
constant difference is fine, a mask that could hide a varying one is not.

Usage:
  raw_request.py <host> <port> <method> <path> [--header K:V]... [--body TEXT]
"""

from __future__ import annotations

import socket
import sys


def main(argv: list[str]) -> int:
    if len(argv) < 4:
        sys.stderr.write(__doc__ or "")
        return 2
    host, port, method, path = argv[0], int(argv[1]), argv[2], argv[3]
    headers: list[str] = []
    body = b""
    rest = argv[4:]
    while rest:
        flag, rest = rest[0], rest[1:]
        if flag == "--header":
            headers.append(rest[0])
            rest = rest[1:]
        elif flag == "--body":
            body = rest[0].encode()
            rest = rest[1:]
        else:
            sys.stderr.write(f"unknown flag {flag}\n")
            return 2

    lines = [f"{method} {path} HTTP/1.1", f"Host: {host}:{port}", "Connection: close"]
    lines += headers
    lines.append(f"Content-Length: {len(body)}")
    request = ("\r\n".join(lines) + "\r\n\r\n").encode() + body

    with socket.create_connection((host, port), timeout=15) as sock:
        sock.sendall(request)
        chunks: list[bytes] = []
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
    raw = b"".join(chunks)
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.split(b"\r\n")
    status = lines[0] if lines else b""
    fields: dict[bytes, bytes] = {}
    for line in lines[1:]:
        name, _, value = line.partition(b":")
        fields[name.strip().lower()] = value.strip()
    out = [status]
    for name in (b"content-type", b"content-length"):
        out.append(name + b": " + fields.get(name, b"<absent>"))
    sys.stdout.buffer.write(b"\n".join(out) + b"\n\n" + body)
    sys.stdout.buffer.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

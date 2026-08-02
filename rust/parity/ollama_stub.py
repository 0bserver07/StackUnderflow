#!/usr/bin/env python
"""A deterministic, LOOPBACK-ONLY stand-in for Ollama's two embed endpoints.

`memory embed`'s interesting legs all sit behind `active_endpoint()`, which is
a live `GET /api/tags`. Without something answering it, the only reachable leg
is the one the parity rows already cover (`T2-embed-*`, exit 1). This serves
the two calls the verb makes and nothing else:

* ``GET  /api/tags``       → ``200 {"models": []}`` — the reachability probe.
* ``POST /api/embeddings`` → ``200 {"embedding": [...]}``

The vector is a pure function of the prompt (a 16-dimensional FNV-1a walk), so
both implementations receive **identical** numbers for identical text and
`embeddings.db` can be compared blob for blob. A random or model-derived vector
would make the two stores differ for a reason that is not the port's, which is
the failure mode this file exists to avoid.

Bound to 127.0.0.1 only. Nothing leaves the machine; this is not a network
dependency, it is a fixture — the same shape `parity/pyserver.py` already has.

    python rust/parity/ollama_stub.py <port> [--fail-embed]

``--fail-embed`` answers 500 to every embed call while still answering the
probe, which is how the "reachable daemon, no model" leg (`0 embedded — …`) is
reached without uninstalling anything.
"""
from __future__ import annotations

import json
import struct
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

DIM = 16
FAIL_EMBED = False


def vector_for(prompt: str) -> list[float]:
    """A 16-float FNV-1a walk over the prompt's UTF-8 bytes.

    Deterministic, dependency-free, and identical across processes and
    machines. Values land in [-1, 1) so they survive the float32 round trip
    `EmbeddingStore._pack` performs without saturating.
    """
    h = 0xCBF29CE484222325
    out: list[float] = []
    data = prompt.encode("utf-8")
    for index in range(DIM):
        for byte in data + bytes([index]):
            h ^= byte
            h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
        # Take 24 bits and map to [-1, 1); float32-exact, so pack/unpack is
        # lossless and a blob comparison is a comparison of the vector.
        raw = (h >> 8) & 0xFFFFFF
        out.append(struct.unpack("<f", struct.pack("<f", raw / 0x800000 - 1.0))[0])
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):  # noqa: D102 — silence the access log
        return

    def _send(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802
        if self.path.startswith("/api/tags"):
            self._send(200, {"models": []})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        if not self.path.startswith("/api/embeddings"):
            self._send(404, {"error": "not found"})
            return
        if FAIL_EMBED:
            self._send(500, {"error": "model not found"})
            return
        try:
            prompt = json.loads(raw or b"{}").get("prompt") or ""
        except json.JSONDecodeError:
            self._send(400, {"error": "bad json"})
            return
        self._send(200, {"embedding": vector_for(str(prompt))})


def main(argv: list[str]) -> int:
    global FAIL_EMBED  # noqa: PLW0603 — one flag, one process
    if len(argv) < 2:
        print(__doc__)
        return 2
    port = int(argv[1])
    FAIL_EMBED = "--fail-embed" in argv[2:]
    server = HTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

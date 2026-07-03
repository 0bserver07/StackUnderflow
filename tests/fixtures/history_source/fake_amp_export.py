#!/usr/bin/env python3
"""A fake ``amp-export`` command emitting a ``stackunderflow-history-jsonl-v1``
stream, used to exercise the external history-source import contract (#12).

It stands in for a real cloud-gated export tool: it streams our JSONL to
stdout, resumes from the opaque cursor handed to it via
``STACKUNDERFLOW_HISTORY_CURSOR``, and reports a new cursor at the end.

Behaviour is parameterised so one script drives every test scenario:

    --mode ok         (default) a session + messages + a file_touch + cursor
    --mode nocursor   the same records but no trailing cursor record
    --mode empty      only a cursor record (nothing to ingest)
    --mode malformed  one valid line, then a broken JSON line (fail-closed)
    --mode fail       write to stderr and exit non-zero (fail-closed)
    --log PATH        append one JSON line per invocation recording what env
                      the child actually saw (cursor + a couple of vars), so a
                      test can assert cursor replay and the env allowlist
                      without polluting the stream itself.

The emitted stream content is FIXED (independent of the incoming cursor) so a
re-run is a byte-identical no-op — that is what makes the idempotency test
meaningful. Deliberately stdlib-only and free of any stackunderflow import.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

# A fixed absolute path so the file_touch is matchable by
# find_sessions_touching_file after path resolution. Under /Users (not a
# symlink on macOS, non-existent on Linux) so Path.resolve() leaves it
# unchanged on both — unlike /tmp, which macOS resolves to /private/tmp.
TOUCHED_PATH = "/Users/fixture/billing-service/service.py"

# A fixed cursor, NOT derived from the incoming one, so re-running with the
# stored cursor reproduces the identical stream + cursor.
FINAL_CURSOR = "amp-cursor-0002"


def _emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")


def _records() -> None:
    _emit({
        "type": "session",
        "session_id": "amp-sess-1",
        "project": "billing-service",
        "cwd": "/work/billing-service",
        "title": "investigate retry storm",
        "first_timestamp": "2026-06-01T10:00:00+00:00",
        "last_timestamp": "2026-06-01T10:05:00+00:00",
    })
    _emit({
        "type": "message",
        "session_id": "amp-sess-1",
        "seq": 0,
        "timestamp": "2026-06-01T10:00:00+00:00",
        "role": "user",
        "content": "The retry loop in service.py hammers the billing API.",
    })
    _emit({
        "type": "message",
        "session_id": "amp-sess-1",
        "seq": 1,
        "timestamp": "2026-06-01T10:01:00+00:00",
        "role": "assistant",
        "content": "I'll add exponential backoff to the retry loop.",
        "model": "amp-large",
        "input_tokens": 1200,
        "output_tokens": 340,
        "cache_read_tokens": 100,
        "cache_creation_tokens": 0,
        "tools": ["Edit"],
    })
    _emit({
        "type": "file_touch",
        "session_id": "amp-sess-1",
        "seq": 2,
        "path": TOUCHED_PATH,
        "operation": "edit",
        "timestamp": "2026-06-01T10:02:00+00:00",
    })


def _log_invocation(log_path: str, mode: str) -> None:
    entry = {
        "mode": mode,
        "cursor_in": os.environ.get("STACKUNDERFLOW_HISTORY_CURSOR"),
        # Recorded so a test can prove the env allowlist works: a var listed in
        # the manifest's env_passthrough is visible, one that isn't is dropped.
        "passthrough_token": os.environ.get("FAKE_EXPORT_TOKEN"),
        "dropped_var": os.environ.get("FAKE_EXPORT_SHOULD_BE_DROPPED"),
    }
    with open(log_path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry) + "\n")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", default="ok")
    parser.add_argument("--log", default=None)
    args = parser.parse_args(argv)

    if args.log:
        _log_invocation(args.log, args.mode)

    if args.mode == "fail":
        sys.stderr.write("fake amp-export: simulated upstream failure\n")
        return 3

    if args.mode == "malformed":
        _emit({
            "type": "message",
            "session_id": "amp-sess-1",
            "seq": 0,
            "timestamp": "2026-06-01T10:00:00+00:00",
            "role": "user",
            "content": "first line is fine",
        })
        sys.stdout.write("{ this is not valid json \n")
        return 0

    if args.mode == "empty":
        _emit({"type": "cursor", "cursor": FINAL_CURSOR})
        return 0

    # ok / nocursor
    _records()
    if args.mode != "nocursor":
        _emit({"type": "cursor", "cursor": FINAL_CURSOR})
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

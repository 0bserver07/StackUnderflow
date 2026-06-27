#!/usr/bin/env python3
"""Cross-platform discovery smoke for CI.

Proves a wheel-installed StackUnderflow can, on whatever OS the runner is:

1. resolve a Claude config dir (here via ``CLAUDE_CONFIG_DIR``),
2. enumerate a session file under it, and
3. parse that file into records.

This exercises the OS-specific path + JSONL-streaming code that the Linux-only
pytest matrix can't fully cover on Windows. It needs no store/ingest — it
drives the adapter layer directly, which is exactly the path code the Windows
gaps concern. Exits non-zero on any failure so CI fails loudly.

Run: ``python scripts/smoke_discovery.py``
"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

# A minimal but realistic per-project JSONL: one user turn + one assistant
# turn with usage, so read() has a parseable assistant record to emit.
_FIXTURE_LINES = [
    (
        '{"type": "user", "message": {"role": "user", "content": "hello"}, '
        '"uuid": "u1", "sessionId": "smoke", "cwd": "/tmp/x", '
        '"timestamp": "2026-01-01T00:00:00.000Z"}'
    ),
    (
        '{"type": "assistant", "message": {"role": "assistant", '
        '"model": "claude-sonnet-4-20250514", "content": [{"type": "text", '
        '"text": "hi"}], "usage": {"input_tokens": 5, "output_tokens": 2}}, '
        '"uuid": "a1", "parentUuid": "u1", "sessionId": "smoke", '
        '"cwd": "/tmp/x", "timestamp": "2026-01-01T00:00:01.000Z"}'
    ),
]


def main() -> int:
    with tempfile.TemporaryDirectory() as td:
        config_dir = Path(td) / ".claude"
        project = config_dir / "projects" / "-smoke-project"
        project.mkdir(parents=True)
        (project / "0001.jsonl").write_text("\n".join(_FIXTURE_LINES) + "\n", encoding="utf-8")
        os.environ["CLAUDE_CONFIG_DIR"] = str(config_dir)

        from stackunderflow.adapters.claude import ClaudeAdapter, _claude_home

        if _claude_home() != config_dir:
            print(
                f"FAIL: CLAUDE_CONFIG_DIR not honored: {_claude_home()} != {config_dir}",
                file=sys.stderr,
            )
            return 1

        adapter = ClaudeAdapter()
        refs = list(adapter.enumerate())
        slugs = {r.project_slug for r in refs}
        if "-smoke-project" not in slugs:
            print(f"FAIL: discovery missed the smoke project: {slugs}", file=sys.stderr)
            return 1

        records = list(adapter.read(refs[0]))
        if not records:
            print("FAIL: parsing yielded no records", file=sys.stderr)
            return 1

    print(
        f"OK [{sys.platform}]: discovered {len(refs)} session(s), parsed {len(records)} record(s) via CLAUDE_CONFIG_DIR"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

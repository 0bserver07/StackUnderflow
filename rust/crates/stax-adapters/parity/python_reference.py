"""The Python side of the wave-2 adapter parity proof.

Drives the *reference* implementation — `stackunderflow/adapters/claude.py` and
`codex.py` — over the same roots and the same fixture bytes as the Rust
`stax-adapter-parity` binary, and prints byte-identical output. A parity run is
therefore a `diff` of two commands rather than an argument about semantics.

Run it with the campaign's interpreter and the worktree on `PYTHONPATH`, so the
Python being measured is the Python that was ported:

    PYTHONPATH=<worktree> ../StackUnderflow/.venv/bin/python \\
        rust/crates/stax-adapters/parity/python_reference.py counts

Verbs and options mirror the binary exactly:

    counts                     one `<provider>\\t<count>` line per adapter
    refs <provider>            one canonical JSON line per SessionRef
    records <provider>         one canonical JSON line per Record
    capabilities               one line per `capabilities.json` row, as loaded
    --claude-home <path>       injected as CLAUDE_CONFIG_DIR
    --codex-root <path>        injected as CodexAdapter(sessions_root=...)
    --since-offset <n>         resume watermark for `records`
    --session <id>             restrict `records` to one session id

Read-only: nothing here writes, so it is safe against the live `~/.claude`.
"""

from __future__ import annotations

import argparse
import json
import os
import sys


def _adapters(args: argparse.Namespace):
    """Build both adapters with the roots the caller injected."""
    if args.claude_home:
        # The adapter reads CLAUDE_CONFIG_DIR inside `_claude_home()` on every
        # call, so setting it here is the same injection the Rust side does
        # through `ClaudeAdapter::with_env`.
        os.environ["CLAUDE_CONFIG_DIR"] = args.claude_home

    from stackunderflow.adapters.claude import ClaudeAdapter
    from stackunderflow.adapters.codex import CodexAdapter

    claude = ClaudeAdapter()
    codex = (
        CodexAdapter(sessions_root=__import__("pathlib").Path(args.codex_root))
        if args.codex_root
        else CodexAdapter()
    )
    return {"claude": claude, "codex": codex}


def _dumps(obj) -> str:
    """Compact JSON, non-ASCII kept verbatim — matches `serde_json::to_string`."""
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


def _ref_line(ref) -> str:
    return _dumps(
        {
            "provider": ref.provider,
            "project_slug": ref.project_slug,
            "session_id": ref.session_id,
            "file_path": str(ref.file_path),
            # Emitted as Python's repr rather than as a JSON float: the two
            # encoders are free to render 1.7e9 differently, and the point of
            # this field is the number, not the encoder.
            "file_mtime": repr(ref.file_mtime),
            "file_size": ref.file_size,
            "source_kind": ref.source_kind,
            "source_hint": ref.source_hint,
        }
    )


def _record_line(rec) -> str:
    return _dumps(
        {
            "provider": rec.provider,
            "session_id": rec.session_id,
            "seq": rec.seq,
            "timestamp": rec.timestamp,
            "role": rec.role,
            "model": rec.model,
            "input_tokens": rec.input_tokens,
            "output_tokens": rec.output_tokens,
            "cache_create_tokens": rec.cache_create_tokens,
            "cache_read_tokens": rec.cache_read_tokens,
            "content_text": rec.content_text,
            "tools": list(rec.tools),
            "cwd": rec.cwd,
            "is_sidechain": rec.is_sidechain,
            "uuid": rec.uuid,
            "parent_uuid": rec.parent_uuid,
            "speed": rec.speed,
            # Exactly what `ingest/writer.py` stores in `messages.raw_json`,
            # key order included.
            "raw": _dumps(rec.raw),
        }
    )


def _capabilities_lines() -> list[str]:
    """One line per curated row, straight out of the loader being ported.

    Reads `services/support_matrix.py:_CAPABILITIES` — the validated table, not
    the raw JSON — so defaults (`emits_usage_events` true, unset fields `none`)
    are compared, not just the file's literal contents. With the worktree on
    PYTHONPATH, `importlib.resources` resolves to the very same
    `stackunderflow/adapters/capabilities.json` the Rust loader is handed.
    """
    from stackunderflow.services.support_matrix import _CAPABILITIES

    lines = []
    for provider in sorted(_CAPABILITIES):
        cap = _CAPABILITIES[provider]
        resume = cap["resume"]
        fields = ",".join(f"{k}:{v}" for k, v in cap["fields"].items())
        lines.append(
            "\t".join(
                [
                    provider,
                    cap["label"],
                    cap["status"],
                    "true" if cap["emits_usage_events"] else "false",
                    resume["scope"] if resume else "-",
                    resume["command"] if resume else "-",
                    fields,
                ]
            )
        )
    return lines


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(add_help=True)
    parser.add_argument("verb", choices=("counts", "refs", "records", "capabilities"))
    parser.add_argument("provider", nargs="?")
    parser.add_argument("--claude-home")
    parser.add_argument("--codex-root")
    parser.add_argument("--since-offset", type=int, default=0)
    parser.add_argument("--session")
    args = parser.parse_args(argv)

    if args.verb == "capabilities":
        for line in _capabilities_lines():
            sys.stdout.write(line + "\n")
        return 0

    adapters = _adapters(args)

    if args.verb == "counts":
        for name in ("claude", "codex"):
            sys.stdout.write(f"{name}\t{sum(1 for _ in adapters[name].enumerate())}\n")
        return 0

    if not args.provider:
        parser.error(f"`{args.verb}` needs a provider")
    adapter = adapters.get(args.provider)
    if adapter is None:
        parser.error(f"unknown provider {args.provider!r}")

    refs = sorted(
        adapter.enumerate(),
        key=lambda r: (r.project_slug, r.session_id, str(r.file_path)),
    )
    if args.session:
        refs = [r for r in refs if r.session_id == args.session]

    if args.verb == "refs":
        for ref in refs:
            sys.stdout.write(_ref_line(ref) + "\n")
        return 0

    for ref in refs:
        for rec in adapter.read(ref, since_offset=args.since_offset):
            sys.stdout.write(_record_line(rec) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

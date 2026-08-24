"""The Python side of the wave-2 adapter parity proof.

Drives the *reference* implementation — `python-legacy: adapters/claude.py` and
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
    --cline-root <path>        injected as ClineAdapter(tasks_root=...)
    --kilocode-root <path>     injected as KiloCodeAdapter(tasks_root=...)
    --roocode-root <path>      injected as RooCodeAdapter(tasks_root=...)
    --cursor-db <path>         injected as CursorAdapter(vscdb_path=...)
    --gemini-root <path>       injected as GeminiAdapter(projects_root=...)
    --grok-root <path>         injected as GrokAdapter(sessions_root=...)
    --qwen-root <path>         injected as QwenAdapter(projects_root=...)
    --antigravity-home <path>  injected as AntigravityAdapter(gemini_home=...)
    --continue-root <path>     injected as ContinueAdapter(root=...)
    --copilot-legacy <path>    injected as CopilotAdapter(legacy_root=...)
    --copilot-vscode <path>    injected as CopilotAdapter(vscode_workspace_storage=...)
    --droid-root <path>        injected as DroidAdapter(sessions_root=...)
    --kiro-root <path>         injected as KiroAdapter(storage_root=...)
    --openclaw-base <path>     injected as OpenClawAdapter(base_dirs=[...])
    --opencode-root <path>     injected as OpenCodeAdapter(data_dir=...)
    --pi-root <path>           injected as PiAdapter(roots=[(..., "pi")])
    --omp-root <path>          the OMP half of the same PiAdapter
    --codeium-root <path>      injected as CodeiumAdapter(root=...)
    --cursor-agent-root <path> injected as CursorAgentAdapter(projects_root=...)
    --cursor-agent-db <path>   injected as CursorAgentAdapter(tracking_db=...)
    --hermes-root <path>       injected as HermesAdapter(roots=[...])
    --since-offset <n>         resume watermark for `records`
    --session <id>             restrict `records` to one session id
    --blank-timestamps         replace every record timestamp with `<now>`

Read-only: nothing here writes, so it is safe against the live `~/.claude`.

## `--blank-timestamps`, and why it is not cheating

Exactly one provider — `cursor-agent` — stamps `datetime.now(tz=UTC)` on every
record, because its source records no per-message time at all. Two processes
never agree on that microsecond, so a byte diff of the field would be a coin
flip. The flag replaces it with a literal `<now>` on **both** sides, so every
other field of every record is still compared byte for byte, and the excluded
field is named in the output rather than quietly normalised. The clock itself is
pinned on the Rust side by unit test with an injected `pytime::Clock`.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile


# The providers `counts` reports, in the registry's order — mirrored exactly by
# the Rust binary, so the two `counts` outputs are diffable line for line.
_COUNT_PROVIDERS = (
    "antigravity",
    "claude",
    "cline",
    "kilocode",
    "roocode",
    "codeium",
    "codex",
    "continue",
    "copilot",
    "cursor",
    "cursor-agent",
    "droid",
    "gemini",
    "grok",
    "hermes",
    "kiro",
    "openclaw",
    "opencode",
    "pi",
    "qwen",
)


def _adapters(args: argparse.Namespace):
    """Build every adapter with the roots the caller injected."""
    import pathlib

    if args.claude_home:
        # The adapter reads CLAUDE_CONFIG_DIR inside `_claude_home()` on every
        # call, so setting it here is the same injection the Rust side does
        # through `ClaudeAdapter::with_env`.
        os.environ["CLAUDE_CONFIG_DIR"] = args.claude_home
    if args.cursor_db:
        # The Cursor adapter writes a fingerprint cache under `app_dir()` on
        # every full read. Redirect the whole data directory into a throwaway
        # so a parity run never touches the developer's real cache — the Rust
        # port has no cache at all (see the DIVERGENCE note in `cursor.rs`).
        os.environ.setdefault(
            "STACKUNDERFLOW_HOME", tempfile.mkdtemp(prefix="stax-parity-home-")
        )

    from stackunderflow.adapters.antigravity import AntigravityAdapter
    from stackunderflow.adapters.claude import ClaudeAdapter
    from stackunderflow.adapters.cline import (
        ClineAdapter,
        KiloCodeAdapter,
        RooCodeAdapter,
    )
    from stackunderflow.adapters.codeium import CodeiumAdapter
    from stackunderflow.adapters.codex import CodexAdapter
    from stackunderflow.adapters.continue_adapter import ContinueAdapter
    from stackunderflow.adapters.copilot import CopilotAdapter
    from stackunderflow.adapters.cursor import CursorAdapter
    from stackunderflow.adapters.cursor_agent import CursorAgentAdapter
    from stackunderflow.adapters.droid import DroidAdapter
    from stackunderflow.adapters.gemini import GeminiAdapter
    from stackunderflow.adapters.grok import GrokAdapter
    from stackunderflow.adapters.hermes import HermesAdapter
    from stackunderflow.adapters.kiro import KiroAdapter
    from stackunderflow.adapters.openclaw import OpenClawAdapter
    from stackunderflow.adapters.opencode import OpenCodeAdapter
    from stackunderflow.adapters.pi import PiAdapter
    from stackunderflow.adapters.qwen import QwenAdapter

    def _path(value):
        return pathlib.Path(value) if value else None

    def _pi_roots():
        """`(root, label)` pairs, only for the roots the caller injected.

        Passing `roots=None` would scan the developer's real `~/.pi` and
        `~/.omp`; passing an explicit list — even an empty one — keeps a parity
        run hermetic. The labels are the adapter's own, and they matter: they
        prefix `project_slug`.
        """
        pairs = []
        if args.pi_root:
            pairs.append((pathlib.Path(args.pi_root), "pi"))
        if args.omp_root:
            pairs.append((pathlib.Path(args.omp_root), "omp"))
        return pairs or None

    return {
        "antigravity": (
            AntigravityAdapter(gemini_home=_path(args.antigravity_home))
            if args.antigravity_home
            else AntigravityAdapter()
        ),
        "claude": ClaudeAdapter(),
        "cline": ClineAdapter(tasks_root=_path(args.cline_root)),
        "kilocode": KiloCodeAdapter(tasks_root=_path(args.kilocode_root)),
        "roocode": RooCodeAdapter(tasks_root=_path(args.roocode_root)),
        # Registered-but-inert: the root is injectable so a parity run can prove
        # that a *populated* tree still enumerates nothing.
        "codeium": CodeiumAdapter(root=_path(args.codeium_root)),
        "codex": (
            CodexAdapter(sessions_root=pathlib.Path(args.codex_root))
            if args.codex_root
            else CodexAdapter()
        ),
        "continue": ContinueAdapter(root=_path(args.continue_root)),
        "copilot": CopilotAdapter(
            legacy_root=_path(args.copilot_legacy),
            vscode_workspace_storage=_path(args.copilot_vscode),
        ),
        "cursor": CursorAdapter(vscdb_path=_path(args.cursor_db)),
        # Both paths default independently, so a hermetic run injects both:
        # passing only the projects root would still read the developer's real
        # `~/.cursor/ai-tracking/ai-code-tracking.db` for the model.
        "cursor-agent": CursorAgentAdapter(
            projects_root=_path(args.cursor_agent_root),
            tracking_db=_path(args.cursor_agent_db),
        ),
        "droid": DroidAdapter(sessions_root=_path(args.droid_root)),
        "gemini": GeminiAdapter(projects_root=_path(args.gemini_root)),
        "grok": GrokAdapter(sessions_root=_path(args.grok_root)),
        "hermes": HermesAdapter(
            roots=[pathlib.Path(args.hermes_root)] if args.hermes_root else None
        ),
        "kiro": KiroAdapter(storage_root=_path(args.kiro_root)),
        "openclaw": OpenClawAdapter(
            base_dirs=[pathlib.Path(args.openclaw_base)] if args.openclaw_base else None
        ),
        "opencode": OpenCodeAdapter(data_dir=_path(args.opencode_root)),
        "pi": PiAdapter(roots=_pi_roots()),
        "qwen": QwenAdapter(projects_root=_path(args.qwen_root)),
    }


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


def _record_line(rec, *, blank_timestamp: bool = False) -> str:
    return _dumps(
        {
            "provider": rec.provider,
            "session_id": rec.session_id,
            "seq": rec.seq,
            # See `--blank-timestamps` in the module docstring: the one field
            # two processes cannot agree on, excluded by name rather than
            # normalised in silence.
            "timestamp": "<now>" if blank_timestamp else rec.timestamp,
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
    parser.add_argument("--cline-root")
    parser.add_argument("--kilocode-root")
    parser.add_argument("--roocode-root")
    parser.add_argument("--cursor-db")
    parser.add_argument("--gemini-root")
    parser.add_argument("--grok-root")
    parser.add_argument("--qwen-root")
    parser.add_argument("--antigravity-home")
    parser.add_argument("--continue-root")
    parser.add_argument("--copilot-legacy")
    parser.add_argument("--copilot-vscode")
    parser.add_argument("--droid-root")
    parser.add_argument("--kiro-root")
    parser.add_argument("--openclaw-base")
    parser.add_argument("--opencode-root")
    parser.add_argument("--pi-root")
    parser.add_argument("--omp-root")
    parser.add_argument("--codeium-root")
    parser.add_argument("--cursor-agent-root")
    parser.add_argument("--cursor-agent-db")
    parser.add_argument("--hermes-root")
    parser.add_argument("--since-offset", type=int, default=0)
    parser.add_argument("--session")
    parser.add_argument("--blank-timestamps", action="store_true")
    args = parser.parse_args(argv)

    if args.verb == "capabilities":
        for line in _capabilities_lines():
            sys.stdout.write(line + "\n")
        return 0

    adapters = _adapters(args)

    if args.verb == "counts":
        for name in _COUNT_PROVIDERS:
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
            sys.stdout.write(
                _record_line(rec, blank_timestamp=args.blank_timestamps) + "\n"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

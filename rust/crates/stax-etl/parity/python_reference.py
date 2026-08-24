#!/usr/bin/env python
"""Drive the REFERENCE normalizers so the Rust port can be diffed against them.

Never imports anything from the Rust side and never writes outside the store
path it is handed. The store path is always a COPY — the campaign's live
dataset is read-only (`docs/specs/rust-port.md` §5) and this script's `pass`
subcommand deletes `usage_events` before rebuilding it.

Subcommands
-----------
``registry``            one ``key<TAB>module.Class`` line per registered
                        provider, in registration order. The table in
                        ``normalize/mod.rs`` is diffed against this — Rust has
                        no import-time reflection, so the self-discovering walk
                        cannot be ported and is *checked* instead.
``pass STORE``          ``DELETE FROM usage_events`` then run
                        ``etl.backfill._run_normalizers`` — the events half of
                        ``backfill(force=True)``. The marts are deliberately
                        untouched: this harness diffs events, and
                        ``rebuild_from_scratch`` is twenty minutes of work
                        nothing here reads.
``dump STORE``          one line per ``usage_events`` row, ordered by the unique
                        key, with ``cost_usd`` as IEEE-754 bits *and* repr.
``counts STORE``        per (provider, cost_source) rollup.
``seams``               report the pricing seams this process is running with —
                        the DIV-016 pin. A diff that ran with a primed price
                        book or a live overlay would be measuring something
                        other than the ETL path.
``fixture PROVIDER``    fixture pack -> adapter -> normalizer -> events, in the
                        ``dump`` line format. Mirrors
                        ``tests/python-legacy: etl/normalize/test_beta_normalizers.py``.
"""

from __future__ import annotations

import json
import shutil
import sqlite3
import struct
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]


# ── shared row rendering ────────────────────────────────────────────────────

_NULL = "\\N"


def _bits(value: float) -> str:
    return f"0x{struct.unpack('<Q', struct.pack('<d', value))[0]:016x}"


def _event_line(row: dict) -> str:
    cost = float(row["cost_usd"])
    extras = row["raw_extras"]
    return "\t".join(
        [
            str(row["source_message_fk"]),
            str(row["provider"]),
            str(row["account"]),
            str(row["project_id"]),
            str(row["session_id"]),
            str(row["ts"]),
            str(row["day"]),
            str(row["model"]),
            str(row["speed"]),
            str(row["input_tokens"]),
            str(row["output_tokens"]),
            str(row["cache_read_tokens"]),
            str(row["cache_create_tokens"]),
            str(row["reasoning_tokens"]),
            _bits(cost),
            repr(cost),
            str(row["cost_source"]),
            str(row["role"]),
            _NULL if extras is None else str(extras),
        ]
    )


_DUMP_SQL = """
    SELECT source_message_fk, provider, account, project_id, session_id,
           ts, day, model, speed, input_tokens, output_tokens,
           cache_read_tokens, cache_create_tokens, reasoning_tokens,
           cost_usd, cost_source, role, raw_extras
      FROM usage_events
     ORDER BY source_message_fk
"""


# ── subcommands ─────────────────────────────────────────────────────────────


def cmd_registry() -> int:
    from stackunderflow.etl import normalize

    for key, cls in normalize.all().items():
        print(f"{key}\t{cls.__module__}.{cls.__name__}")
    return 0


def cmd_seams() -> int:
    """The DIV-016 pin, from the Python side.

    ``etl backfill`` never calls ``use_price_book_store``; only ``server.py``
    does. If that ever changes, or if a LiteLLM overlay lands in the cache dir,
    the dollars in a parity diff stop being the ETL path's dollars — so the
    state is asserted, printed, and compared rather than assumed.
    """
    from stackunderflow.infra import costs, model_manifest

    overlay = costs._load_overlay()
    print(f"price_book_wired\t{bool(model_manifest._use_store)}")
    print(f"price_book_cache\t{'primed' if model_manifest._book_cache is not None else 'unprimed'}")
    print(f"overlay_entries\t{len(overlay)}")
    print(f"model_aliases\t{len(costs._user_aliases())}")
    print(f"rate_card_ids\t{len(costs.RATE_CARD)}")
    return 0


def cmd_pass(store: str) -> int:
    # `stackunderflow.etl.backfill` the ATTRIBUTE is the re-exported function
    # (`etl/__init__.py:20`), not the module, so `import … as backfill` binds
    # the wrong object. Import the symbol directly.
    from stackunderflow.etl.backfill import _run_normalizers
    from stackunderflow.etl.normalize import all as all_normalizers

    conn = sqlite3.connect(store)
    conn.row_factory = sqlite3.Row
    try:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.execute("DELETE FROM usage_events")
        conn.commit()
        import time

        started = time.perf_counter()
        inserted, skipped, seen = _run_normalizers(conn, all_normalizers())
        elapsed = time.perf_counter() - started
        conn.commit()
    finally:
        conn.close()
    print(
        f"events_inserted={inserted} events_skipped_duplicate={skipped} "
        f"messages_seen={seen} rows_raised=? seconds={elapsed:.3f}"
    )
    return 0


def cmd_dump(store: str) -> int:
    conn = sqlite3.connect(f"file:{store}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        out = sys.stdout
        for row in conn.execute(_DUMP_SQL):
            out.write(_event_line(dict(row)) + "\n")
    finally:
        conn.close()
    return 0


def cmd_counts(store: str) -> int:
    conn = sqlite3.connect(f"file:{store}?mode=ro", uri=True)
    try:
        rows = conn.execute(
            """
            SELECT provider, cost_source, COUNT(*), SUM(cost_usd),
                   SUM(input_tokens), SUM(output_tokens),
                   SUM(cache_read_tokens), SUM(cache_create_tokens),
                   SUM(reasoning_tokens)
              FROM usage_events
             GROUP BY provider, cost_source
             ORDER BY provider, cost_source
            """
        ).fetchall()
    finally:
        conn.close()
    for provider, source, count, total, i, o, cr, cc, rt in rows:
        total = float(total or 0.0)
        print(
            "\t".join(
                [provider, source, str(count), _bits(total), repr(total),
                 str(i), str(o), str(cr), str(cc), str(rt)]
            )
        )
    return 0


# ── fixture-pack pipeline (mirrors the Python suite's own harness) ──────────


def _record_to_msg_row(rec, *, msg_id: int, project_id: int, provider: str) -> dict:
    """The columns ``backfill._run_normalizers`` selects, built from a Record."""
    return {
        "id": msg_id,
        "session_fk": 1,
        "seq": rec.seq,
        "timestamp": rec.timestamp,
        "role": rec.role,
        "model": rec.model,
        "input_tokens": rec.input_tokens,
        "output_tokens": rec.output_tokens,
        "cache_read_tokens": rec.cache_read_tokens,
        "cache_create_tokens": rec.cache_create_tokens,
        "content_text": rec.content_text,
        "tools_json": json.dumps(list(rec.tools)),
        "raw_json": json.dumps(rec.raw, default=str),
        "is_sidechain": int(rec.is_sidechain),
        "uuid": rec.uuid,
        "parent_uuid": rec.parent_uuid,
        "speed": rec.speed,
        "session_id": rec.session_id,
        "project_id": project_id,
        "provider": provider,
    }


def _event_to_row(event: dict) -> dict:
    """Fill the writer's defaults so a yielded event renders like a stored one."""
    filled = dict(event)
    filled.setdefault("reasoning_tokens", 0)
    filled["raw_extras"] = event.get("raw_extras")
    return filled


def cmd_fixture(provider: str, layout_root: str) -> int:
    """Run one fixture pack end to end, against a layout the CALLER built.

    The Rust harness lays the pack out under a temp root and hands that root
    here, so both implementations parse the identical bytes on disk rather than
    two copies of a fixture.
    """
    from stackunderflow.etl.normalize import get as get_normalizer

    root = Path(layout_root)
    adapter = _build_adapter(provider, root)
    normalizer_cls = get_normalizer(provider)
    if normalizer_cls is None:
        print(f"no normalizer registered for {provider!r}", file=sys.stderr)
        return 1
    normalizer = normalizer_cls()

    next_id = 1
    for ref in adapter.enumerate():
        for rec in adapter.read(ref):
            msg_row = _record_to_msg_row(
                rec, msg_id=next_id, project_id=42, provider=provider
            )
            next_id += 1
            for event in normalizer.normalize(msg_row):
                print(_event_line(_event_to_row(event)))
    return 0


def _build_adapter(provider: str, root: Path):
    """Point the reference adapter at the layout the Rust harness built."""
    if provider == "codex":
        from stackunderflow.adapters.codex import CodexAdapter

        return CodexAdapter(sessions_root=root / "codex")
    if provider == "cursor-agent":
        from stackunderflow.adapters.cursor_agent import CursorAgentAdapter

        return CursorAgentAdapter(
            projects_root=root / "projects", tracking_db=root / "missing.db"
        )
    if provider == "opencode":
        from stackunderflow.adapters.opencode import OpenCodeAdapter

        return OpenCodeAdapter(data_dir=root / "opencode-data")
    if provider == "qwen":
        from stackunderflow.adapters.qwen import QwenAdapter

        return QwenAdapter(projects_root=root / "qwen-projects")
    if provider == "gemini":
        from stackunderflow.adapters.gemini import GeminiAdapter

        return GeminiAdapter(projects_root=root / "gemini-tmp")
    if provider == "copilot":
        from stackunderflow.adapters.copilot import CopilotAdapter

        return CopilotAdapter(
            legacy_root=root / "copilot-legacy",
            vscode_workspace_storage=root / "missing-vscode-storage",
        )
    if provider == "codeium":
        from stackunderflow.adapters.codeium import CodeiumAdapter

        return CodeiumAdapter(root=root / "codeium-empty")
    if provider == "continue":
        from stackunderflow.adapters.continue_adapter import ContinueAdapter

        return ContinueAdapter(root=root / "continue")
    if provider == "droid":
        from stackunderflow.adapters.droid import DroidAdapter

        return DroidAdapter(sessions_root=root / "droid-sessions")
    if provider == "kiro":
        from stackunderflow.adapters.kiro import KiroAdapter

        return KiroAdapter(storage_root=root / "kiro-storage")
    if provider == "openclaw":
        from stackunderflow.adapters.openclaw import OpenClawAdapter

        return OpenClawAdapter(base_dirs=[root / "openclaw-agents"])
    if provider == "pi":
        from stackunderflow.adapters.pi import PiAdapter

        return PiAdapter(roots=[(root / "pi-sessions", "pi")])
    if provider == "kilocode":
        from stackunderflow.adapters.cline import KiloCodeAdapter

        return KiloCodeAdapter(tasks_root=root / "kilocode-tasks")
    if provider == "roocode":
        from stackunderflow.adapters.cline import RooCodeAdapter

        return RooCodeAdapter(tasks_root=root / "roocode-tasks")
    raise SystemExit(f"no fixture adapter wiring for {provider!r}")


def cmd_layout(provider: str, root: str) -> int:
    """Build the on-disk layout each adapter expects, from the checked-in pack.

    Extracted from ``test_beta_normalizers.py``'s scenario builders so both
    implementations point at ONE tree. Two of the packs (`opencode`,
    `continue`) are SQLite *specs* rather than data and are materialised here
    the way the Python suite materialises them.
    """
    packs = REPO_ROOT / "tests" / "fixtures" / "beta_normalizers"
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)

    def place(pack_file: Path, target: Path) -> None:
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(pack_file, target)

    if provider == "codex":
        shutil.copytree(packs / "codex", root / "codex", dirs_exist_ok=True)
    elif provider == "cursor-agent":
        place(
            packs / "cursor_agent" / "transcript.jsonl",
            root
            / "projects"
            / "myproj"
            / "agent-transcripts"
            / "11111111-2222-3333-4444-555555555555"
            / "session.jsonl",
        )
    elif provider == "qwen":
        place(packs / "qwen" / "chat.jsonl",
              root / "qwen-projects" / "myproj" / "chats" / "session-qwen-001.jsonl")
    elif provider == "gemini":
        place(packs / "gemini" / "chat.jsonl",
              root / "gemini-tmp" / "myproj" / "chats" / "session-gemini-001.jsonl")
    elif provider == "copilot":
        place(packs / "copilot" / "events.jsonl",
              root / "copilot-legacy" / "session-001" / "events.jsonl")
    elif provider == "codeium":
        (root / "codeium-empty").mkdir(parents=True, exist_ok=True)
    elif provider == "droid":
        place(packs / "droid" / "session.jsonl",
              root / "droid-sessions" / "projhash-001" / "session.jsonl")
        place(packs / "droid" / "session.settings.json",
              root / "droid-sessions" / "projhash-001" / "session.settings.json")
    elif provider == "kiro":
        place(packs / "kiro" / "chat.chat",
              root / "kiro-storage" / "kiro-workflow-001.chat")
    elif provider == "openclaw":
        place(packs / "openclaw" / "session.jsonl",
              root / "openclaw-agents" / "claw-agent" / "sessions" / "claw-sess-001.jsonl")
    elif provider == "pi":
        place(packs / "pi" / "session.jsonl", root / "pi-sessions" / "pi-sess-001.jsonl")
    elif provider in ("kilocode", "roocode"):
        base = root / f"{provider}-tasks" / "task-001"
        place(packs / provider / "ui_messages.json", base / "ui_messages.json")
        place(packs / provider / "api_conversation_history.json",
              base / "api_conversation_history.json")
    elif provider == "opencode":
        _materialise_opencode(packs / "opencode" / "session.json", root / "opencode-data")
    elif provider == "continue":
        _materialise_continue(packs / "continue" / "session.json", root / "continue")
    else:
        raise SystemExit(f"no fixture layout for {provider!r}")
    return 0


def _materialise_opencode(spec_file: Path, data_dir: Path) -> None:
    spec = json.loads(spec_file.read_text())
    data_dir.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(data_dir / "opencode.db")
    try:
        conn.executescript(
            """
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT,
                                  time_created INTEGER, time_archived INTEGER,
                                  parent_id TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                                  time_created INTEGER, data TEXT);
            CREATE TABLE part (id INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT,
                               session_id TEXT, data TEXT);
            """
        )
        s = spec["session"]
        conn.execute(
            "INSERT INTO session VALUES (?,?,?,?,?,?)",
            (s["id"], s["directory"], s["title"], s["time_created"],
             s["time_archived"], s["parent_id"]),
        )
        for m in spec["messages"]:
            conn.execute(
                "INSERT INTO message VALUES (?,?,?,?)",
                (m["id"], s["id"], m["time_created"], json.dumps(m["data"])),
            )
            for part in m["parts"]:
                conn.execute(
                    "INSERT INTO part(message_id, session_id, data) VALUES (?,?,?)",
                    (m["id"], s["id"], json.dumps(part)),
                )
        conn.commit()
    finally:
        conn.close()


def _materialise_continue(spec_file: Path, root: Path) -> None:
    spec = json.loads(spec_file.read_text())
    root.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(root / "state.db")
    try:
        conn.executescript(
            """
            CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER);
            CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
                                   session_id TEXT, role TEXT, content TEXT,
                                   model TEXT, input_tokens INTEGER,
                                   output_tokens INTEGER, createdAt INTEGER);
            """
        )
        for s in spec["sessions"]:
            conn.execute("INSERT INTO sessions VALUES (?,?,?)",
                         (s["id"], s["title"], s["createdAt"]))
        for m in spec["messages"]:
            conn.execute(
                "INSERT INTO messages(session_id, role, content, model, "
                "input_tokens, output_tokens, createdAt) VALUES (?,?,?,?,?,?,?)",
                (m["session_id"], m["role"], m["content"], m["model"],
                 m["input_tokens"], m["output_tokens"], m["createdAt"]),
            )
        conn.commit()
    finally:
        conn.close()


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    command, *rest = argv
    if command == "registry":
        return cmd_registry()
    if command == "seams":
        return cmd_seams()
    if command == "pass":
        return cmd_pass(rest[0])
    if command == "dump":
        return cmd_dump(rest[0])
    if command == "counts":
        return cmd_counts(rest[0])
    if command == "layout":
        return cmd_layout(rest[0], rest[1])
    if command == "fixture":
        return cmd_fixture(rest[0], rest[1])
    print(f"unknown command {command!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

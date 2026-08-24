#!/usr/bin/env python3
"""Regenerate the rust-campaign-added goldens under this directory.

    ../StackUnderflow/.venv/bin/python \
        rust/crates/stax-memory/tests/goldens/generate.py

Why these exist, and why they are NOT in ``contracts/``:

* The shipped pack (``contracts/stackunderflow-memory-v1/fixtures/``, 15 files)
  covers one query shape per command and every query in it is a single word.
  Findings-ledger #3 of the Rust campaign — *phrase queries silently zero on the
  LIKE path* — says multi-word asks must be pinned by wave-1 fixtures, and they
  were not. ``stackunderflow.resume/1`` had no golden pack at all, only inline
  assertions in ``tests/python-legacy: cli/test_resume_cmd.py``.
* They stay out of ``contracts/`` because the Python suite asserts that pack is
  exactly 15 files, one per command x {success, empty, error}
  (``test_one_fixture_per_command_and_case``). Adding files there would fail
  Python CI. Every file written here is still validated against the SHIPPED
  ``contracts/stackunderflow-memory-v1/schema.json`` by the shipped checker
  before it lands — see the tail of this script.

Every byte is produced by the PYTHON implementation:

* the ``memory/1`` pack goes through ``cli_helpers.agent_output.build_envelope``
  / ``build_error_envelope`` + ``render`` — the exact functions the CLI uses, so
  ``result_count``/``token_estimate``/key order/escaping are Python's answers,
  not a transcription;
* the ``resume/1`` pack runs the real ``stax resume --json`` CLI over
  the seed from ``test_resume_cmd.py``, capturing stdout verbatim.

The trailing newline in each file is ``click.echo``'s, so the files are literal
stdout — the same convention the shipped pack follows.
"""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
# rust/crates/stax-memory/tests/goldens -> repo root
REPO = HERE.parents[4]
sys.path.insert(0, str(REPO))

from click.testing import CliRunner  # noqa: E402

import stackunderflow.deps as deps  # noqa: E402
from stackunderflow.cli import cli  # noqa: E402
from stackunderflow.cli_helpers import agent_output as ao  # noqa: E402
from stackunderflow.store import db, schema  # noqa: E402

ADDED = HERE / "rust-campaign-added"
MEMORY_DIR = ADDED / "memory-v1"
RESUME_DIR = ADDED / "resume-v1"

SLUG = "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow"
PROJECT_PATH = "/Users/yadkonrad/dev/dev/year26/jan26/StackUnderflow"
SINCE_ERROR = (
    "Invalid since value 'notadate': expected '7d'/'1w'/'1m'/'24h' or an ISO "
    "date/datetime."
)


def _row(session_id: str, *, snippet: str | None = None, **extra: object) -> dict:
    """A discovery result row in the shape the shipped goldens carry."""
    row = {
        "session_id": session_id,
        "project_slug": SLUG,
        "project_path": PROJECT_PATH,
        "provider": "claude",
        "first_ts": "2026-06-14T09:12:03.114Z",
        "last_ts": "2026-06-14T11:48:52.507Z",
        "message_count": 412,
        "cost_usd": 18.3372105,
        "snippet": snippet,
    }
    row.update(extra)
    return row


def _write(directory: Path, name: str, envelope: dict) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{name}.json").write_text(ao.render(envelope) + "\n")
    print(f"  rust-campaign-added/{directory.name}/{name}.json")


# ── stackunderflow.memory/1 — the phrase pack ───────────────────────────────


def build_memory_goldens() -> None:
    print("memory/1 (phrase + escaping pack)")

    # 1. A multi-word decisions query that DOES match. The snippet carries the
    #    ellipsis the truncator inserts (U+2026) — ensure_ascii's first victim.
    _write(MEMORY_DIR, "decisions.phrase-hit", ao.build_envelope(
        command="decisions",
        query={
            "text": "why rsync instead of a second copy",
            "project": SLUG,
            "since": None,
            "limit": 2,
        },
        results=[
            _row(
                "9f1c2b7e-4a0d-4f52-9d18-6f1a3c8e5b40",
                snippet="…chose rsync --link-dest for the hardlink incrementals: a second "
                        "full copy costs a disk per week, the hardlink tree costs the delta…",
            ),
            _row(
                "1d4e8a06-3c77-4a91-bb2e-70d9e5c1f832",
                snippet="…decision: keep the LIKE path for `memory decisions` until FTS5 "
                        "lands — the rewrite is wave 6, not a hotfix…",
            ),
        ],
        budget=2000,
        truncated=False,
    ))

    # 2. THE finding-#3 case: the same phrase, zero rows. A multi-word query on
    #    the LIKE path silently returns nothing; the envelope must still be a
    #    well-formed success envelope, not an error.
    _write(MEMORY_DIR, "decisions.phrase-zero", ao.build_envelope(
        command="decisions",
        query={
            "text": "why did we choose rsync over a second full copy",
            "project": SLUG,
            "since": None,
            "limit": 20,
        },
        results=[],
        budget=2000,
        truncated=False,
    ))

    # 3. The same silence through `ask`, which carries the two documented extras.
    _write(MEMORY_DIR, "ask.phrase-zero", ao.build_envelope(
        command="ask",
        query={
            "question": "how does the tiered cache decide what to evict",
            "project": SLUG,
            "since": None,
            "limit": 20,
        },
        results=[],
        budget=2000,
        truncated=False,
        extra={
            "note": "keyword search over past decisions (local semantic vector "
                    "search unavailable — start Ollama to enable it).",
            "vector_used": False,
        },
    ))

    # 4. A phrase with shell operators, and a truncating budget: `truncated`
    #    true with rows present is a combination the shipped pack never shows.
    _write(MEMORY_DIR, "worked.phrase-truncated", ao.build_envelope(
        command="worked",
        query={
            "action": "npm run build && npm run typecheck",
            "project": SLUG,
            "since": None,
            "limit": 20,
        },
        results=[
            _row(
                "b7350d21-9e64-4d0f-84a9-2c5b1e07af93",
                snippet=None,
                outcome="worked",
                outcome_evidence="user wrote: 'build is green — 168 unit tests pass, "
                                 "typecheck clean'",
                outcome_msg_id=418_902,
                outcome_confidence=0.8,
            ),
        ],
        budget=120,
        truncated=True,
    ))

    # 5. A path with spaces and non-ASCII, plus the `risk` extra `memory file`
    #    attaches. Escaping stress on a QUERY value, not just a snippet.
    weird_path = "/Users/yadkonrad/dev/Mes Projets/naïve café/src/résumé_builder.py"
    _write(MEMORY_DIR, "file.unicode-path", ao.build_envelope(
        command="file",
        query={"path": weird_path, "project": None, "since": None, "limit": 2},
        results=[
            _row(
                "3a2f5c88-71bb-4e19-9c07-8ad4e6f21b55",
                snippet=None,
                kind="touched",
            ),
        ],
        budget=2000,
        truncated=False,
        extra={
            "risk": {
                "path": weird_path,
                "since": None,
                "total_sessions": 1,
                "reverted": 0,
                "failed": 1,
                "worked": 3,
                "recent_session_ids": ["3a2f5c88-71bb-4e19-9c07-8ad4e6f21b55"],
            },
        },
    ))

    # 6. A multi-word path for `sessions`, whose query also carries `scope`.
    _write(MEMORY_DIR, "sessions.phrase-path", ao.build_envelope(
        command="sessions",
        query={
            "path": "/Users/yadkonrad/dev/My Work Projects/year 26",
            "project": None,
            "since": None,
            "limit": 2,
            "scope": "path",
        },
        results=[_row("c0ffee00-1111-4222-8333-444455556666", snippet=None)],
        budget=2000,
        truncated=False,
    ))

    # 7. Escaping torture: emoji (a surrogate PAIR under ensure_ascii), CJK,
    #    an ellipsis, a tab, an embedded quote and a Windows path's backslashes.
    _write(MEMORY_DIR, "ask.escaping", ao.build_envelope(
        command="ask",
        query={
            "question": 'why did the 🚀 deploy of "本番" fail on C:\\Users\\dev\\tmp?',
            "project": SLUG,
            "since": None,
            "limit": 1,
        },
        results=[
            _row(
                "7e5d9f31-2c08-4b6a-9f14-0d3a8b6c5e72",
                snippet="…the run died at\tstep 3: `C:\\Users\\dev\\tmp` is not a path "
                        "the 🚀 runner can write — 本番 env only…",
            ),
        ],
        budget=2000,
        truncated=False,
        extra={
            "note": "hybrid retrieval (keyword + local semantic vectors).",
            "vector_used": True,
        },
    ))

    # 8. Float presentation edges in a row: `repr(float)` vs Rust's shortest
    #    round-trip writer disagree on every one of these (1e+16 vs 1e16,
    #    1e-05 vs 1e-5) — and -0.0 must not collapse to 0.0.
    _write(MEMORY_DIR, "decisions.float-edges", ao.build_envelope(
        command="decisions",
        query={"text": "cost accounting rounding", "project": SLUG,
               "since": None, "limit": 6},
        results=[
            {"session_id": "float-0", "cost_usd": 0.0},
            {"session_id": "float-neg-zero", "cost_usd": -0.0},
            {"session_id": "float-long", "cost_usd": 600.7909187500001},
            {"session_id": "float-longer", "cost_usd": 499.25254474999997},
            {"session_id": "float-tiny", "cost_usd": 1e-05},
            {"session_id": "float-huge", "cost_usd": 1e16},
            {"session_id": "float-fixed-edge", "cost_usd": 1e15},
            {"session_id": "float-just-under", "cost_usd": 0.0001},
            {"session_id": "int-big", "message_count": 9223372036854775807},
            {"session_id": "int-negative", "message_count": -1},
        ],
        budget=0,
        truncated=False,
    ))

    # 9. The sixth `command` value. `context-replay` is in the schema's enum and
    #    reuses this envelope (tests/python-legacy: cli/test_context_replay_cli.py
    #    asserts the tag) but the shipped pack has no golden for it at all.
    _write(MEMORY_DIR, "context-replay.success", ao.build_envelope(
        command="context-replay",
        query={
            "session_id": "43da8464-3633-45cc-a2a4-a056daaffb84",
            "around": "the retry decision",
            "limit": 2,
        },
        results=[
            {"role": "user", "ts": "2026-05-01T04:27:10.652Z",
             "text": "let's disable retry for EtlPipelineNotReadyError"},
            {"role": "assistant", "ts": "2026-05-01T04:27:44.918Z",
             "text": "done — React Query `retry` is off for that error class."},
        ],
        budget=2000,
        truncated=False,
    ))

    # 10. An error envelope whose query echoes a multi-word question.
    _write(MEMORY_DIR, "ask.phrase-error", ao.build_error_envelope(
        command="ask",
        query={
            "question": "what did we decide about the mart cost freeze",
            "project": SLUG,
            "since": "notadate",
            "limit": 20,
        },
        error=SINCE_ERROR,
    ))

    # 11. Forward-compat as a GOLDEN, not just a checker phase: an envelope from
    #     a producer newer than this build — an unknown extra AND an unknown row
    #     field. Both implementations must preserve it byte-for-byte.
    _write(MEMORY_DIR, "ask.forward-compat", ao.build_envelope(
        command="ask",
        query={"question": "does the envelope survive a newer producer",
               "project": SLUG, "since": None, "limit": 1},
        results=[_row("f0000000-0000-4000-8000-000000000001", snippet=None,
                      x_future_row_field="ignored, not rejected")],
        budget=2000,
        truncated=False,
        extra={
            "note": "keyword search over past decisions.",
            "vector_used": False,
            "x_future_additive_field": {"added_later": [1, 2, 3]},
        },
    ))


# ── stackunderflow.resume/1 — a pack where none existed ─────────────────────

# The seed from tests/python-legacy: cli/test_resume_cmd.py, verbatim: it is the
# only fixture in the tree that exercises session-scope, latest-scope and
# no-known-resume providers at once.
_WS = [
    ("claude", "-Users-t-my-ws", [("cl-ws-new", "2026-07-01T10:00:00Z", 142)]),
    ("claude", "-Users-t-my-ws-child", [("cl-child-old", "2026-06-19T10:00:00Z", 601)]),
    ("codex", "-Users-t-my-ws-child", [
        ("cx-child-new", "2026-07-08T10:00:00Z", 151),
        ("cx-child-old", "2026-06-26T10:00:00Z", 62),
    ]),
    ("grok", "-Users-t-my-ws-child", [("gr-child", "2026-07-09T10:00:00Z", 96)]),
    ("mystery", "-Users-t-my-ws-child", [("my-child", "2026-05-24T10:00:00Z", 82)]),
    ("claude", "-Users-t", [("cl-home", "2026-05-27T10:00:00Z", 40)]),
    ("claude", "-Users-t-other-proj", [("cl-other", "2026-07-09T10:00:00Z", 10)]),
]


def _seed(store: Path) -> None:
    conn = db.connect(store)
    schema.apply(conn)
    for provider, slug, sessions in _WS:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, "
            "last_modified) VALUES (?, ?, ?, 0.0, 0.0)",
            (provider, slug, slug),
        )
        pid = int(cur.lastrowid or 0)
        for sid, last_ts, count in sessions:
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, "
                "last_ts, message_count) VALUES (?, ?, ?, ?, ?)",
                (pid, sid, last_ts, last_ts, count),
            )
    conn.commit()
    conn.close()


def build_resume_goldens() -> None:
    print("resume/1 (real CLI stdout over the test_resume_cmd seed)")
    RESUME_DIR.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        store = Path(tmp) / "store.db"
        _seed(store)
        original = deps.store_path
        deps.store_path = store
        try:
            cases = {
                # Every template kind in one envelope: claude/codex session-scope,
                # grok latest-scope, mystery with resume: null.
                "resume.workspace": ["resume", "/Users/t/my_ws"],
                # --provider narrowing echoes `provider_filter`.
                "resume.filtered": ["resume", "/Users/t/my_ws", "-p", "codex"],
                # A requested agent with no sessions here adds
                # `unmatched_providers` — both optional keys, in order.
                "resume.unmatched": [
                    "resume", "/Users/t/my_ws", "-p", "codex", "-p", "kiro",
                ],
                # No project anywhere near: `providers` is an empty array.
                "resume.no-sessions": ["resume", "/Elsewhere/entirely"],
                # A path with spaces and non-ASCII, slug-folded — the resume
                # analogue of the memory phrase cases.
                "resume.unicode-path": ["resume", "/Users/t/Mes Projets/naïve café"],
            }
            for name, args in cases.items():
                result = CliRunner().invoke(cli, [*args, "--json"])
                assert result.exit_code == 0, (name, result.output)
                (RESUME_DIR / f"{name}.json").write_text(result.output)
                print(f"  rust-campaign-added/{RESUME_DIR.name}/{name}.json")
        finally:
            deps.store_path = original


# ── validation: the shipped checker, on the shipped schema ──────────────────


def validate_memory_pack() -> int:
    """Run scripts/check_memory_contract.py's validator over the new pack."""
    spec = importlib.util.spec_from_file_location(
        "check_memory_contract", REPO / "scripts" / "check_memory_contract.py"
    )
    assert spec is not None and spec.loader is not None
    checker = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checker)
    schema_doc = checker.load_schema()

    problems: list[str] = []
    for path in sorted(MEMORY_DIR.glob("*.json")):
        instance = json.loads(path.read_text())
        problems += [f"{path.name}: {e}"
                     for e in checker.validate(instance, schema_doc, schema_doc)]
    if problems:
        print("FAIL: campaign-added memory goldens do not conform:")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print(f"OK: {len(list(MEMORY_DIR.glob('*.json')))} campaign-added memory "
          f"golden(s) conform to the shipped schema.json")
    return 0


def main() -> int:
    print(f"repo root: {REPO}")
    print(f"sqlite:    {sqlite3.sqlite_version}")
    build_memory_goldens()
    build_resume_goldens()
    return validate_memory_pack()


if __name__ == "__main__":
    sys.exit(main())

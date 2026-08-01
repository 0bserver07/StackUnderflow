#!/usr/bin/env python3
"""Build the SYNTHETIC $STACKUNDERFLOW_HOME the hook differ runs against.

Why synthetic rather than the shared `.parity-state/fresh` real store:

1. **The real store cannot exercise two of the nine hooks.** `tools_json` on
   the maintainer's data is a names-only array (`["Bash","Bash"]`) — the
   arguments are not stored — and `find_failure_modes_for_file` matches on
   `tools_json LIKE '%<path>%'`. So `stackunderflow-inject-pre-tool-use` and
   the whole recall path return `""` on every real input. A differ that only
   proves "both sides say nothing" proves nothing.
2. **The capture hooks WRITE.** Four of the nine insert into `captured_events`,
   and `store.db.connect` opens read-write and sets `journal_mode = WAL`. The
   parity states under `rust/.parity-state/` are shared fleet infrastructure;
   a differ must not mutate them, and both implementations need their *own*
   copy anyway so their rows can be diffed against each other.

The store this writes is deliberately small (a few hundred KB) so
`hooks-parity.sh` can copy a fresh pair of homes per run and still finish
inside ci.sh's budget.

Schema comes from `stackunderflow.store.schema.apply` — the real migrations, not
a transcription, so the differ can never pass against a shape the product does
not have.

Usage:  build_hook_state.py <out-dir> [--force]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from stackunderflow.store import db, schema  # noqa: E402

# The project the SessionStart digest is about. `projects.path` is left NULL so
# `_project_fs_path` decodes the slug, which is what the maintainer's real rows
# do — the decode is lossy and both implementations must be lossy identically.
PROJECT_SLUG = "-tmp-stax-hook-parity-proj"
PROJECT_PATH = "/tmp/stax/hook/parity/proj"

# A file with real failure history, and one that is merely touched.
RISKY_FILE = "/tmp/stax/hook/parity/proj/services/discovery.py"
CLEAN_FILE = "/tmp/stax/hook/parity/proj/README.md"

SESSIONS = [
    # session_id, first_ts, last_ts, message_count, cost_usd
    ("hook-parity-session-0001", "2026-07-01T09:00:00Z", "2026-07-01T11:30:00Z", 42, 1.2345),
    ("hook-parity-session-0002", "2026-07-10T09:00:00Z", "2026-07-10T10:00:00Z", 7, 0.0),
    ("hook-parity-session-0003", "2026-07-20T09:00:00Z", "2026-07-20T09:05:00Z", 3, 12.5),
    # An undated session — exercises the `(undated)` branch of `_session_line`.
    ("hook-parity-session-0004", None, None, 1, 0.005),
    # The NEWEST session, carrying the over-long snippet (see `_messages`).
    ("hook-parity-session-0005", "2026-07-21T09:00:00Z", "2026-07-21T09:00:00Z", 1, 0.25),
]


def _messages(conn) -> None:
    """Write the message rows the three injection hooks read.

    Three shapes matter and each one is a branch:

    * `content_text` carrying a distinctive token — `search_past_decisions`
      does a substring `LIKE`, and the token has to be one `_prompt_to_query`
      would actually pick (identifier-shaped, >= 5 characters, not a stopword).
    * `tools_json` carrying a real `file_path` argument, so
      `_tools_json_mentions_file` can find it in *write* mode. This is exactly
      what the maintainer's store lacks.
    * a following user turn whose text trips the outcome classifier into
      `failed` / `reverted`, because `find_failure_modes_for_file` returns only
      those two and only above `min_confidence = 0.5`.
    """
    partition = "messages_202607"
    rows: list[tuple] = []
    seq = 0

    def add(session_fk, role, ts, content, tools=None):
        nonlocal seq
        seq += 1
        rows.append(
            (
                session_fk,
                seq,
                role,
                ts,
                content,
                json.dumps(tools) if tools is not None else "[]",
            )
        )

    # Session 1: an edit to the risky file, then the user says it broke.
    add(1, "assistant", "2026-07-01T10:00:00Z",
        "I will update the cache lookup in services/discovery.py.",
        [{"name": "Edit", "input": {"file_path": RISKY_FILE, "old_string": "a", "new_string": "b"}}])
    add(1, "user", "2026-07-01T10:01:00Z",
        "that broke the build, the tests fail now")
    add(1, "assistant", "2026-07-01T10:02:00Z",
        "Reverting. The STACKUNDERFLOW_HOME resolution was the problem.")

    # Session 2: another edit to the same file, reverted.
    add(2, "assistant", "2026-07-10T09:30:00Z",
        "Editing services/discovery.py again for the cache lookup.",
        [{"name": "Write", "input": {"file_path": RISKY_FILE, "content": "x"}}])
    add(2, "user", "2026-07-10T09:31:00Z",
        "no, revert that — it is wrong")

    # Session 3: a clean file, a decision worth surfacing, no failure.
    add(3, "assistant", "2026-07-20T09:01:00Z",
        "Decision: we keep services/discovery.py on the LIKE path and defer FTS.",
        [{"name": "Read", "input": {"file_path": CLEAN_FILE}}])
    add(3, "user", "2026-07-20T09:02:00Z", "perfect, that works")

    # Session 5 exists ONLY to carry a snippet longer than `inject._SNIPPET_CHARS`
    # (140), with a run of whitespace in it, so `_trim`'s collapse and its `…`
    # marker are exercised by the differ instead of merely present in the code.
    # `search_past_decisions` keeps the FIRST hit per session by `timestamp
    # DESC` and then caps at `limit`, ordered by `sessions.last_ts DESC`, so a
    # long line has to be the newest hit in the NEWEST session — otherwise it is
    # either not the session's snippet or cut by the limit. Found the hard way:
    # a deliberate 140 -> 139 mutation of the constant passed the whole suite,
    # twice, before the row landed somewhere the renderer could reach.
    add(5, "assistant", "2026-07-21T09:00:00Z",
        "Long decision about services/discovery.py:   "
        + "the LIKE scan stays because the FTS sidecar is empty on a fresh box, "
        * 3
        + "and that is the whole reason.")

    conn.executemany(
        f"INSERT INTO {partition} "
        "(session_fk, seq, role, timestamp, content_text, tools_json, raw_json, "
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens) "
        "VALUES (?, ?, ?, ?, ?, ?, '{}', 10, 20, 30, 40)",
        rows,
    )


def build(out: Path, force: bool) -> None:
    if out.exists() and not force:
        print(f"build_hook_state: {out} exists (use --force to rebuild)")
        return
    if out.exists():
        for child in sorted(out.iterdir()):
            if child.is_file():
                child.unlink()
    out.mkdir(parents=True, exist_ok=True)

    store = out / "store.db"
    conn = db.connect(store)
    schema.apply(conn)

    # A second provider on the SAME slug, with the LOWER id, so
    # `_resolve_project_id`'s `ORDER BY (provider = 'claude') DESC, id` tiebreak
    # is actually exercised: a port that forgot the host preference would write
    # project_id 1 into every captured row instead of 100.
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (1, 'codex', ?, NULL, 'parity-proj', 0.0, 0.0)",
        (PROJECT_SLUG,),
    )
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (100, 'claude', ?, NULL, 'parity-proj', 0.0, 0.0)",
        (PROJECT_SLUG,),
    )

    for session_id, first_ts, last_ts, count, _cost in SESSIONS:
        conn.execute(
            "INSERT INTO sessions (session_id, project_id, first_ts, last_ts, message_count) "
            "VALUES (?, 100, ?, ?, ?)",
            (session_id, first_ts, last_ts, count),
        )
    _messages(conn)

    for session_id, first_ts, last_ts, count, cost in SESSIONS:
        conn.execute(
            "INSERT INTO session_mart "
            "(session_id, project_id, provider, first_ts, last_ts, message_count, cost_usd) "
            "VALUES (?, 100, 'claude', ?, ?, ?, ?)",
            (session_id, first_ts or "", last_ts or "", count, cost),
        )
    conn.commit()
    conn.close()

    # `config.json` — the differ's cases override `proactive_enabled` through the
    # environment, so the file stays at the shipped defaults.
    (out / "config.json").write_text(json.dumps({}) + "\n")

    # The precomputed nudge cache, keyed exactly as `patterns._normalise_command`
    # and `_normalise_signature` produce. Hand-written rather than mined, because
    # the point is to prove both implementations DERIVE the same key from a live
    # payload and find this entry.
    signals = {
        "version": 1,
        "generated_at": "2026-07-25T00:00:00+00:00",
        "projects": {
            PROJECT_SLUG.replace("_", "-"): {
                "generated_at": "2026-07-25T00:00:00+00:00",
                "command_clusters": {
                    "npm install": {
                        "command": "npm install",
                        "failure_count": 7,
                        "session_count": 4,
                        "categories": {"network": 4, "permission": 4, "other": 1},
                        "last_failure_ts": "2026-07-24T12:00:00+00:00",
                    },
                    "pytest": {
                        "command": "pytest",
                        "failure_count": 1,
                        "session_count": 1,
                        "categories": {},
                        "last_failure_ts": "2026-07-24T12:00:00+00:00",
                    },
                },
                "file_risk": {},
                "error_signatures": {
                    "ModuleNotFoundError: No module named 'stax<n>'": {
                        "signature": "ModuleNotFoundError: No module named 'stax<n>'",
                        "category": "import",
                        "session_count": 5,
                        "count": 11,
                        "resolution_hints": [
                            {"action": "pip install -e .", "count": 4},
                            {"action": "uv sync", "count": 1},
                        ],
                        "last_ts": "2026-07-24T12:00:00+00:00",
                        "example": "ModuleNotFoundError: No module named 'stax9'",
                    }
                },
            }
        },
    }
    (out / "proactive_signals.json").write_text(json.dumps(signals))

    print(f"build_hook_state: wrote {out}")
    print(f"  store.db          {store.stat().st_size:,} bytes")
    print(f"  project slug      {PROJECT_SLUG}")
    print(f"  decoded path      {PROJECT_PATH}")
    print(f"  risky file        {RISKY_FILE}")


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        print(__doc__)
        raise SystemExit(2)
    build(Path(args[0]), force="--force" in sys.argv[1:])

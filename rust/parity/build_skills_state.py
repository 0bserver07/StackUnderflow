#!/usr/bin/env python3
"""Build the SYNTHETIC seed homes the `skills` / `recommend` rows run against.

Why synthetic rather than the shared `.parity-state` store:

1. **The detectors are threshold machines and the real store crosses none of
   them cleanly.** `skill_synth` emits a candidate only when a pattern appears
   in >= `--min-occurrences` *distinct sessions*, and the "runs X after an
   edit" detector additionally needs that to hold in >= 50% of the sessions
   that edited anything. A corpus that does not cross those constants proves
   only that both implementations print "No patterns" — the wave-6 lesson
   (`ARCHITECT-STATE.md`: *every constant a port copies needs a row that
   crosses it*) applies here almost word for word.
2. **`skill_synth` reads `raw_json`, not `tools_json`.** The tool calls come
   out of the verbatim provider payload (`message.content[]` blocks of type
   `tool_use`). A store whose `raw_json` is `'{}'` — which is what
   `build_hook_state.py` writes, because the hooks read `tools_json` — makes
   every detector return nothing.
3. **`skills generate` opens the store read-write.** Python's `_open_store` is
   `db.connect` + `schema.apply`, so it must not run against shared fleet
   state, and each implementation needs its own copy anyway for the tree diff.

The store is written through the product's own schema (`store.schema.apply`)
and the `messages` view's routing trigger, never a transcription, and then
"settled" by one read so a later `schema.apply` is a byte no-op — which is what
makes the case-home diff meaningful: the two implementations must leave the
seed exactly as they found it.

Usage:  build_skills_state.py <homes-dir> [--force]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from stackunderflow.store import db, schema  # noqa: E402

PROJECT_SLUG = "-tmp-stax-skills-parity-proj"
PROJECT_ID = 100

# Every timestamp is inside one month so the corpus lands in one partition, and
# far from any `parse_since` boundary the two implementations could straddle.
BASE_DAY = "2026-07-{day:02d}"

EDITED_FILE = "/tmp/stax/skills/parity/proj/services/thing.py"
CONFIG_FILE = "/tmp/stax/skills/parity/proj/config.json"

# The first user turn of the six "workflow" sessions. `mode_recommender`
# feature-extracts exactly this string: intent=fix (the `fix` keyword),
# token_band=tiny (< 800 chars), languages=['python'] (`.py` and `pytest`).
WORKFLOW_PROMPT = "fix the failing test in cost.py with pytest"

# session_id, primary_model, cost_usd — three cheap and three expensive, so
# `_pick_cheapest_model` has two groups to choose between and the cost-gap term
# of the confidence score is non-zero.
WORKFLOW_SESSIONS = [
    ("skills-parity-w01", "cheap-model", 0.10),
    ("skills-parity-w02", "cheap-model", 0.12),
    ("skills-parity-w03", "cheap-model", 0.11),
    ("skills-parity-w04", "spendy-model", 1.00),
    ("skills-parity-w05", "spendy-model", 1.20),
    ("skills-parity-w06", "spendy-model", 1.10),
]
AVOID_SESSIONS = [f"skills-parity-a{n:02d}" for n in range(1, 6)]
NEVER_SESSIONS = [f"skills-parity-n{n:02d}" for n in range(1, 6)]
LONE_SESSION = "skills-parity-z01"


def _tool_use(name: str, arguments: dict) -> str:
    """The Claude-shaped `raw_json` one assistant turn carries."""
    return json.dumps(
        {"message": {"content": [{"type": "tool_use", "name": name, "input": arguments}]}}
    )


def _plain(text: str) -> str:
    return json.dumps({"message": {"content": [{"type": "text", "text": text}]}})


def _rows() -> list[tuple]:
    """(session_key, seq, role, ts, content_text, raw_json) for every message."""
    rows: list[tuple] = []

    def add(session, seq, role, day, hour, text, raw):
        rows.append(
            (
                session,
                seq,
                role,
                f"{BASE_DAY.format(day=day)}T{hour:02d}:00:00Z",
                text,
                raw,
            )
        )

    # ── six workflow sessions: edit, then the canonical test command, then the
    # lint command. Crosses `--min-occurrences 5` for the test detector, the
    # flag-combo detector AND the after-edit detector (6 of 11 edit sessions =
    # 54%, just over the 50% floor the detector requires).
    for index, (session, _model, _cost) in enumerate(WORKFLOW_SESSIONS):
        day = 1 + index
        add(session, 1, "user", day, 9, WORKFLOW_PROMPT, _plain(WORKFLOW_PROMPT))
        add(
            session,
            2,
            "assistant",
            day,
            10,
            "Editing the service.",
            _tool_use("Edit", {"file_path": EDITED_FILE, "old_string": "a", "new_string": "b"}),
        )
        add(
            session,
            3,
            "assistant",
            day,
            11,
            "Running the suite.",
            _tool_use("Bash", {"command": "pytest tests/ -q"}),
        )
        add(
            session,
            4,
            "assistant",
            day,
            12,
            "Linting.",
            _tool_use("Bash", {"command": "ruff check --fix ."}),
        )

    # ── five correction sessions: the assistant reaches for `pkill`, the user
    # says no. Crosses the `avoids-X` threshold exactly (5 of 5).
    for index, session in enumerate(AVOID_SESSIONS):
        day = 10 + index
        add(session, 1, "user", day, 9, "restart the dev server", _plain("restart the dev server"))
        add(
            session,
            2,
            "assistant",
            day,
            10,
            "Killing it.",
            _tool_use("Bash", {"command": "pkill -f devserver"}),
        )
        add(
            session,
            3,
            "user",
            day,
            11,
            "don't use pkill, send SIGTERM instead",
            _plain("don't use pkill, send SIGTERM instead"),
        )

    # ── five more: the assistant edits a generated file, the user says no.
    # Crosses `never-touches-paths` (5) and adds five more edit sessions, which
    # is what puts the after-edit detector's ratio near its floor rather than
    # trivially at 1.0.
    for index, session in enumerate(NEVER_SESSIONS):
        day = 16 + index
        add(session, 1, "user", day, 9, "update the config", _plain("update the config"))
        add(
            session,
            2,
            "assistant",
            day,
            10,
            "Editing the config.",
            _tool_use("Edit", {"file_path": CONFIG_FILE, "old_string": "a", "new_string": "b"}),
        )
        add(
            session,
            3,
            "user",
            day,
            11,
            "don't edit config.json — it is generated",
            _plain("don't edit config.json — it is generated"),
        )

    # ── one session BELOW every threshold, so "the detector filtered it" is a
    # proven branch rather than an assumed one.
    add(LONE_SESSION, 1, "user", 22, 9, "add a health endpoint", _plain("add a health endpoint"))
    add(
        LONE_SESSION,
        2,
        "assistant",
        22,
        10,
        "Type-checking.",
        _tool_use("Bash", {"command": "mypy --strict ."}),
    )
    return rows


def _sessions() -> list[tuple]:
    """(session_id, primary_model, cost_usd) for every session, in id order."""
    out = list(WORKFLOW_SESSIONS)
    out.extend((session, "cheap-model", 0.05) for session in AVOID_SESSIONS)
    out.extend((session, "cheap-model", 0.06) for session in NEVER_SESSIONS)
    out.append((LONE_SESSION, "spendy-model", 3.5))
    return out


def build_store(store: Path) -> None:
    conn = db.connect(store)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, 'claude', ?, NULL, 'skills-parity-proj', 0.0, 0.0)",
        (PROJECT_ID, PROJECT_SLUG),
    )

    rows = _rows()
    stamps: dict[str, list[str]] = {}
    for session, _seq, _role, ts, _text, _raw in rows:
        stamps.setdefault(session, []).append(ts)

    session_fk: dict[str, int] = {}
    for session, _model, _cost in _sessions():
        marks = stamps[session]
        cursor = conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, ?, ?)",
            (PROJECT_ID, session, min(marks), max(marks), len(marks)),
        )
        session_fk[session] = int(cursor.lastrowid)

    # Through the `messages` VIEW, so the partition-routing trigger places each
    # row exactly as ingest would.
    conn.executemany(
        "INSERT INTO messages "
        "(session_fk, seq, role, timestamp, content_text, tools_json, raw_json, "
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens) "
        "VALUES (?, ?, ?, ?, ?, '[]', ?, 10, 20, 0, 0)",
        [
            (session_fk[session], seq, role, ts, text, raw)
            for session, seq, role, ts, text, raw in rows
        ],
    )

    for session, model, cost in _sessions():
        marks = stamps[session]
        conn.execute(
            "INSERT INTO session_mart "
            "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
            " message_count, cost_usd) VALUES (?, ?, 'claude', ?, ?, ?, ?, ?)",
            (session, PROJECT_ID, model, min(marks), max(marks), len(marks), cost),
        )
    conn.commit()
    conn.close()

    # "Settle" the file: a second `db.connect` + `schema.apply` is what every
    # Python CLI invocation does, and after this one the bytes stop moving — so
    # a case-home diff compares the two implementations, not the migration.
    settle = db.connect(store)
    schema.apply(settle)
    # VACUUM before settling: the migrations leave ~0.5 MB of free pages behind
    # and the seed is copied twice per case per state. The rebuild is followed by
    # one more `connect` + `apply` so the file the harness copies is the one a
    # CLI invocation has already touched.
    settle.execute("VACUUM")
    settle.close()
    final = db.connect(store)
    schema.apply(final)
    final.close()


# ── the on-disk skills tree ──────────────────────────────────────────────────

GENERATED_PATTERN_ID = "0000000000000000"

_INSTALLED = {
    # Ours, and old enough for every `--older-than` window.
    "auto-old-thing": (
        "---\n"
        "name: auto-old-thing\n"
        "description: An auto-generated skill from a previous run.\n"
        "auto_generated: true\n"
        "generated_at: 2026-01-02T03:04:05+00:00\n"
        "generated_from: 9 sessions in " + PROJECT_SLUG + "\n"
        "pattern_kind: avoids-X\n"
        "pattern_id: " + GENERATED_PATTERN_ID + "\n"
        "evidence_count: 9\n"
        "---\n"
        "\n"
        "<!-- Generated by stackunderflow skills generate at 2026-01-02T03:04:05+00:00 "
        "from 9 sessions — do not edit manually; regenerate to update -->\n"
        "\n"
        "\n"
        "# Avoid `oldthing` in this project\n"
    ),
    # Ours, and NEWER than a `30d` window would reach.
    "auto-recent-thing": (
        "---\n"
        "name: auto-recent-thing\n"
        "description: A recently generated skill.\n"
        "auto_generated: true\n"
        "generated_at: 2099-01-01T00:00:00+00:00\n"
        "generated_from: 4 sessions\n"
        "pattern_kind: uses-tool-flag-combo\n"
        "pattern_id: 1111111111111111\n"
        "evidence_count: 4\n"
        "---\n"
        "\n"
        "# Recent\n"
    ),
    # The user's own, under our prefix: `clean` must never touch it and `list`
    # must never report it.
    "auto-hand-written": "---\nname: auto-hand-written\ndescription: mine\n---\n\n# Mine\n",
    # Not our prefix at all.
    "handwritten": "---\nname: handwritten\nauto_generated: true\n---\n\n# Not auto-*\n",
    # Ours by marker but with NO `generated_at`, which `--older-than` keeps
    # (the conservative branch) and a bare `clean` removes.
    "auto-undated": (
        "---\n"
        "name: auto-undated\n"
        "description: no stamp\n"
        "auto_generated: true\n"
        "pattern_kind: avoids-X\n"
        "pattern_id: 2222222222222222\n"
        "evidence_count: 3\n"
        "---\n"
        "\n"
        "# Undated\n"
    ),
}


def build_skills_tree(root: Path) -> None:
    skills = root / ".claude" / "skills"
    for name, body in _INSTALLED.items():
        directory = skills / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "SKILL.md").write_text(body, encoding="utf-8")


# The `skills-both` tree adds three directories whose NAMES collide with what
# the corpus mines, because "created" is the only write branch a fresh tree can
# reach. These reach the other three:
#
#   auto-avoid-pkill            ours, same pattern_id -> "updated" + a .bak
#   auto-never-touch-config-json  NOT ours -> "skipped-user-authored"
#   auto-canonical-test-command ours, DIFFERENT pattern_id -> the collision
#                               suffix (`<name>-<hash6>`)
#
# `auto-avoid-pkill`'s pattern_id is the real mined one, which is also what
# makes `recommend skills` report `filtered_already_installed = 1`.
_BOTH_EXTRA = {
    "auto-avoid-pkill": (
        "---\n"
        "name: auto-avoid-pkill\n"
        "description: A stale copy of the mined skill.\n"
        "auto_generated: true\n"
        "generated_at: 2026-02-03T04:05:06+00:00\n"
        "generated_from: 5 sessions in " + PROJECT_SLUG + "\n"
        "pattern_kind: avoids-X\n"
        "pattern_id: ac6d58955fb3f131\n"
        "evidence_count: 5\n"
        "---\n"
        "\n"
        "# Avoid `pkill` in this project\n"
        "\n"
        "An older body, so the rewrite is a real change and not `unchanged`.\n"
    ),
    "auto-never-touch-config-json": (
        "---\nname: auto-never-touch-config-json\ndescription: hand written\n---\n"
        "\n# Mine, not yours\n"
    ),
    "auto-canonical-test-command": (
        "---\n"
        "name: auto-canonical-test-command\n"
        "description: A different pattern that claimed this directory first.\n"
        "auto_generated: true\n"
        "generated_at: 2026-02-03T04:05:06+00:00\n"
        "generated_from: 8 sessions\n"
        "pattern_kind: canonical-test-command\n"
        "pattern_id: 3333333333333333\n"
        "evidence_count: 8\n"
        "---\n"
        "\n"
        "# Some other project's test command\n"
    ),
}


def build_both_tree(root: Path) -> None:
    build_skills_tree(root)
    skills = root / ".claude" / "skills"
    for name, body in _BOTH_EXTRA.items():
        directory = skills / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "SKILL.md").write_text(body, encoding="utf-8")


def build(homes: Path, force: bool) -> None:
    targets = {
        "skills-corpus": ("store", False),
        "skills-installed": ("tree", False),
        "skills-both": ("store", True),
    }
    for name, (kind, with_tree) in targets.items():
        out = homes / name
        if out.exists() and not force:
            print(f"build_skills_state: {out} exists (use --force to rebuild)")
            continue
        if out.exists():
            import shutil

            shutil.rmtree(out)
        out.mkdir(parents=True, exist_ok=True)
        if kind == "store":
            build_store(out / "store.db")
        if with_tree:
            build_both_tree(out)
        elif kind == "tree":
            build_skills_tree(out)
        print(f"build_skills_state: wrote {out}")


def main(argv: list[str]) -> int:
    args = [a for a in argv[1:] if not a.startswith("--")]
    force = "--force" in argv[1:]
    if len(args) != 1:
        print(__doc__)
        return 2
    build(Path(args[0]), force)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

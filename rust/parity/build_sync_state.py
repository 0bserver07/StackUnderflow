#!/usr/bin/env python3
"""Build the synthetic multi-device stores the sync differ runs against.

Four stores, each a real `store.db` with the real schema applied by
``stackunderflow.store.schema.apply`` — not a hand-written subset, because a
fixture whose DDL drifts from production proves nothing about production:

  ``empty.db``    schema only. No projects, no marts, no `sync_identity`.
                  This is the DEFAULT-OFF store, and it is the one that proves
                  `status` and `/api/sync/overview` do no work when sync is off.

  ``device-a.db`` this device: an identity row, three projects, two months of
                  `daily_mart`, and the full Overview/Cost-core set.

  ``device-b.db`` a PEER: a different identity, overlapping `(provider, slug)`
                  with A on one project and disjoint on another, plus one
                  `session_id` A also has — the §5.3 hand-copied-logs case that
                  `merge_warnings` counts.

  ``merged.db``   A, with B's shards already landed in the `<mart>_remote`
                  tables and `sync_cursors` / `sync_remote_devices` populated.
                  Built by actually running `runner.push` + `runner.pull`, so
                  the fixture is the feature's own output rather than a guess
                  at its shape.

Why the values are what they are
--------------------------------
Every one of them crosses something. This wave's own law — *a constant a port
copies needs a corpus row that crosses it* — applies to fixture DATA as well as
to constants:

* an em-dash in a `display_name` crosses the shard writer's
  ``ensure_ascii=False`` (wave 5's finding 11, here changing a HASH);
* `1e16` and `0.1 + 0.2` cross `repr(float)` and the Neumaier-vs-`+=` split;
* an INTEGER-typed `0` next to a REAL-typed `0.0` crosses the storage-class rule
  that `8` and `8.0` are different bytes;
* a `first_ts` of `NULL` on one `session_mart` row crosses `_month_of`'s
  ``str(None)`` → `"unknown"` bucket;
* two months of `daily_mart` cross the monthly shard split, and a month-less
  `project_mart` crosses the single-`"all"`-shard branch;
* a project with rows on BOTH devices crosses the union's SUM, and one with rows
  on only one crosses the pass-through.

Usage
-----
    build_sync_state.py <outdir> [--force]
"""

from __future__ import annotations

import shutil
import sqlite3
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from stackunderflow.store import schema  # noqa: E402
from stackunderflow.sync import runner  # noqa: E402


class _Bucket:
    """Minimal directory-backed ObjectStore — mirrors sync_parity.py's."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def put(self, key, data):
        path = self.root / key
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def get(self, key):
        path = self.root / key
        if not path.is_file():
            from stackunderflow.sync.bucket import ObjectNotFound

            raise ObjectNotFound(key)
        return path.read_bytes()

    def list(self, prefix):  # noqa: A003
        if not self.root.is_dir():
            return []
        return sorted(
            p.relative_to(self.root).as_posix()
            for p in self.root.rglob("*")
            if p.is_file() and p.relative_to(self.root).as_posix().startswith(prefix)
        )

    def delete(self, key):
        (self.root / key).unlink(missing_ok=True)


def connect(path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(path, isolation_level=None)
    conn.row_factory = sqlite3.Row
    schema.apply(conn)
    return conn


def seed_device_a(conn: sqlite3.Connection) -> None:
    conn.executescript(
        """
        INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified)
        VALUES (1, 'claude', '-home-yad-alpha',  '/home/yad/alpha',  'alpha — main', 1.0, 2.0),
               (2, 'claude', '-home-yad-beta',   '/home/yad/beta',   'beta',          1.0, 2.0),
               (9, 'codex',  '-home-yad-alpha',  '/home/yad/alpha',  'alpha (codex)', 1.0, 2.0);

        -- Two months, so the monthly shard split has something to split, and a
        -- 1e16 cost so `repr(float)` and the compensated sum both get crossed.
        INSERT INTO daily_mart
          (day, project_id, provider, model, speed, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count, cost_usd)
        VALUES ('2026-06-30', 1, 'claude', 'opus',   'standard', 100, 200,  10,  5, 12, 2, 1.25),
               ('2026-07-01', 1, 'claude', 'opus',   'standard', 300, 400,  20, 10, 24, 3, 0.1),
               ('2026-07-01', 1, 'claude', 'haiku',  'fast',       7,   3,   0,  0,  2, 1, 0.2),
               ('2026-07-02', 2, 'claude', 'opus',   'standard',   0,   0,   0,  0,  0, 0, 0.0),
               ('2026-07-02', 9, 'codex',  'gpt-5',  'standard',  50,  60,   0,  0,  6, 1, 1e16);

        INSERT INTO provider_day_mart (day, provider, cost_usd, message_count, session_count, project_count)
        VALUES ('2026-06-30', 'claude', 1.25, 12, 2, 1),
               ('2026-07-01', 'claude', 0.30000000000000004, 26, 4, 1),
               ('2026-07-02', 'codex',  1e16, 6, 1, 1);

        INSERT INTO model_day_mart
          (day, model, speed, cost_usd, input_tokens, output_tokens, cache_read,
           cache_create, message_count, session_count)
        VALUES ('2026-06-30', 'opus',  'standard', 1.25, 100, 200, 10, 5, 12, 2),
               ('2026-07-01', 'opus',  'standard', 0.1,  300, 400, 20, 10, 24, 3),
               ('2026-07-01', 'haiku', 'fast',     0.2,    7,   3,  0,  0,  2, 1);

        INSERT INTO project_mart
          (project_id, provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd)
        VALUES (1, 'claude', '-home-yad-alpha', 'alpha — main',
                '2026-06-30T08:00:00', '2026-07-01T18:00:00', 38, 6, 407, 603, 30, 15, 1.55),
               (2, 'claude', '-home-yad-beta', 'beta',
                '2026-07-02T09:00:00', '2026-07-02T09:30:00', 0, 0, 0, 0, 0, 0, 0.0),
               (9, 'codex',  '-home-yad-alpha', 'alpha (codex)',
                '2026-07-02T10:00:00', '2026-07-02T11:00:00', 6, 1, 50, 60, 0, 0, 1e16);

        -- `a-null-ts` has a NULL first_ts: `_month_of(None)` is "unknown", the
        -- one shard month that is not a date. Nothing else in the corpus
        -- reaches that branch.
        INSERT INTO session_mart
          (session_id, project_id, provider, primary_model, first_ts, last_ts, cwd,
           message_count, user_message_count, assistant_message_count, input_tokens,
           output_tokens, cache_read, cache_create, cost_usd, is_one_shot)
        VALUES ('a-jun', 1, 'claude', 'opus', '2026-06-30T08:00:00', '2026-06-30T09:00:00',
                '/home/yad/alpha', 12, 6, 6, 100, 200, 10, 5, 1.25, 0),
               ('a-jul', 1, 'claude', 'opus', '2026-07-01T08:00:00', '2026-07-01T18:00:00',
                '/home/yad/alpha', 26, 13, 13, 307, 403, 20, 10, 0.30000000000000004, 0),
               ('shared-session', 9, 'codex', 'gpt-5', '2026-07-02T10:00:00',
                '2026-07-02T11:00:00', '/home/yad/alpha', 6, 3, 3, 50, 60, 0, 0, 1e16, 1);
        """
    )
    # A NULL `first_ts` cannot go through the NOT NULL column, so the "unknown"
    # month is reached the only way a real store could reach it: an empty
    # string, which is shorter than seven characters.
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, cwd, "
        " message_count, user_message_count, assistant_message_count, input_tokens, "
        " output_tokens, cache_read, cache_create, cost_usd, is_one_shot) "
        "VALUES ('a-noty', 2, 'claude', NULL, '', '', NULL, 1, 1, 0, 1, 1, 0, 0, 0.0, 1)"
    )
    runner.write_identity(
        conn,
        device_uuid="aaaaaaaaaaaa4aaaaaaaaaaaaaaaaaaa",
        key_fingerprint="fp0123456789abcd",
        bucket_url="ssh://yad@peer.example:2222/srv/stackunderflow-sync",
        endpoint_url=None,
        created_at="2026-06-01T00:00:00+00:00",
    )


def seed_device_b(conn: sqlite3.Connection) -> None:
    conn.executescript(
        """
        -- DIFFERENT local ids for the SAME (provider, slug) as A: id 1 here is
        -- A's id 9. That is the whole point of the re-key — the shard bytes
        -- must come out identical anyway.
        INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified)
        VALUES (1, 'codex',  '-home-yad-alpha', '/data/alpha', 'alpha (codex)', 1.0, 2.0),
               (5, 'claude', '-home-yad-alpha', '/data/alpha', 'alpha — main',  1.0, 2.0),
               (6, 'claude', '-work-gamma',     '/data/gamma', 'gamma',         1.0, 2.0);

        INSERT INTO daily_mart
          (day, project_id, provider, model, speed, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count, cost_usd)
        VALUES ('2026-07-01', 5, 'claude', 'opus',  'standard', 11, 22, 1, 1, 4, 1, 0.05),
               ('2026-07-03', 6, 'claude', 'opus',  'standard', 33, 44, 0, 0, 8, 2, 0.75),
               ('2026-07-02', 1, 'codex',  'gpt-5', 'standard',  9,  9, 0, 0, 2, 1, 0.5);

        INSERT INTO provider_day_mart (day, provider, cost_usd, message_count, session_count, project_count)
        VALUES ('2026-07-01', 'claude', 0.05, 4, 1, 1),
               ('2026-07-03', 'claude', 0.75, 8, 2, 1),
               ('2026-07-02', 'codex',  0.5,  2, 1, 1);

        INSERT INTO model_day_mart
          (day, model, speed, cost_usd, input_tokens, output_tokens, cache_read,
           cache_create, message_count, session_count)
        VALUES ('2026-07-01', 'opus',  'standard', 0.05, 11, 22, 1, 1, 4, 1),
               ('2026-07-03', 'opus',  'standard', 0.75, 33, 44, 0, 0, 8, 2),
               ('2026-07-02', 'gpt-5', 'standard', 0.5,   9,  9, 0, 0, 2, 1);

        INSERT INTO project_mart
          (project_id, provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd)
        VALUES (5, 'claude', '-home-yad-alpha', 'alpha — main',
                '2026-07-01T07:00:00', '2026-07-01T12:00:00', 4, 1, 11, 22, 1, 1, 0.05),
               (6, 'claude', '-work-gamma', 'gamma',
                '2026-07-03T07:00:00', '2026-07-03T12:00:00', 8, 2, 33, 44, 0, 0, 0.75),
               (1, 'codex',  '-home-yad-alpha', 'alpha (codex)',
                '2026-07-02T10:00:00', '2026-07-02T11:00:00', 2, 1, 9, 9, 0, 0, 0.5);

        -- `shared-session` also exists on A: the §5.3 duplicate the dedup drops
        -- and `merge_warnings` counts.
        INSERT INTO session_mart
          (session_id, project_id, provider, primary_model, first_ts, last_ts, cwd,
           message_count, user_message_count, assistant_message_count, input_tokens,
           output_tokens, cache_read, cache_create, cost_usd, is_one_shot)
        VALUES ('b-jul', 5, 'claude', 'opus', '2026-07-01T07:00:00', '2026-07-01T12:00:00',
                '/data/alpha', 4, 2, 2, 11, 22, 1, 1, 0.05, 0),
               ('b-gamma', 6, 'claude', 'opus', '2026-07-03T07:00:00', '2026-07-03T12:00:00',
                '/data/gamma', 8, 4, 4, 33, 44, 0, 0, 0.75, 0),
               ('shared-session', 1, 'codex', 'gpt-5', '2026-07-02T10:00:00',
                '2026-07-02T11:00:00', '/data/alpha', 6, 3, 3, 50, 60, 0, 0, 1e16, 1);
        """
    )
    runner.write_identity(
        conn,
        device_uuid="bbbbbbbbbbbb4bbbbbbbbbbbbbbbbbbb",
        key_fingerprint="fp0123456789abcd",
        bucket_url="ssh://yad@peer.example:2222/srv/stackunderflow-sync",
        endpoint_url=None,
        created_at="2026-06-02T00:00:00+00:00",
    )


def build(outdir: Path, force: bool) -> None:
    if outdir.exists() and force:
        shutil.rmtree(outdir)
    outdir.mkdir(parents=True, exist_ok=True)

    for name, seed in (
        ("empty.db", None),
        ("device-a.db", seed_device_a),
        ("device-b.db", seed_device_b),
    ):
        path = outdir / name
        path.unlink(missing_ok=True)
        conn = connect(path)
        try:
            if seed is not None:
                seed(conn)
        finally:
            conn.close()

    # `merged.db` is A after a real pull of B — the feature's own output.
    merged = outdir / "merged.db"
    merged.unlink(missing_ok=True)
    shutil.copy2(outdir / "device-a.db", merged)
    bucket_root = outdir / "seed-bucket"
    if bucket_root.exists():
        shutil.rmtree(bucket_root)
    bucket_root.mkdir(parents=True)
    bucket = _Bucket(bucket_root)

    # THE FIXTURE MUST STAY CLEAN. `runner.push` writes `sync_outbox` rows into
    # the store it pushes FROM, so seeding straight out of `device-b.db` would
    # ship a fixture whose watermarks are already at the current hashes — and
    # every later `push`/`pull` case would then be a silent no-op that both
    # implementations agree on. That is the green-by-vacuum trap the hooks wave
    # named; it cost this differ one full run to rediscover. Push from a COPY.
    peer_scratch = outdir / ".seed-peer.db"
    peer_scratch.unlink(missing_ok=True)
    shutil.copy2(outdir / "device-b.db", peer_scratch)
    peer = connect(peer_scratch)
    try:
        runner.push(
            peer,
            bucket,
            device_uuid="bbbbbbbbbbbb4bbbbbbbbbbbbbbbbbbb",
            key_fingerprint="fp0123456789abcd",
            encryptor=lambda raw: raw,
            now="2026-07-01T00:00:00+00:00",
        )
    finally:
        peer.close()
    peer_scratch.unlink(missing_ok=True)

    conn = connect(merged)
    try:
        result = runner.pull(
            conn,
            bucket,
            self_device_uuid="aaaaaaaaaaaa4aaaaaaaaaaaaaaaaaaa",
            decryptor=lambda raw: raw,
            now="2026-07-05T00:00:00+00:00",
        )
    finally:
        conn.close()
    if result.warnings:
        raise SystemExit(f"build_sync_state: seeding pull warned: {result.warnings}")
    if result.shards_ingested == 0:
        raise SystemExit("build_sync_state: seeding pull landed nothing")

    print(
        f"build_sync_state: {outdir} — "
        f"merged.db carries {result.shards_ingested} shard(s) from 1 peer"
    )


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        sys.stderr.write("usage: build_sync_state.py <outdir> [--force]\n")
        return 2
    build(Path(argv[1]), "--force" in argv[2:])
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

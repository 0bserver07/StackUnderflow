# Seed homes for the case-local rows in `parity-cli.sh`

A case whose cwd token ends in `@home:<name>` gets a fresh copy of the directory
named here as its `$STACKUNDERFLOW_HOME` — **one copy per implementation** — and
the harness diffs the two trees after the run. That is what makes a *writer*
(`cfg set`, `cfg rm`, `cfg model-alias …`, `clear-cache`) gateable: neither run
can see the other's writes, and the proof is both the printed bytes and the
resulting file tree.

`@home` with no name is an empty home, which is its own fixture: it exercises
the "no `config.json`", "no `backups/`", "no cursor cache" branches.

| seed | what it pins |
| --- | --- |
| `cfg-populated` | a realistic `config.json`: a validated key (`currency`), an int, a bool, a `None`-defaulted key holding a **string** (`budget_monthly_usd`), a list, and a two-entry `model_aliases` in non-sorted insertion order |
| `cfg-corrupt` | `settings._load`'s `JSONDecodeError` leg — every key must read as `[default]` |
| `cfg-wrongtypes` | the defensive coercion: a non-dict `model_aliases` and a non-list `plan_alert_thresholds` fall back, a non-int `port` does **not** |
| `cursor-cache` | `clear-cache`'s three-line branch (the file exists and is removed) |
| `backup-tree` | two backups, one complete and one missing every artifact, a `.hidden` directory that `pathlib.rglob` still counts, and a stray file that inflates `backup list`'s count but not its listing |
| `skills-corpus` | the wave-8 tranche-4 miner corpus: 17 sessions whose `raw_json` carries real `tool_use` blocks, built to cross each `skill_synth` threshold by ONE (6 sessions for a 5-session detector, 5 for both correction detectors, 6-of-11 edit sessions against a 50% floor) plus `session_mart` rows in two price tiers for `recommend mode`. Built by `parity/build_skills_state.py`; the store is settled so `schema.apply` is a byte no-op |
| `skills-installed` | an on-disk `.claude/skills/` with all five shapes `skills list` / `clean` must tell apart: ours-and-old, ours-and-future-dated, ours-with-no-stamp, `auto-`-prefixed-but-hand-authored, and marked-but-not-`auto-`-prefixed |
| `skills-both` | `skills-corpus` plus a skills tree whose directory NAMES collide with what the corpus mines — the only way to reach `updated` (+ its `.bak`), `skipped-user-authored`, the `<name>-<hash6>` collision suffix, and `recommend skills`' already-installed filter |
| `doctor-findings` | `doctor`'s RUNTIME health findings, which no other state can produce: a dangling `sessions` row for `PRAGMA foreign_key_check`, a `mart_watermark` ahead of every event, and orphan rows in two of the three marts the check walks — so the loop's order shows up in the finding order. Also the only seed reaching `NO_BILLABLE` |
| `doctor-newer` | a healthy store stamped `PRAGMA user_version = 99`. *Behind*-schema is the normal pre-migration state and is deliberately not a finding, so the advisory `schema` check needs a store that is AHEAD |
| `doctor-delivered` | a provider that made it all the way through — base rows → `usage_events` → `provider_day_mart`. The only seed that reaches status `OK`, and the only one where the `marts` column is not a constant zero |
| `doctor-diskgap` | sessions on disk (`.claude/projects/<slug>/*.jsonl`) and no store at all → `DISK_GAP` + the `stranded providers:` note. Works only because a `home` cwd exports `$HOME` into the case tree; see DIV-387 for why every `doctor` row must |
| `doctor-corrupt` | a `store.db` that is not a database. `sqlite3.connect(uri=True)` is lazy, so this lands as an `integrity` finding rather than a `store` one, and it is the only seed that sets `billable_scan_error` |
| `risk-corpus` | four sessions over one file — reverted, failed, worked, and mentioned-in-prose-only — so `risk file`'s four counts are all DIFFERENT and its `recent failure-mode sessions:` block renders. The maintainer's real store answers 0/0/0 on every candidate file (`tools_json` there is names-only), so this behaviour has no other home |

Keep these small and text-only: they are committed, they are copied twice per
case per state, and their byte sizes are load-bearing (`backup list` prints MB).
The `skills-*` and `doctor-*`/`risk-*` seeds carrying a store are the exception
at ~530 KB each —
`skill_synth` reads `raw_json` and needs a real schema underneath it, and the
alternative (mining the shared state) is a writer running against fleet state.

The six `doctor-*` / `risk-corpus` seeds are built by
`parity/build_doctor_state.py --force`, whose schema is the reference's own
`stackunderflow.store.schema.apply` and whose partitions come from the ingest
writer's `_ensure_partition` — only the ROWS are the fixture's (DIV-282).

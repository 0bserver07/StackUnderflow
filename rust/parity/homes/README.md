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

Keep these small and text-only: they are committed, they are copied twice per
case per state, and their byte sizes are load-bearing (`backup list` prints MB).

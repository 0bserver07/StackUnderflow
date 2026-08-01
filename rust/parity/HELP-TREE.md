# The `--help`-tree differ — measurement, not a wish

**Generated.** Regenerate from the rust worktree root:

```
rust/help-tree.sh          # or: rust/parity/tools/help_tree.py rust/parity/HELP-TREE.md
```

## Verdict

* **105** nodes in the Python tree; **30** exist in the Rust binary today (the other **75** are unported — listed below by name, never skipped silently).
* **0 / 30** are byte-identical after the scoped program-name substitution.
* **30 / 30** agree on all three contract facts the wave-8 items name — *same summary, same options, same subcommand list*.
* **0** ported nodes disagree on a contract fact.
* clap's stripped trailing `.` accounted for **14** summary differences and Click's mid-word 80-column wrap for **1** more; both were normalised away and both are counted here, not hidden.

## D-1, measured and re-filed

`rust/PARITY-wave1-resume.md` filed D-1 at wave 1 as "Click wraps at 80 columns with a two-column option table; clap prints its own layout" and deferred the measurement to this wave. Here it is. The two templates differ in **eight structural ways**, and only three of them are reachable by tuning a clap option:

| # | Click | clap 4.5 | fixable without a custom template? |
| ---: | --- | --- | --- |
| 1 | `Usage:` first, summary second | summary first, `Usage:` second | no |
| 2 | summary indented two spaces | summary flush left | no |
| 3 | summary keeps its trailing `.` | derive strips one trailing `.` | yes — `about = "…"` on all 105 nodes |
| 4 | no `Arguments:` section | `Arguments:` section for positionals | no |
| 5 | `--help  Show this message and exit.` | `-h, --help  Print help` | partly — `-h` can be dropped, the text cannot |
| 6 | subcommands listed **sorted** | subcommands in declaration order | yes — declare alphabetically |
| 7 | no `help` subcommand | a synthesised `help` subcommand | yes — `disable_help_subcommand` |
| 8 | every column wrapped to 80 | option help not wrapped | no |

**Ruling requested.** Byte-parity on `--help` is reachable only by replacing clap's renderer with a hand-written Click-shaped template (`Command::help_template` plus a per-node `about`), which means the port carries a second help engine whose only consumer is a differ. The cheaper contract — *same summary, same options, same subcommand list*, which is what RS-8-014..027 actually specify — is what this tool gates, and it is what the table below reports. Filed as **DIV-240**; the maintainer decides whether the byte-level goal is worth a template.

## Per-node status

`—` means the fact does not apply to that node (a leaf has no `Commands:`).

| path | kind | ported | bytes py/rs | summary | options | subcommands | usage | notes |
| --- | --- | --- | ---: | :---: | :---: | :---: | :---: | --- |
| `(root)` | group | yes | 3031/1594 | ok | ok | ok | **DIFF** | Rust-only by ruling: `anchor`, `store`; 29 subcommand(s) not ported yet, excluded from the comparison: `analyze`, `benchmark`, `compare`, `context-budget`, `context-replay`, `discovery`, `docs`, `doctor`, `etl`, `export`, `guide`, `hooks`, `import`, `ingest`, `init`, `month`, `optimize`, `plan`, `pricing`, `recommend`, `reindex`, `report`, `risk`, `skills`, `start`, `sync`, `today`, `worktrees`, `yield`; usage py='Usage: stackunderflow [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow <COMMAND>' |
| `analyze` | group | **no** | 969/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze backfill` | command | **no** | 1009/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze quality` | command | **no** | 376/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze session` | command | **no** | 644/— | — | — | — | — | unported — the Rust binary has no such node |
| `backup` | group | yes | 473/312 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; 3 subcommand(s) not ported yet, excluded from the comparison: `auto`, `create`, `restore`; usage py='Usage: stackunderflow backup [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow backup <COMMAND>' |
| `backup auto` | command | **no** | 235/— | — | — | — | — | unported — the Rust binary has no such node |
| `backup create` | command | **no** | 1341/— | — | — | — | — | unported — the Rust binary has no such node |
| `backup list` | command | yes | 118/92 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow backup list [OPTIONS]' rs='Usage: stackunderflow backup list' |
| `backup restore` | command | **no** | 198/— | — | — | — | — | unported — the Rust binary has no such node |
| `backup verify` | command | yes | 540/561 | ok | ok | ok | ok |  |
| `benchmark` | group | **no** | 651/— | — | — | — | — | unported — the Rust binary has no such node |
| `benchmark recommend` | command | **no** | 728/— | — | — | — | — | unported — the Rust binary has no such node |
| `benchmark show` | command | **no** | 784/— | — | — | — | — | unported — the Rust binary has no such node |
| `cfg` | group | yes | 372/407 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow cfg <COMMAND>' |
| `cfg ls` | command | yes | 150/138 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `cfg model-alias` | group | yes | 336/368 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow cfg model-alias <COMMAND>' |
| `cfg model-alias ls` | command | yes | 159/147 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `cfg model-alias rm` | command | yes | 143/172 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias rm [OPTIONS] SOURCE' rs='Usage: stackunderflow cfg model-alias rm <SOURCE>' |
| `cfg model-alias set` | command | yes | 182/248 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias set [OPTIONS] SOURCE TARGET' rs='Usage: stackunderflow cfg model-alias set <SOURCE> <TARGET>' |
| `cfg rm` | command | yes | 127/146 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg rm [OPTIONS] KEY' rs='Usage: stackunderflow cfg rm <KEY>' |
| `cfg set` | command | yes | 137/189 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg set [OPTIONS] KEY VALUE' rs='Usage: stackunderflow cfg set <KEY> <VALUE>' |
| `clear-cache` | command | yes | 165/234 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow clear-cache [OPTIONS] [PROJECT]' rs='Usage: stackunderflow clear-cache [PROJECT]' |
| `compare` | command | **no** | 1214/— | — | — | — | — | unported — the Rust binary has no such node |
| `config` | group | yes | 137/183 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow config <COMMAND>' |
| `config set` | command | yes | 101/157 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config set [OPTIONS] KEY VALUE' rs='Usage: stackunderflow config set <KEY> <VALUE>' |
| `config show` | command | yes | 101/106 | ok | ok | ok | ok |  |
| `config unset` | command | yes | 97/120 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config unset [OPTIONS] KEY' rs='Usage: stackunderflow config unset <KEY>' |
| `context-budget` | command | **no** | 645/— | — | — | — | — | unported — the Rust binary has no such node |
| `context-replay` | command | **no** | 1395/— | — | — | — | — | unported — the Rust binary has no such node |
| `discovery` | group | **no** | 331/— | — | — | — | — | unported — the Rust binary has no such node |
| `discovery demote-uncited` | command | **no** | 688/— | — | — | — | — | unported — the Rust binary has no such node |
| `discovery telemetry` | command | **no** | 673/— | — | — | — | — | unported — the Rust binary has no such node |
| `docs` | group | **no** | 277/— | — | — | — | — | unported — the Rust binary has no such node |
| `docs list` | command | **no** | 260/— | — | — | — | — | unported — the Rust binary has no such node |
| `docs show` | command | **no** | 201/— | — | — | — | — | unported — the Rust binary has no such node |
| `doctor` | command | **no** | 828/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl` | group | **no** | 318/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl backfill` | command | **no** | 570/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl status` | command | **no** | 434/— | — | — | — | — | unported — the Rust binary has no such node |
| `export` | command | **no** | 1654/— | — | — | — | — | unported — the Rust binary has no such node |
| `find-failure-modes-for-file` | command | yes | 1417/1337 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-failure-modes-for-file [OPTIONS] FILE' rs='Usage: stackunderflow find-failure-modes-for-file [OPTIONS] <FILE>' |
| `find-sessions-in-path` | command | yes | 1258/1037 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-sessions-in-path [OPTIONS] PATH' rs='Usage: stackunderflow find-sessions-in-path [OPTIONS] <PATH>' |
| `find-sessions-touching-file` | command | yes | 768/1011 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow find-sessions-touching-file [OPTIONS] FILE' rs='Usage: stackunderflow find-sessions-touching-file [OPTIONS] <FILE>' |
| `find-sessions-where-action-worked` | command | yes | 1829/1674 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-sessions-where-action-worked [OPTIONS] ACTION' rs='Usage: stackunderflow find-sessions-where-action-worked [OPTIONS] <ACTION>' |
| `guide` | group | **no** | 409/— | — | — | — | — | unported — the Rust binary has no such node |
| `guide install` | command | **no** | 429/— | — | — | — | — | unported — the Rust binary has no such node |
| `guide status` | command | **no** | 322/— | — | — | — | — | unported — the Rust binary has no such node |
| `guide uninstall` | command | **no** | 306/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks` | group | **no** | 521/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks install` | command | **no** | 948/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks repair` | command | **no** | 477/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks run` | command | **no** | 300/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks status` | command | **no** | 347/— | — | — | — | — | unported — the Rust binary has no such node |
| `hooks uninstall` | command | **no** | 276/— | — | — | — | — | unported — the Rust binary has no such node |
| `import` | command | **no** | 1361/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest` | group | **no** | 329/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest github` | command | **no** | 1002/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest webhook` | group | **no** | 243/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest webhook serve` | command | **no** | 764/— | — | — | — | — | unported — the Rust binary has no such node |
| `init` | command | **no** | 1343/— | — | — | — | — | unported — the Rust binary has no such node |
| `memory` | group | yes | 1177/1174 | ok | ok | ok | **DIFF** | summary differs only by Click's mid-word line wrap; 1 subcommand(s) not ported yet, excluded from the comparison: `embed`; usage py='Usage: stackunderflow memory [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow memory <COMMAND>' |
| `memory ask` | command | yes | 1639/2467 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory ask [OPTIONS] QUESTION' rs='Usage: stackunderflow memory ask [OPTIONS] <QUESTION>' |
| `memory decisions` | command | yes | 1300/2054 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory decisions [OPTIONS] QUERY' rs='Usage: stackunderflow memory decisions [OPTIONS] <QUERY>' |
| `memory embed` | command | **no** | 585/— | — | — | — | — | unported — the Rust binary has no such node |
| `memory file` | command | yes | 1369/2100 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory file [OPTIONS] PATH' rs='Usage: stackunderflow memory file [OPTIONS] <PATH>' |
| `memory sessions` | command | yes | 1514/2256 | ok | ok | ok | ok |  |
| `memory worked` | command | yes | 1339/2091 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory worked [OPTIONS] ACTION' rs='Usage: stackunderflow memory worked [OPTIONS] <ACTION>' |
| `month` | command | **no** | 713/— | — | — | — | — | unported — the Rust binary has no such node |
| `optimize` | command | **no** | 1121/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan` | group | **no** | 411/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan reset` | command | **no** | 117/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan set` | command | **no** | 457/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan show` | command | **no** | 203/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan thresholds` | group | **no** | 368/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan thresholds reset` | command | **no** | 155/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan thresholds set` | command | **no** | 173/— | — | — | — | — | unported — the Rust binary has no such node |
| `plan thresholds show` | command | **no** | 175/— | — | — | — | — | unported — the Rust binary has no such node |
| `pricing` | group | **no** | 236/— | — | — | — | — | unported — the Rust binary has no such node |
| `pricing doctor` | command | **no** | 866/— | — | — | — | — | unported — the Rust binary has no such node |
| `recommend` | group | **no** | 459/— | — | — | — | — | unported — the Rust binary has no such node |
| `recommend mode` | command | **no** | 625/— | — | — | — | — | unported — the Rust binary has no such node |
| `recommend skills` | command | **no** | 931/— | — | — | — | — | unported — the Rust binary has no such node |
| `reindex` | command | **no** | 131/— | — | — | — | — | unported — the Rust binary has no such node |
| `report` | command | **no** | 1245/— | — | — | — | — | unported — the Rust binary has no such node |
| `resume` | command | yes | 1085/1044 | ok | ok | ok | ok |  |
| `risk` | group | **no** | 423/— | — | — | — | — | unported — the Rust binary has no such node |
| `risk file` | command | **no** | 600/— | — | — | — | — | unported — the Rust binary has no such node |
| `search-past-decisions` | command | yes | 1509/1050 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow search-past-decisions [OPTIONS] QUERY' rs='Usage: stackunderflow search-past-decisions [OPTIONS] <QUERY>' |
| `skills` | group | **no** | 553/— | — | — | — | — | unported — the Rust binary has no such node |
| `skills clean` | command | **no** | 740/— | — | — | — | — | unported — the Rust binary has no such node |
| `skills generate` | command | **no** | 1696/— | — | — | — | — | unported — the Rust binary has no such node |
| `skills list` | command | **no** | 469/— | — | — | — | — | unported — the Rust binary has no such node |
| `start` | command | **no** | 984/— | — | — | — | — | unported — the Rust binary has no such node |
| `status` | command | yes | 718/617 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `sync` | group | **no** | 494/— | — | — | — | — | unported — the Rust binary has no such node |
| `sync init` | command | **no** | 947/— | — | — | — | — | unported — the Rust binary has no such node |
| `sync pull` | command | **no** | 687/— | — | — | — | — | unported — the Rust binary has no such node |
| `sync push` | command | **no** | 274/— | — | — | — | — | unported — the Rust binary has no such node |
| `sync status` | command | **no** | 209/— | — | — | — | — | unported — the Rust binary has no such node |
| `today` | command | **no** | 708/— | — | — | — | — | unported — the Rust binary has no such node |
| `worktrees` | group | **no** | 329/— | — | — | — | — | unported — the Rust binary has no such node |
| `worktrees attribute` | command | **no** | 424/— | — | — | — | — | unported — the Rust binary has no such node |
| `worktrees list` | command | **no** | 624/— | — | — | — | — | unported — the Rust binary has no such node |
| `yield` | command | **no** | 1423/— | — | — | — | — | unported — the Rust binary has no such node |

## The unported nodes

Reported so the count is honest: a differ that only walked what exists would have claimed a clean tree at wave 1 with nine commands ported.

```
analyze
analyze backfill
analyze quality
analyze session
backup auto
backup create
backup restore
benchmark
benchmark recommend
benchmark show
compare
context-budget
context-replay
discovery
discovery demote-uncited
discovery telemetry
docs
docs list
docs show
doctor
etl
etl backfill
etl status
export
guide
guide install
guide status
guide uninstall
hooks
hooks install
hooks repair
hooks run
hooks status
hooks uninstall
import
ingest
ingest github
ingest webhook
ingest webhook serve
init
memory embed
month
optimize
plan
plan reset
plan set
plan show
plan thresholds
plan thresholds reset
plan thresholds set
plan thresholds show
pricing
pricing doctor
recommend
recommend mode
recommend skills
reindex
report
risk
risk file
skills
skills clean
skills generate
skills list
start
sync
sync init
sync pull
sync push
sync status
today
worktrees
worktrees attribute
worktrees list
yield
```


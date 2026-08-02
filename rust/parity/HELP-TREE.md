# The `--help`-tree differ — measurement, not a wish

**Generated.** Regenerate from the rust worktree root:

```
rust/help-tree.sh          # or: rust/parity/tools/help_tree.py rust/parity/HELP-TREE.md
```

## Verdict

* **105** nodes in the Python tree; **83** exist in the Rust binary today (the other **22** are unported — listed below by name, never skipped silently).
* **0 / 83** are byte-identical after the scoped program-name substitution.
* **77 / 83** agree on all three contract facts the wave-8 items name — *same summary, same options, same subcommand list*.
* **6** ported nodes disagree on a contract fact.
* clap's stripped trailing `.` accounted for **48** summary differences and Click's mid-word 80-column wrap for **3** more; both were normalised away and both are counted here, not hidden.

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
| `(root)` | group | yes | 3031/3464 | ok | ok | ok | **DIFF** | Rust-only by ruling: `anchor`, `store`; 9 subcommand(s) not ported yet, excluded from the comparison: `analyze`, `discovery`, `doctor`, `etl`, `import`, `ingest`, `pricing`, `reindex`, `risk`; usage py='Usage: stackunderflow [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow <COMMAND>' |
| `analyze` | group | **no** | 969/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze backfill` | command | **no** | 1009/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze quality` | command | **no** | 376/— | — | — | — | — | unported — the Rust binary has no such node |
| `analyze session` | command | **no** | 644/— | — | — | — | — | unported — the Rust binary has no such node |
| `backup` | group | yes | 473/509 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow backup [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow backup <COMMAND>' |
| `backup auto` | command | yes | 235/250 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `backup create` | command | yes | 1341/1297 | ok | ok | ok | ok |  |
| `backup list` | command | yes | 118/92 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow backup list [OPTIONS]' rs='Usage: stackunderflow backup list' |
| `backup restore` | command | yes | 198/236 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow backup restore [OPTIONS] NAME' rs='Usage: stackunderflow backup restore [OPTIONS] <NAME>' |
| `backup verify` | command | yes | 540/561 | ok | ok | ok | ok |  |
| `benchmark` | group | yes | 651/711 | ok | ok | ok | **DIFF** | summary differs only by Click's mid-word line wrap; usage py='Usage: stackunderflow benchmark [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow benchmark <COMMAND>' |
| `benchmark recommend` | command | yes | 728/721 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow benchmark recommend [OPTIONS]' rs='Usage: stackunderflow benchmark recommend [OPTIONS] --intent <INTENT>' |
| `benchmark show` | command | yes | 784/709 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `cfg` | group | yes | 372/407 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow cfg <COMMAND>' |
| `cfg ls` | command | yes | 150/138 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `cfg model-alias` | group | yes | 336/368 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow cfg model-alias <COMMAND>' |
| `cfg model-alias ls` | command | yes | 159/147 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `cfg model-alias rm` | command | yes | 143/172 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias rm [OPTIONS] SOURCE' rs='Usage: stackunderflow cfg model-alias rm <SOURCE>' |
| `cfg model-alias set` | command | yes | 182/248 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg model-alias set [OPTIONS] SOURCE TARGET' rs='Usage: stackunderflow cfg model-alias set <SOURCE> <TARGET>' |
| `cfg rm` | command | yes | 127/146 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg rm [OPTIONS] KEY' rs='Usage: stackunderflow cfg rm <KEY>' |
| `cfg set` | command | yes | 137/189 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow cfg set [OPTIONS] KEY VALUE' rs='Usage: stackunderflow cfg set <KEY> <VALUE>' |
| `clear-cache` | command | yes | 165/234 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow clear-cache [OPTIONS] [PROJECT]' rs='Usage: stackunderflow clear-cache [PROJECT]' |
| `compare` | command | yes | 1214/1158 | ok | ok | ok | ok |  |
| `config` | group | yes | 137/183 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow config <COMMAND>' |
| `config set` | command | yes | 101/157 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config set [OPTIONS] KEY VALUE' rs='Usage: stackunderflow config set <KEY> <VALUE>' |
| `config show` | command | yes | 101/106 | ok | ok | ok | ok |  |
| `config unset` | command | yes | 97/120 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow config unset [OPTIONS] KEY' rs='Usage: stackunderflow config unset <KEY>' |
| `context-budget` | command | yes | 645/421 | **DIFF** | ok | ok | ok | summary py='Estimate the per-session context tax (system prompt + MCP + skills + memory). Inspects the visible config files (CLAUDE.md, ~/.claude.json mcpServers, ~/.claude/skills/, agents) and produces a token / cost estimate. The ``len(text) // 4`` heuristic is approximate — useful for spotting bloat, not for billing.' rs='Estimate the per-session context tax (system prompt + MCP + skills + memory)' |
| `context-replay` | command | yes | 1395/1729 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow context-replay [OPTIONS] SESSION_ID' rs='Usage: stackunderflow context-replay [OPTIONS] <SESSION_ID>' |
| `discovery` | group | **no** | 331/— | — | — | — | — | unported — the Rust binary has no such node |
| `discovery demote-uncited` | command | **no** | 688/— | — | — | — | — | unported — the Rust binary has no such node |
| `discovery telemetry` | command | **no** | 673/— | — | — | — | — | unported — the Rust binary has no such node |
| `docs` | group | yes | 277/307 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow docs [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow docs <COMMAND>' |
| `docs list` | command | yes | 260/256 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `docs show` | command | yes | 201/228 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow docs show [OPTIONS] TOPIC' rs='Usage: stackunderflow docs show [OPTIONS] <TOPIC>' |
| `doctor` | command | **no** | 828/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl` | group | **no** | 318/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl backfill` | command | **no** | 570/— | — | — | — | — | unported — the Rust binary has no such node |
| `etl status` | command | **no** | 434/— | — | — | — | — | unported — the Rust binary has no such node |
| `export` | command | yes | 1654/1506 | ok | ok | ok | **DIFF** | summary differs only by Click's mid-word line wrap; usage py='Usage: stackunderflow export [OPTIONS]' rs='Usage: stackunderflow export [OPTIONS] --format <FMT> --output <OUTPUT>' |
| `find-failure-modes-for-file` | command | yes | 1417/1337 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-failure-modes-for-file [OPTIONS] FILE' rs='Usage: stackunderflow find-failure-modes-for-file [OPTIONS] <FILE>' |
| `find-sessions-in-path` | command | yes | 1258/1037 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-sessions-in-path [OPTIONS] PATH' rs='Usage: stackunderflow find-sessions-in-path [OPTIONS] <PATH>' |
| `find-sessions-touching-file` | command | yes | 768/1011 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow find-sessions-touching-file [OPTIONS] FILE' rs='Usage: stackunderflow find-sessions-touching-file [OPTIONS] <FILE>' |
| `find-sessions-where-action-worked` | command | yes | 1829/1674 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow find-sessions-where-action-worked [OPTIONS] ACTION' rs='Usage: stackunderflow find-sessions-where-action-worked [OPTIONS] <ACTION>' |
| `guide` | group | yes | 409/499 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow guide [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow guide <COMMAND>' |
| `guide install` | command | yes | 429/403 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `guide status` | command | yes | 322/343 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `guide uninstall` | command | yes | 306/283 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `hooks` | group | yes | 521/688 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow hooks [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow hooks <COMMAND>' |
| `hooks install` | command | yes | 948/768 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `hooks repair` | command | yes | 477/411 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `hooks run` | command | yes | 300/326 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow hooks run [OPTIONS] HOOK_ID' rs='Usage: stackunderflow hooks run [OPTIONS] <HOOK_ID>' |
| `hooks status` | command | yes | 347/366 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `hooks uninstall` | command | yes | 276/279 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `import` | command | **no** | 1361/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest` | group | **no** | 329/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest github` | command | **no** | 1002/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest webhook` | group | **no** | 243/— | — | — | — | — | unported — the Rust binary has no such node |
| `ingest webhook serve` | command | **no** | 764/— | — | — | — | — | unported — the Rust binary has no such node |
| `init` | command | yes | 1343/1231 | ok | ok | ok | ok |  |
| `memory` | group | yes | 1177/1174 | ok | ok | ok | **DIFF** | summary differs only by Click's mid-word line wrap; 1 subcommand(s) not ported yet, excluded from the comparison: `embed`; usage py='Usage: stackunderflow memory [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow memory <COMMAND>' |
| `memory ask` | command | yes | 1639/2467 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory ask [OPTIONS] QUESTION' rs='Usage: stackunderflow memory ask [OPTIONS] <QUESTION>' |
| `memory decisions` | command | yes | 1300/2054 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory decisions [OPTIONS] QUERY' rs='Usage: stackunderflow memory decisions [OPTIONS] <QUERY>' |
| `memory embed` | command | **no** | 585/— | — | — | — | — | unported — the Rust binary has no such node |
| `memory file` | command | yes | 1369/2100 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory file [OPTIONS] PATH' rs='Usage: stackunderflow memory file [OPTIONS] <PATH>' |
| `memory sessions` | command | yes | 1514/2256 | ok | ok | ok | ok |  |
| `memory worked` | command | yes | 1339/2091 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow memory worked [OPTIONS] ACTION' rs='Usage: stackunderflow memory worked [OPTIONS] <ACTION>' |
| `month` | command | yes | 713/722 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `optimize` | command | yes | 1121/1414 | ok | ok | ok | ok |  |
| `plan` | group | yes | 411/540 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow plan <COMMAND>' |
| `plan reset` | command | yes | 117/91 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan reset [OPTIONS]' rs='Usage: stackunderflow plan reset' |
| `plan set` | command | yes | 457/415 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan set [OPTIONS] NAME' rs='Usage: stackunderflow plan set [OPTIONS] <NAME>' |
| `plan show` | command | yes | 203/240 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `plan thresholds` | group | yes | 368/398 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan thresholds [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow plan thresholds <COMMAND>' |
| `plan thresholds reset` | command | yes | 155/129 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan thresholds reset [OPTIONS]' rs='Usage: stackunderflow plan thresholds reset' |
| `plan thresholds set` | command | yes | 173/215 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow plan thresholds set [OPTIONS] VALUES...' rs='Usage: stackunderflow plan thresholds set <VALUES>...' |
| `plan thresholds show` | command | yes | 175/212 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `pricing` | group | **no** | 236/— | — | — | — | — | unported — the Rust binary has no such node |
| `pricing doctor` | command | **no** | 866/— | — | — | — | — | unported — the Rust binary has no such node |
| `recommend` | group | yes | 459/533 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow recommend [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow recommend <COMMAND>' |
| `recommend mode` | command | yes | 625/725 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow recommend mode [OPTIONS]' rs='Usage: stackunderflow recommend mode [OPTIONS] --prompt <TEXT>' |
| `recommend skills` | command | yes | 931/971 | ok | ok | ok | ok |  |
| `reindex` | command | **no** | 131/— | — | — | — | — | unported — the Rust binary has no such node |
| `report` | command | yes | 1245/1146 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `resume` | command | yes | 1085/1044 | ok | ok | ok | ok |  |
| `risk` | group | **no** | 423/— | — | — | — | — | unported — the Rust binary has no such node |
| `risk file` | command | **no** | 600/— | — | — | — | — | unported — the Rust binary has no such node |
| `search-past-decisions` | command | yes | 1509/1050 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow search-past-decisions [OPTIONS] QUERY' rs='Usage: stackunderflow search-past-decisions [OPTIONS] <QUERY>' |
| `skills` | group | yes | 553/618 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow skills [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow skills <COMMAND>' |
| `skills clean` | command | yes | 740/649 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `skills generate` | command | yes | 1696/1316 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `skills list` | command | yes | 469/451 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `start` | command | yes | 984/841 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `status` | command | yes | 718/617 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `sync` | group | yes | 494/552 | **DIFF** | ok | ok | **DIFF** | summary py='Encrypted, bring-your-own-bucket backup of your analytics aggregates (opt- in).' rs='Encrypted, bring-your-own-bucket backup of your analytics aggregates (opt-in)'; usage py='Usage: stackunderflow sync [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow sync <COMMAND>' |
| `sync init` | command | yes | 947/648 | **DIFF** | ok | ok | **DIFF** | summary py="Generate this device's encryption key and record the bucket destination. Prints the freshly generated key ONCE — save it, and copy it to your other devices. Only the key's fingerprint is stored in the database; the secret lives in a 0600 file (or the keychain / STACKUNDERFLOW_SYNC_KEY env var)." rs="Generate this device's encryption key and record the bucket destination"; usage py='Usage: stackunderflow sync init [OPTIONS]' rs='Usage: stackunderflow sync init [OPTIONS] --bucket <BUCKET_URL>' |
| `sync pull` | command | yes | 687/194 | **DIFF** | ok | ok | ok | summary py="Fetch and merge every OTHER device's encrypted aggregates from your bucket. Reads each peer's prefix (never writes to it), downloads only the shards that changed since the last pull, decrypts + verifies them, and lands them in the local remote tables. The unified cross-device view is then available at /api/sync/overview?scope=all-devices. Idempotent — an unchanged peer downloads nothing. Exits non-zero on a hard failure (e.g. bucket unreachable) so it is safe to script; per-peer/per-shard problems are reported as warnings, not fatal." rs="Fetch and merge every OTHER device's encrypted aggregates from your bucket" |
| `sync push` | command | yes | 274/127 | **DIFF** | ok | ok | **DIFF** | summary py='Encrypt and upload changed aggregate shards to your bucket. Idempotent — an unchanged shard is skipped (zero uploads). Exits non-zero on any failure so it is safe to script.' rs='Encrypt and upload changed aggregate shards to your bucket'; usage py='Usage: stackunderflow sync push [OPTIONS]' rs='Usage: stackunderflow sync push' |
| `sync status` | command | yes | 209/197 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `today` | command | yes | 708/717 | ok | ok | ok | ok | summary differs only by clap's stripped trailing `.` |
| `worktrees` | group | yes | 329/403 | ok | ok | ok | **DIFF** | summary differs only by clap's stripped trailing `.`; usage py='Usage: stackunderflow worktrees [OPTIONS] COMMAND [ARGS]...' rs='Usage: stackunderflow worktrees <COMMAND>' |
| `worktrees attribute` | command | yes | 424/426 | ok | ok | ok | **DIFF** | usage py='Usage: stackunderflow worktrees attribute [OPTIONS]' rs='Usage: stackunderflow worktrees attribute' |
| `worktrees list` | command | yes | 624/692 | ok | ok | ok | ok |  |
| `yield` | command | yes | 1423/869 | **DIFF** | ok | ok | ok | summary py='Yield analysis: productive vs reverted vs abandoned sessions. Cross-references each session\'s cwd with the git commit history of that repo over a 24-hour window after the session started. A session is "productive" if a non-reverted commit lands in that window, "reverted" if the commit was later reverted (or wiped from HEAD), "abandoned" if no commit followed, and "no_repo" if the cwd isn\'t a git repo. Heuristic warning: this correlates by time, not by content. A commit within 24h is credited to the session even if it\'s about something else.' rs='Yield analysis: productive vs reverted vs abandoned sessions' |

## The unported nodes

Reported so the count is honest: a differ that only walked what exists would have claimed a clean tree at wave 1 with nine commands ported.

```
analyze
analyze backfill
analyze quality
analyze session
discovery
discovery demote-uncited
discovery telemetry
doctor
etl
etl backfill
etl status
import
ingest
ingest github
ingest webhook
ingest webhook serve
memory embed
pricing
pricing doctor
reindex
risk
risk file
```


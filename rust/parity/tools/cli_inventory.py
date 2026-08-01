#!/usr/bin/env python3
"""Generate `rust/parity/CLI-INVENTORY.md` — the wave-8 command map.

Everything in the emitted document except the `STATUS` map below is extracted
from the live `click.Command` objects (`stackunderflow.cli:cli`) by walking
`cmd.params` / `cmd.commands`. No decorator is paraphrased: the option strings,
the resolved types, the `repr()` of every default, the `nargs` / `multiple` /
`required` / `hidden` flags and the verbatim `help` text all come out of Click
itself, so a drift between this file and `cli.py` is impossible without a
regeneration.

Run from the rust worktree root:

    PYTHONPATH=$PWD ../StackUnderflow/.venv/bin/python \\
        rust/parity/tools/cli_inventory.py rust/parity/CLI-INVENTORY.md

The `STATUS` map is the one piece of judgment — it is the architect's wave
assignment, and it is reviewed at each tranche gate.
"""

from __future__ import annotations

import sys
from pathlib import Path

import importlib.metadata
import click

from stackunderflow.cli import cli as ROOT

# ── the only judgment in this file: port state + wave assignment ─────────────
#
# status ∈ {PORTED, TRANCHE-1, PARTIAL, UNPORTED}
STATUS: dict[str, tuple[str, str, str]] = {
    "": ("PARTIAL", "W8-T1", "—"),
    # ── wave 1, landed and gated by rust/parity-cli.sh ───────────────────────
    "memory": ("PORTED", "W1", "RS-1-001"),
    "memory decisions": ("PORTED", "W1", "RS-1-001"),
    "memory file": ("PORTED", "W1", "RS-1-001"),
    "memory worked": ("PORTED", "W1", "RS-1-001"),
    "memory sessions": ("PORTED", "W1", "RS-1-001"),
    "memory ask": ("PORTED", "W1", "RS-1-014"),
    "memory embed": ("UNPORTED", "W8-T2", "RS-8-088*"),
    "resume": ("PORTED", "W1", "RS-1-002"),
    "find-failure-modes-for-file": ("PORTED", "W1", "RS-1-010"),
    "find-sessions-in-path": ("PORTED", "W1", "RS-1-011"),
    "find-sessions-touching-file": ("PORTED", "W1", "RS-1-012"),
    "find-sessions-where-action-worked": ("PORTED", "W1", "RS-1-013"),
    "search-past-decisions": ("PORTED", "W1", "RS-1-020"),
    # ── wave 8 tranche 1 — this pass ─────────────────────────────────────────
    "cfg": ("TRANCHE-1", "W8-T1", "RS-8-015"),
    "cfg ls": ("TRANCHE-1", "W8-T1", "RS-8-032"),
    "cfg set": ("TRANCHE-1", "W8-T1", "RS-8-037"),
    "cfg rm": ("TRANCHE-1", "W8-T1", "RS-8-036"),
    "cfg model-alias": ("TRANCHE-1", "W8-T1", "RS-8-016"),
    "cfg model-alias ls": ("TRANCHE-1", "W8-T1", "RS-8-033"),
    "cfg model-alias rm": ("TRANCHE-1", "W8-T1", "RS-8-034"),
    "cfg model-alias set": ("TRANCHE-1", "W8-T1", "RS-8-035"),
    "config": ("TRANCHE-1", "W8-T1", "RS-8-017"),
    "config show": ("TRANCHE-1", "W8-T1", "RS-8-041"),
    "config set": ("TRANCHE-1", "W8-T1", "RS-8-040"),
    "config unset": ("TRANCHE-1", "W8-T1", "RS-8-042"),
    "clear-cache": ("TRANCHE-1", "W8-T1", "RS-8-038"),
    "status": ("TRANCHE-1", "W8-T1", "RS-8-089*"),
    "backup": ("TRANCHE-1", "W8-T1", "RS-8-090*"),
    "backup list": ("TRANCHE-1", "W8-T1", "RS-8-091*"),
    "backup verify": ("TRANCHE-1", "W8-T1", "RS-8-092*"),
    # ── wave 8 tranche 2 — writers (rsync / launchd / network / installers) ──
    "backup create": ("UNPORTED", "W8-T2", "RS-8-093*"),
    "backup restore": ("UNPORTED", "W8-T2", "RS-8-094*"),
    "backup auto": ("UNPORTED", "W8-T2", "RS-8-095*, RS-8-080"),
    "sync": ("UNPORTED", "W8-T2", "RS-8-096*"),
    "sync init": ("UNPORTED", "W8-T2", "RS-8-097*"),
    "sync push": ("UNPORTED", "W8-T2", "RS-8-098*"),
    "sync pull": ("UNPORTED", "W8-T2", "RS-8-099*"),
    "sync status": ("UNPORTED", "W8-T2", "RS-8-100*"),
    "hooks": ("UNPORTED", "W8-T2", "RS-8-021"),
    "hooks install": ("UNPORTED", "W8-T2", "RS-8-054, RS-8-004"),
    "hooks repair": ("UNPORTED", "W8-T2", "RS-8-055, RS-8-005"),
    "hooks run": ("UNPORTED", "W8-T2", "RS-8-056"),
    "hooks status": ("UNPORTED", "W8-T2", "RS-8-057"),
    "hooks uninstall": ("UNPORTED", "W8-T2", "RS-8-058"),
    "guide": ("UNPORTED", "W8-T2", "RS-8-020"),
    "guide install": ("UNPORTED", "W8-T2", "RS-8-051, RS-8-001"),
    "guide status": ("UNPORTED", "W8-T2", "RS-8-052"),
    "guide uninstall": ("UNPORTED", "W8-T2", "RS-8-053"),
    "import": ("UNPORTED", "W8-T2", "RS-8-101*"),
    "reindex": ("UNPORTED", "W8-T2", "RS-8-102*"),
    "etl": ("UNPORTED", "W8-T2", "RS-8-103*"),
    "etl backfill": ("UNPORTED", "W8-T2", "RS-8-104*"),
    "etl status": ("UNPORTED", "W8-T2", "RS-8-105*"),
    "analyze": ("UNPORTED", "W8-T2", "RS-8-106*"),
    "analyze backfill": ("UNPORTED", "W8-T2", "RS-8-107*"),
    "discovery": ("UNPORTED", "W8-T2", "RS-8-018"),
    "discovery demote-uncited": ("UNPORTED", "W8-T2", "RS-8-045"),
    "discovery telemetry": ("UNPORTED", "W8-T2", "RS-8-046"),
    "pricing": ("UNPORTED", "W8-T2", "RS-8-108*"),
    "pricing doctor": ("UNPORTED", "W8-T2", "RS-8-109*"),
    # ── wave 8 tranche 3 — the spend + reports family ────────────────────────
    "report": ("UNPORTED", "W8-T3", "RS-8-070, RS-5-002"),
    "today": ("UNPORTED", "W8-T3", "RS-8-076, RS-5-002"),
    "month": ("UNPORTED", "W8-T3", "RS-8-060, RS-5-002"),
    "export": ("UNPORTED", "W8-T3", "RS-8-050, RS-5-005"),
    "compare": ("UNPORTED", "W8-T3", "RS-8-039"),
    "optimize": ("UNPORTED", "W8-T3", "RS-8-061, RS-5-007"),
    "yield": ("UNPORTED", "W8-T3", "RS-8-079"),
    "context-budget": ("UNPORTED", "W8-T3", "RS-8-043"),
    "context-replay": ("UNPORTED", "W8-T3", "RS-8-044"),
    "doctor": ("UNPORTED", "W8-T3", "RS-8-049"),
    "benchmark": ("UNPORTED", "W8-T3", "RS-8-014"),
    "benchmark show": ("UNPORTED", "W8-T3", "RS-8-031, RS-5-004"),
    "benchmark recommend": ("UNPORTED", "W8-T3", "RS-8-030, RS-5-004"),
    "plan": ("UNPORTED", "W8-T3", "RS-8-022"),
    "plan show": ("UNPORTED", "W8-T3", "RS-8-064"),
    "plan set": ("UNPORTED", "W8-T3", "RS-8-063"),
    "plan reset": ("UNPORTED", "W8-T3", "RS-8-062"),
    "plan thresholds": ("UNPORTED", "W8-T3", "RS-8-023"),
    "plan thresholds show": ("UNPORTED", "W8-T3", "RS-8-067"),
    "plan thresholds set": ("UNPORTED", "W8-T3", "RS-8-066"),
    "plan thresholds reset": ("UNPORTED", "W8-T3", "RS-8-065"),
    "risk": ("UNPORTED", "W8-T3", "RS-8-025"),
    "risk file": ("UNPORTED", "W8-T3", "RS-8-071, RS-8-011"),
    "analyze quality": ("UNPORTED", "W8-T3", "RS-8-028"),
    "analyze session": ("UNPORTED", "W8-T3", "RS-8-029"),
    "worktrees": ("UNPORTED", "W8-T3", "RS-8-027"),
    "worktrees list": ("UNPORTED", "W8-T3", "RS-8-078"),
    "worktrees attribute": ("UNPORTED", "W8-T3", "RS-8-077"),
    # ── wave 8 tranche 4 — skills / docs / recommend (large services) ────────
    "skills": ("UNPORTED", "W8-T4", "RS-8-026"),
    "skills list": ("UNPORTED", "W8-T4", "RS-8-074, RS-8-013"),
    "skills generate": ("UNPORTED", "W8-T4", "RS-8-073, RS-8-013"),
    "skills clean": ("UNPORTED", "W8-T4", "RS-8-072, RS-8-013"),
    "recommend": ("UNPORTED", "W8-T4", "RS-8-024"),
    "recommend mode": ("UNPORTED", "W8-T4", "RS-8-068"),
    "recommend skills": ("UNPORTED", "W8-T4", "RS-8-069, RS-8-012"),
    "docs": ("UNPORTED", "W8-T4", "RS-8-019"),
    "docs list": ("UNPORTED", "W8-T4", "RS-8-047, RS-8-002"),
    "docs show": ("UNPORTED", "W8-T4", "RS-8-048, RS-8-002"),
    # ── wave 7 — server boot and long-running processes ──────────────────────
    "start": ("UNPORTED", "W7", "RS-8-075"),
    "init": ("UNPORTED", "W7", "RS-8-059"),
    "ingest": ("UNPORTED", "W7", "RS-8-110*"),
    "ingest github": ("UNPORTED", "W7", "RS-8-111*, RS-5-020"),
    "ingest webhook": ("UNPORTED", "W7", "RS-8-112*"),
    "ingest webhook serve": ("UNPORTED", "W7", "RS-8-113*"),
}

WAVE_LEGEND = """| wave tag | meaning |
| --- | --- |
| `W1` | wave 1 — landed, gated every run by `rust/parity-cli.sh` |
| `W8-T1` | **this tranche** — read-only + config verbs, writers on case-local homes |
| `W8-T2` | tranche 2 — writers: rsync / launchd / network / installers (argv-differ pattern) |
| `W8-T3` | tranche 3 — the spend + reports family (`services::{aggregate,export,optimize,…}`) |
| `W8-T4` | tranche 4 — skills / docs / recommend (`skill_synth.py` 1256 ln, `embedded_docs.py` 306 ln) |
| `W7` | wave 7 — server boot and long-running processes (`start`, `init`, webhook serve) |"""


def esc(text) -> str:
    if text is None:
        return ""
    return str(text).replace("|", "\\|").replace("\n", " ").replace("\r", " ").strip()


def type_name(param) -> str:
    t = param.type
    if isinstance(t, click.Choice):
        return "choice[" + ", ".join(t.choices) + "]"
    if isinstance(t, click.IntRange):
        lo = "-inf" if t.min is None else t.min
        hi = "inf" if t.max is None else t.max
        return f"int range({lo}..{hi})"
    if isinstance(t, click.FloatRange):
        lo = "-inf" if t.min is None else t.min
        hi = "inf" if t.max is None else t.max
        return f"float range({lo}..{hi})"
    if isinstance(t, click.Path):
        return (
            f"path(file_okay={t.file_okay}, dir_okay={t.dir_okay}, exists={t.exists})"
        )
    return getattr(t, "name", type(t).__name__)


def spec_of(param) -> str:
    if isinstance(param, click.Argument):
        return param.name.upper()
    primary = " / ".join(param.opts)
    if param.secondary_opts:
        return primary + " \\| " + " / ".join(param.secondary_opts)
    return primary


def modifiers(param) -> str:
    out = []
    if param.required:
        out.append("required")
    if param.nargs != 1:
        out.append(f"nargs={param.nargs}")
    if param.multiple:
        out.append("multiple")
    if isinstance(param, click.Option):
        if param.is_flag:
            out.append("flag")
        if param.count:
            out.append("count")
        if param.hidden:
            out.append("hidden")
        if param.name != param.opts[0].lstrip("-").replace("-", "_"):
            out.append(f"dest={param.name}")
    return ", ".join(out) or "—"


def walk(cmd, path, acc):
    acc.append((path, cmd))
    if isinstance(cmd, click.Group):
        for name in sorted(cmd.commands):
            walk(cmd.commands[name], path + [name], acc)
    return acc


def main(out_path: Path) -> None:
    nodes = walk(ROOT, [], [])
    groups = [(p, c) for p, c in nodes if isinstance(c, click.Group)]
    leaves = [(p, c) for p, c in nodes if not isinstance(c, click.Group)]
    total_params = sum(len(c.params) for _, c in nodes)
    hidden = [" ".join(p) for p, c in nodes if getattr(c, "hidden", False)]

    def status(path):
        return STATUS.get(" ".join(path), ("UNPORTED", "UNASSIGNED", "—"))

    by_status: dict[str, int] = {}
    by_wave: dict[str, list] = {}
    for path, cmd in nodes:
        s, w, _ = status(path)
        by_status[s] = by_status.get(s, 0) + 1
        by_wave.setdefault(w, []).append((path, cmd))

    cli_py = Path("stackunderflow/cli.py")
    line_count = len(cli_py.read_text().splitlines()) if cli_py.is_file() else 0

    L: list[str] = []
    A = L.append

    A("# The wave-8 CLI inventory — every verb, flag, default and output shape")
    A("")
    A("**Generated, not written.** Every row is extracted from the live")
    A("`click.Command` objects by walking `cmd.params` / `cmd.commands`; no")
    A("decorator is paraphrased. Regenerate from the rust worktree root with:")
    A("")
    A("```")
    A("PYTHONPATH=$PWD ../StackUnderflow/.venv/bin/python \\")
    A("    rust/parity/tools/cli_inventory.py rust/parity/CLI-INVENTORY.md")
    A("```")
    A("")
    A(
        f"Reference: Click **{importlib.metadata.version("click")}**, CPython "
        f"**{sys.version.split()[0]}**, `stackunderflow/cli.py` at "
        f"**{line_count}** lines."
    )
    A("")
    A("## 1. Counts")
    A("")
    A(
        f"* **{len(nodes)}** nodes — **{len(groups)}** groups (including the root "
        f"`stackunderflow` group) and **{len(leaves)}** leaf commands."
    )
    A(
        f"* **{total_params}** declared parameters. Click's own `--help` (and the "
        f"root's `--version`) are added at `get_params()` time, so `--help` is "
        f"not counted here — it exists on all {len(nodes)} nodes."
    )
    A(
        "* **"
        + str(len(hidden))
        + "** hidden node(s): "
        + ", ".join(f"`{h}`" for h in hidden)
        + " — reachable, absent from every listing."
    )
    A("")
    A("### 1.1 By port status")
    A("")
    A("| status | nodes |")
    A("| --- | ---: |")
    for s in ("PORTED", "TRANCHE-1", "PARTIAL", "UNPORTED"):
        if s in by_status:
            A(f"| {s} | {by_status[s]} |")
    A(f"| **total** | **{len(nodes)}** |")
    A("")
    A("### 1.2 By wave assignment")
    A("")
    A("| wave | nodes | leaf commands | groups |")
    A("| --- | ---: | ---: | ---: |")
    for w in sorted(by_wave):
        ns = by_wave[w]
        lv = len([1 for p, c in ns if not isinstance(c, click.Group)])
        A(f"| `{w}` | {len(ns)} | {lv} | {len(ns) - lv} |")
    A(f"| **total** | **{len(nodes)}** | **{len(leaves)}** | **{len(groups)}** |")
    A("")
    A(WAVE_LEGEND)
    A("")
    A(
        "An RS item id marked `*` did **not** exist in `rust/TASKS-RS.md` when this "
        "inventory was first generated — building the inventory is what found the "
        "gap. See §4."
    )
    A("")
    A("## 2. The master table")
    A("")
    A("| # | path | kind | status | wave | RS item | params | summary |")
    A("| ---: | --- | --- | --- | --- | --- | ---: | --- |")
    for i, (path, cmd) in enumerate(nodes):
        s, w, item = status(path)
        label = " ".join(path) or "(root)"
        kind = "group" if isinstance(cmd, click.Group) else "command"
        if getattr(cmd, "hidden", False):
            kind += " · hidden"
        A(
            "| {} | `{}` | {} | {} | `{}` | {} | {} | {} |".format(
                i,
                label,
                kind,
                s,
                w,
                item,
                len(cmd.params),
                esc(cmd.get_short_help_str(limit=120)),
            )
        )
    A("")
    A("## 3. Per-node parameter detail")
    A("")
    A(
        "Columns: the literal option strings (`secondary_opts` after the `\\|` for "
        "`--x/--no-x` pairs), the parameter kind, Click's resolved type, the "
        "`default` exactly as `repr()` prints it, the modifiers Click records "
        "(`required` / `nargs` / `multiple` / `flag` / `hidden` / a `dest` that "
        "differs from the option spelling), and the verbatim `help` string."
    )
    A("")
    for path, cmd in nodes:
        s, w, item = status(path)
        label = " ".join(path) or "(root)"
        A(f"### `{label}` — {s} · {w} · {item}")
        A("")
        doc = (cmd.help or "").strip()
        if doc:
            A("> " + doc.replace("\n", "\n> "))
            A("")
        if isinstance(cmd, click.Group):
            A(
                "Subcommands: "
                + (", ".join(f"`{n}`" for n in sorted(cmd.commands)) or "—")
            )
            A("")
        if not cmd.params:
            A("*No declared parameters.*")
            A("")
            continue
        A("| spec | kind | type | default | modifiers | help |")
        A("| --- | --- | --- | --- | --- | --- |")
        for p in cmd.params:
            kind = "arg" if isinstance(p, click.Argument) else "opt"
            A(
                "| `{}` | {} | {} | `{}` | {} | {} |".format(
                    esc(spec_of(p)),
                    kind,
                    esc(type_name(p)),
                    esc(repr(p.default)),
                    esc(modifiers(p)),
                    esc(getattr(p, "help", None)),
                )
            )
        A("")

    A("## 4. Ledger gaps this inventory found")
    A("")
    A(
        "`rust/TASKS-RS.md`'s wave-8 block carries 87 items, of which "
        "RS-8-014..RS-8-079 name a command path. Cross-checking that list "
        "against the live tree leaves the following commands with **no item at "
        "all** — every one of them is a real, user-reachable verb:"
    )
    A("")
    A("| path | why it was missed |")
    A("| --- | --- |")
    gap_notes = {
        "status": "**the reserved verb.** DIV-025 renamed Rust's `status` to `store` precisely so this could be ported, and the item list never carried it",
        "backup": "the whole `backup` group (6 nodes) is absent — it is `cli.py`'s single largest command by body length (`create`, 123 lines)",
        "sync": "the whole `sync` group (5 nodes) is absent",
        "etl": "the whole `etl` group (3 nodes) is absent",
        "ingest": "the whole `ingest` group (4 nodes incl. the nested `webhook` group) is absent",
        "pricing": "the whole `pricing` group (2 nodes) is absent",
        "analyze": "the group node and `analyze backfill` are absent (`quality` / `session` have items)",
        "import": "absent",
        "reindex": "absent",
        "memory embed": "absent — the one `memory` verb wave 1 did not port",
    }
    for k, v in gap_notes.items():
        A(f"| `{k}` | {v} |")
    A("")
    A(
        "Filed as RS-8-088..RS-8-113 in `rust/TASKS-RS.md` (additive; no existing "
        "item was renumbered)."
    )
    A("")
    A("## 5. Output shapes — the tranche-1 verbs")
    A("")
    A(
        "Extracted from the command bodies rather than from Click, so this "
        "section is hand-maintained and scoped to what this tranche ported."
    )
    A("")
    A("| command | stdout shape | exit | writes |")
    A("| --- | --- | ---: | --- |")
    for row in [
        (
            "`cfg ls`",
            "`Settings:` then one `  {key:<34s}  {rendered:<14s}  [{src}]` line per key, `sorted(data)`; `rendered` is `json.dumps(v)` for a dict else `str(v)`; `src` ∈ env/file/default",
            "0",
            "—",
        ),
        (
            "`cfg ls --json`",
            "`json.dumps(get_all(), indent=2)` — **declaration order**, not sorted",
            "0",
            "—",
        ),
        (
            "`cfg set K V`",
            "`  {key} = {final}` where `final` is re-read after persist (currency uppercases)",
            "0",
            "`$HOME/config.json`",
        ),
        (
            "`cfg set` (bad key / dict key / `plan_*` key)",
            "Click `BadParameter` with `param_hint=KEY`: usage block + `Error: Invalid value for KEY: …`",
            "2",
            "—",
        ),
        (
            "`cfg rm K`",
            "`  {key} removed` — unconditionally, even for a key that was never set",
            "0",
            "`config.json` (**created** if absent — `_save` always writes)",
        ),
        (
            "`cfg model-alias ls`",
            "`No model aliases configured.` or `Model aliases:` + `  {src:<width}  ->  {dst}` per sorted source",
            "0",
            "—",
        ),
        (
            "`cfg model-alias ls --json`",
            "`json.dumps(aliases, indent=2, sort_keys=True)`",
            "0",
            "—",
        ),
        ("`cfg model-alias set S T`", "`  {source} -> {target}`", "0", "`config.json`"),
        (
            "`cfg model-alias rm S`",
            "`  {source} removed`, or `  no alias for {source!r}` (Python `repr`) and **no write**",
            "0",
            "`config.json` (only on a hit)",
        ),
        (
            "`config show|set|unset`",
            "`ctx.invoke` into `cfg ls` / `cfg set` / `cfg rm` — byte-identical output to the target",
            "as target",
            "as target",
        ),
        (
            "`clear-cache [PROJECT]`",
            "optional `  cursor parse cache cleared.` then two fixed lines; **PROJECT is accepted and ignored**",
            "0",
            "deletes `$HOME/cache/cursor-results.json`",
        ),
        (
            "`status`",
            "`today: $X.XX (N msg) \\| month: $Y.YY (M msg)`",
            "0",
            "`store.db` (schema apply; ingest when stale unless `--no-auto-ingest`)",
        ),
        (
            "`status --format json`",
            "`json.dumps({\"today\": report, \"month\": report}, indent=2, sort_keys=False)`",
            "0",
            "as above",
        ),
        (
            "`backup list`",
            "`  No backups yet. Run: stackunderflow backup create`, or `  N backup(s) in {dir}` + blank + `  {name}  ({files} files, {mb:.1f} MB)` per dir",
            "0",
            "—",
        ),
        (
            "`backup verify`",
            "`  Verifying {name}` + `    {artifact:<16} ok\\|MISSING` ×4 + a summary line",
            "0 / 1",
            "—",
        ),
    ]:
        A("| {} | {} | {} | {} |".format(*row))
    A("")

    out_path.write_text("\n".join(L) + "\n")
    print(f"wrote {out_path} — {len(L)} lines, {len(nodes)} nodes, {total_params} params")


if __name__ == "__main__":
    main(Path(sys.argv[1] if len(sys.argv) > 1 else "rust/parity/CLI-INVENTORY.md"))

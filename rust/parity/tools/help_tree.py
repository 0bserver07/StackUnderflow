#!/usr/bin/env python3
"""The `--help`-tree differ — wave 8's RS-8-014..027 / RS-8-087 measurement.

Dumps `--help` for **every** node of the Python command tree (105 of them) and
for the same node of the Rust binary, with `COLUMNS=80` pinned on both sides,
and reports what agrees.

Why it is not a plain `diff`
----------------------------
Click and clap render help through different templates, and the difference is
structural, not cosmetic (`rust/PARITY-wave1-resume.md` D-1 filed it at wave 1;
this run is what measures it):

    Click                                   clap
    Usage: … first                          the summary first
    summary indented two spaces             summary flush left, no trailing `.`
    no `Arguments:` section                 `Arguments:` section for positionals
    `--help  Show this message and exit.`   `-h, --help  Print help`
    subcommands sorted                      subcommands in declaration order
    no `help` subcommand                    a synthesised `help` subcommand
    every column wrapped at 80              option help not wrapped

A byte diff therefore says "different" for all 105 nodes and measures nothing.
What the wave-8 items actually ask for is *"same summary, same shared options,
same subcommand list"*, so this tool extracts those three facts from each help
text and compares them, while ALSO recording the raw byte delta so the layout
divergence is quantified rather than waved away.

Normalisation, scoped
---------------------
Exactly one substitution, on the Rust side only, on `Usage:` and `Try '…'`
lines: `stax` → `stackunderflow`. That is `parity-cli.sh`'s rule verbatim, and
for the same reason — a blanket substitution would rewrite help *text* that
legitimately contains the word. The summary comparison additionally strips one
trailing `.`, because clap's derive strips it from a doc comment and no
`about = "…"` override can be inferred from `cli.py`; every such strip is
counted and reported.

Usage
-----
    PYTHONPATH=$PWD ../StackUnderflow/.venv/bin/python \\
        rust/parity/tools/help_tree.py rust/parity/HELP-TREE.md

Exit code: 0 when every node the Rust binary implements agrees on all three
contract facts, 1 otherwise. Nodes the port has not reached are reported as
`unported`, never skipped silently.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

import click

from stackunderflow.cli import cli as ROOT

PROGRAM_PY = "stackunderflow"
PROGRAM_RS = "stax"

# Commands the port has and the reference does not, by ruling. `anchor` is the
# maintainer-ordered campaign-state feature (RS-1-029..033) and `store` is the
# wave-0 schema reader that DIV-025 renamed out of `status`'s way. Naming them
# here is the difference between "the differ knows about them" and "the differ
# has a blind spot": anything else that shows up as an extra is a real finding.
RUST_ONLY = {"anchor", "store"}

REPO = Path(__file__).resolve().parents[3]
PY_BIN = os.environ.get(
    "STAX_PARITY_PY_BIN", str(REPO.parent / "StackUnderflow" / ".venv" / "bin" / "stackunderflow")
)
RS_BIN = os.environ.get("STAX_PARITY_RS_BIN", str(REPO / "rust" / "target" / "release" / "stax"))

ENV = {
    **os.environ,
    "COLUMNS": "80",
    "LINES": "24",
    "LC_ALL": "C.UTF-8",
    "LANG": "C.UTF-8",
    "TERM": "dumb",
    "NO_COLOR": "1",
    "CLICOLOR": "0",
    "PYTHONIOENCODING": "utf-8",
    "PYTHONPATH": str(REPO),
}


def run(binary: str, path: list[str]) -> tuple[str, str, int]:
    proc = subprocess.run(
        [binary, *path, "--help"],
        capture_output=True,
        text=True,
        env=ENV,
        cwd=str(REPO),
        timeout=120,
    )
    return proc.stdout, proc.stderr, proc.returncode


def normalise_program(text: str) -> str:
    """The harness's SCOPED substitution: `Usage:` and `Try '…'` lines only."""
    out = []
    for line in text.splitlines(keepends=True):
        if line.startswith("Usage:") or line.startswith("Try '"):
            line = re.sub(rf"\b{PROGRAM_RS}\b", PROGRAM_PY, line)
        out.append(line)
    return "".join(out)


# ── the three contract facts ─────────────────────────────────────────────────


def _sections(text: str) -> dict[str, list[str]]:
    """Split a help text into its `Name:` blocks, keyed by the header."""
    blocks: dict[str, list[str]] = {}
    current = "_preamble"
    blocks[current] = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.endswith(":") and not line.startswith(" ") and len(stripped) > 1:
            current = stripped[:-1]
            blocks.setdefault(current, [])
            continue
        blocks[current].append(line)
    return blocks


def summary_of(text: str, *, click_style: bool) -> str:
    """The one-line description, whitespace-collapsed."""
    lines = text.splitlines()
    if click_style:
        # Click: `Usage: …`, blank, then the docstring indented two spaces.
        try:
            start = next(i for i, line in enumerate(lines) if line.startswith("Usage:"))
        except StopIteration:
            return ""
        # The docstring is the contiguous indented block that follows the
        # `Usage:` line, blank lines included — `--help` prints the WHOLE
        # docstring and clap's `--help` prints the whole `long_about`, so
        # stopping at the first blank paragraph break reported `resume` as
        # divergent when both sides carried the identical text.
        #
        # Two guards, both learned by getting them wrong: the block must START
        # immediately (a command with no docstring goes straight to `Options:`,
        # and skipping ahead to find something indented harvests the OPTION
        # TABLE as the summary), and it must END at the first non-indented
        # non-blank line (the next section header).
        rest = lines[start + 1 :]
        first = next((i for i, line in enumerate(rest) if line.strip()), None)
        if first is None or not rest[first].startswith("  "):
            return ""
        body: list[str] = []
        for line in rest[first:]:
            if not line.strip():
                continue
            if not line.startswith("  "):
                break
            body.append(line.strip())
        return " ".join(body)
    # clap: everything before the blank line that precedes `Usage:`.
    body = []
    for line in lines:
        if line.startswith("Usage:"):
            break
        if line.strip():
            body.append(line.strip())
    return " ".join(body)


_SPEC_SPLIT = re.compile(r"\s{2,}")
_LONG_OPT = re.compile(r"--[A-Za-z0-9][A-Za-z0-9-]*")


def options_of(text: str) -> set[str]:
    """Every long option in the `Options:` block's SPEC column.

    Scanning the whole block would pick up options *named inside help prose*
    (`status`'s help says "Disable with --no-auto-ingest"), which is how a
    differ starts agreeing by accident. So only the spec column — the text
    before the first run of two or more spaces on a line that starts an entry —
    is scanned, and `--help` itself is dropped because the two frameworks
    spell it differently by construction.
    """
    found: set[str] = set()
    for line in _sections(text).get("Options", []):
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        body = line.strip()
        if not body.startswith("-"):
            continue
        if indent > 8:
            # A continuation of the previous entry's help column.
            continue
        spec = _SPEC_SPLIT.split(body, maxsplit=1)[0]
        found.update(_LONG_OPT.findall(spec))
    found.discard("--help")
    return found


def commands_of(text: str) -> set[str]:
    """Every subcommand name in the `Commands:` block."""
    found: set[str] = set()
    for line in _sections(text).get("Commands", []):
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent != 2:
            continue
        found.add(line.strip().split()[0])
    # clap synthesises a `help` subcommand Click has no equivalent for.
    found.discard("help")
    return found


def collapse(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


_HYPHEN_WRAP = re.compile(r"(?<=[A-Za-z0-9])- (?=[A-Za-z0-9])")


def unwrap_hyphens(text: str) -> str:
    """Undo Click's line break *inside* a hyphenated word.

    Click rewraps the docstring at 80 columns and will break `token-bounded`
    into `token-` + `bounded`; collapsing the newline then leaves `token- `,
    which is a WRAPPING artifact and nothing else. Undoing it is deterministic
    (a hyphen followed by a space between two word characters cannot survive
    Click's own wrapper unless the source had it), and every application is
    counted and reported alongside the trailing-`.` strips — a normalisation
    nobody can see is a normalisation that hides a bug.
    """
    return _HYPHEN_WRAP.sub("-", text)


# ── the walk ─────────────────────────────────────────────────────────────────


def walk(cmd, path, acc):
    acc.append((path, cmd))
    if isinstance(cmd, click.Group):
        for name in sorted(cmd.commands):
            walk(cmd.commands[name], path + [name], acc)
    return acc


def main(out_path: Path) -> int:
    nodes = walk(ROOT, [], [])

    # Which nodes does the Rust binary actually implement? Answered FIRST,
    # because a group's subcommand list can only be judged against the
    # subcommands that exist: `backup` is missing `create` / `restore` / `auto`
    # because tranche 2 has not run, which is a *coverage* fact already counted
    # in CLI-INVENTORY.md, not a help-tree divergence. Conflating the two would
    # make this report say "8 divergent" every wave until the last command
    # lands, which is a number nobody can act on.
    implemented: set[str] = set()
    for path, _cmd in nodes:
        _out, _err, rc = run(RS_BIN, path)
        if rc == 0 and _out.strip():
            implemented.add(" ".join(path))

    rows = []
    strips = 0
    unwraps = 0
    for path, cmd in nodes:
        py_out, py_err, py_rc = run(PY_BIN, path)
        rs_out, rs_err, rs_rc = run(RS_BIN, path)
        rs_out = normalise_program(rs_out)
        rs_err = normalise_program(rs_err)

        ported = rs_rc == 0 and bool(rs_out.strip())
        row = {
            "path": " ".join(path) or "(root)",
            "is_group": isinstance(cmd, click.Group),
            "ported": ported,
            "py_bytes": len(py_out.encode()),
            "rs_bytes": len(rs_out.encode()) if ported else 0,
            "identical": False,
            "summary_ok": None,
            "options_ok": None,
            "commands_ok": None,
            "usage_ok": None,
            "notes": [],
        }
        if not ported:
            row["notes"].append("unported — the Rust binary has no such node")
            rows.append(row)
            continue

        row["identical"] = py_out == rs_out

        py_summary = collapse(summary_of(py_out, click_style=True))
        rs_summary = collapse(summary_of(rs_out, click_style=False))
        if py_summary != rs_summary and unwrap_hyphens(py_summary) == unwrap_hyphens(rs_summary):
            unwraps += 1
            row["notes"].append("summary differs only by Click's mid-word line wrap")
            py_summary = rs_summary = unwrap_hyphens(rs_summary)
        if py_summary != rs_summary and py_summary.rstrip(".") == rs_summary.rstrip("."):
            strips += 1
            row["notes"].append("summary differs only by clap's stripped trailing `.`")
            row["summary_ok"] = True
        else:
            row["summary_ok"] = py_summary == rs_summary
        if not row["summary_ok"]:
            row["notes"].append(f"summary py={py_summary!r} rs={rs_summary!r}")

        py_opts, rs_opts = options_of(py_out), options_of(rs_out)
        row["options_ok"] = py_opts == rs_opts
        if not row["options_ok"]:
            missing = sorted(py_opts - rs_opts)
            extra = sorted(rs_opts - py_opts)
            if missing:
                row["notes"].append("options missing: " + ", ".join(f"`{o}`" for o in missing))
            if extra:
                row["notes"].append("options extra: " + ", ".join(f"`{o}`" for o in extra))

        py_cmds, rs_cmds = commands_of(py_out), commands_of(rs_out)
        prefix = " ".join(path)
        expected = {
            name
            for name in py_cmds
            if (f"{prefix} {name}".strip()) in implemented
        }
        deferred = sorted(py_cmds - expected)
        row["commands_ok"] = expected == (rs_cmds - RUST_ONLY)
        if rs_cmds & RUST_ONLY:
            row["notes"].append(
                "Rust-only by ruling: "
                + ", ".join(f"`{c}`" for c in sorted(rs_cmds & RUST_ONLY))
            )
        if deferred:
            row["notes"].append(
                f"{len(deferred)} subcommand(s) not ported yet, excluded from the "
                "comparison: " + ", ".join(f"`{c}`" for c in deferred)
            )
        if not row["commands_ok"]:
            missing = sorted(expected - rs_cmds)
            extra = sorted(rs_cmds - py_cmds - RUST_ONLY)
            if missing:
                row["notes"].append("subcommands missing: " + ", ".join(f"`{c}`" for c in missing))
            if extra:
                row["notes"].append(
                    "subcommands the reference does not have: "
                    + ", ".join(f"`{c}`" for c in extra)
                )

        py_usage = collapse(next((l for l in py_out.splitlines() if l.startswith("Usage:")), ""))
        rs_usage = collapse(next((l for l in rs_out.splitlines() if l.startswith("Usage:")), ""))
        row["usage_ok"] = py_usage == rs_usage
        if not row["usage_ok"]:
            row["notes"].append(f"usage py={py_usage!r} rs={rs_usage!r}")
        rows.append(row)

    ported = [r for r in rows if r["ported"]]
    clean = [
        r
        for r in ported
        if r["summary_ok"] and r["options_ok"] and r["commands_ok"]
    ]
    identical = [r for r in ported if r["identical"]]
    failures = [r for r in ported if r not in clean]

    write_report(out_path, rows, ported, clean, identical, failures, strips, unwraps, len(nodes))
    print(
        f"help-tree: {len(nodes)} nodes · {len(ported)} ported · "
        f"{len(clean)} contract-clean · {len(identical)} byte-identical · "
        f"{len(failures)} contract-divergent"
    )
    return 0 if not failures else 1


def write_report(out_path, rows, ported, clean, identical, failures, strips, unwraps, total):
    L: list[str] = []
    A = L.append
    A("# The `--help`-tree differ — measurement, not a wish")
    A("")
    A("**Generated.** Regenerate from the rust worktree root:")
    A("")
    A("```")
    A("rust/help-tree.sh          # or: rust/parity/tools/help_tree.py rust/parity/HELP-TREE.md")
    A("```")
    A("")
    A("## Verdict")
    A("")
    A(
        f"* **{total}** nodes in the Python tree; **{len(ported)}** exist in the Rust "
        f"binary today (the other **{total - len(ported)}** are unported — listed "
        f"below by name, never skipped silently)."
    )
    A(
        f"* **{len(identical)} / {len(ported)}** are byte-identical after the scoped "
        f"program-name substitution."
    )
    A(
        f"* **{len(clean)} / {len(ported)}** agree on all three contract facts the "
        f"wave-8 items name — *same summary, same options, same subcommand list*."
    )
    A(f"* **{len(failures)}** ported nodes disagree on a contract fact.")
    A(
        f"* clap's stripped trailing `.` accounted for **{strips}** summary "
        f"differences and Click's mid-word 80-column wrap for **{unwraps}** more; "
        f"both were normalised away and both are counted here, not hidden."
    )
    A("")
    A("## D-1, measured and re-filed")
    A("")
    A(
        "`rust/PARITY-wave1-resume.md` filed D-1 at wave 1 as \"Click wraps at 80 "
        "columns with a two-column option table; clap prints its own layout\" and "
        "deferred the measurement to this wave. Here it is. The two templates "
        "differ in **eight structural ways**, and only three of them are reachable "
        "by tuning a clap option:"
    )
    A("")
    A("| # | Click | clap 4.5 | fixable without a custom template? |")
    A("| ---: | --- | --- | --- |")
    for index, row in enumerate([
        ("`Usage:` first, summary second", "summary first, `Usage:` second", "no"),
        ("summary indented two spaces", "summary flush left", "no"),
        ("summary keeps its trailing `.`", "derive strips one trailing `.`", "yes — `about = \"…\"` on all 105 nodes"),
        ("no `Arguments:` section", "`Arguments:` section for positionals", "no"),
        ("`--help  Show this message and exit.`", "`-h, --help  Print help`", "partly — `-h` can be dropped, the text cannot"),
        ("subcommands listed **sorted**", "subcommands in declaration order", "yes — declare alphabetically"),
        ("no `help` subcommand", "a synthesised `help` subcommand", "yes — `disable_help_subcommand`"),
        ("every column wrapped to 80", "option help not wrapped", "no"),
    ], start=1):
        A("| {} | {} | {} | {} |".format(index, *row))
    A("")
    A(
        "**Ruling requested.** Byte-parity on `--help` is reachable only by "
        "replacing clap's renderer with a hand-written Click-shaped template "
        "(`Command::help_template` plus a per-node `about`), which means the port "
        "carries a second help engine whose only consumer is a differ. The "
        "cheaper contract — *same summary, same options, same subcommand list*, "
        "which is what RS-8-014..027 actually specify — is what this tool gates, "
        "and it is what the table below reports. Filed as **DIV-240**; the "
        "maintainer decides whether the byte-level goal is worth a template."
    )
    A("")
    A("## Per-node status")
    A("")
    A("`—` means the fact does not apply to that node (a leaf has no `Commands:`).")
    A("")
    A("| path | kind | ported | bytes py/rs | summary | options | subcommands | usage | notes |")
    A("| --- | --- | --- | ---: | :---: | :---: | :---: | :---: | --- |")

    def tick(value):
        if value is None:
            return "—"
        return "ok" if value else "**DIFF**"

    for row in rows:
        A(
            "| `{path}` | {kind} | {ported} | {pb}/{rb} | {s} | {o} | {c} | {u} | {notes} |".format(
                path=row["path"],
                kind="group" if row["is_group"] else "command",
                ported="yes" if row["ported"] else "**no**",
                pb=row["py_bytes"],
                rb=row["rs_bytes"] or "—",
                s=tick(row["summary_ok"]),
                o=tick(row["options_ok"]),
                c=tick(row["commands_ok"]),
                u=tick(row["usage_ok"]),
                notes="; ".join(row["notes"]).replace("|", "\\|"),
            )
        )
    A("")
    A("## The unported nodes")
    A("")
    A(
        "Reported so the count is honest: a differ that only walked what exists "
        "would have claimed a clean tree at wave 1 with nine commands ported."
    )
    A("")
    unported = [r["path"] for r in rows if not r["ported"]]
    A("```")
    for name in unported:
        A(name)
    A("```")
    A("")
    out_path.write_text("\n".join(L) + "\n")


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1] if len(sys.argv) > 1 else "rust/parity/HELP-TREE.md")))

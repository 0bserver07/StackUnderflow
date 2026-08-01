#!/usr/bin/env python
"""Capture the launchd plist `backup auto --enable` writes on macOS.

Run on a Linux host, against the REAL `cli.py`. `platform.system` is faked to
``"Darwin"``, ``Path.home()`` is pointed at a scratch directory, and
``subprocess.run`` is stubbed so ``launchctl`` is never spawned — nothing here
touches a real ``~/Library/LaunchAgents`` or a real launchd, on any platform.

The point is the *file*: whatever the reference writes is the golden that
`crates/stax-cli/tests/plist_golden.rs` diffs `backup::darwin_plist` against,
byte for byte. Paraphrasing the f-string into a fixture would have proved
nothing except that the paraphrase matched itself.

    python rust/parity/backup_auto_plist.py <scratch-home> <su-bin> <out-plist>

Prints the command's stdout to stdout so the differ can compare that too.
"""
from __future__ import annotations

import os
import pathlib
import platform
import shutil
import subprocess
import sys
import types
from unittest import mock


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    scratch, su_bin, out_plist = (pathlib.Path(sys.argv[1]), sys.argv[2], pathlib.Path(sys.argv[3]))
    scratch.mkdir(parents=True, exist_ok=True)

    # `_STATE_DIR` is resolved at import time from `$STACKUNDERFLOW_HOME`.
    os.environ.setdefault("STACKUNDERFLOW_HOME", str(scratch / "state"))

    from click.testing import CliRunner

    import stackunderflow.cli as cli

    spawned: list[list[str]] = []

    def fake_run(argv, *args, **kwargs):
        spawned.append([str(part) for part in argv])
        return types.SimpleNamespace(returncode=0, stdout=b"", stderr=b"")

    with (
        mock.patch.object(platform, "system", lambda: "Darwin"),
        mock.patch.object(pathlib.Path, "home", staticmethod(lambda: scratch)),
        mock.patch.object(subprocess, "run", fake_run),
        mock.patch.object(shutil, "which", lambda name: su_bin if name == "stackunderflow" else None),
    ):
        result = CliRunner().invoke(cli.cli, ["backup", "auto", "--enable"])

    if result.exception is not None and not isinstance(result.exception, SystemExit):
        raise result.exception

    written = scratch / "Library" / "LaunchAgents" / "com.stackunderflow.backup.plist"
    if not written.is_file():
        print(f"backup_auto_plist: the reference wrote no plist at {written}", file=sys.stderr)
        return 1
    out_plist.parent.mkdir(parents=True, exist_ok=True)
    out_plist.write_bytes(written.read_bytes())

    sys.stdout.write(result.output)
    for argv in spawned:
        print(f"SPAWNED: {' '.join(argv)}", file=sys.stderr)
    return result.exit_code


if __name__ == "__main__":
    raise SystemExit(main())

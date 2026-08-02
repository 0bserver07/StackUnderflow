#!/usr/bin/env python3
"""Build the seeded `$STACKUNDERFLOW_HOME` trees the telephone differ runs on.

The inbox is files and nothing else — `msg inbox` never opens the store, and
`render_for_injection` works on a machine that has never ingested anything — so
these seeds are a directory of small JSON files and no database. That is the
point: the telephone's read path has no store dependency, and a differ that
required one would be testing something the feature does not do.

Every seed is written with EXPLICIT bytes (`wb`), never through `json.dumps`.
The reference's writer is what is under test on the send side; on the read side
what matters is that both implementations parse the same bytes, including the
bytes no writer would ever produce.

Seeds
-----
empty       no `inbox/` at all — the floor
plain       two senders, three unseen, one already `.seen.json`
unicode     an em-dash, CJK, an astral emoji, a combining mark, a tab
corrupt     truncated JSON, a top-level list, `{}`, non-string fields,
            a dotfile, a hidden sender dir, an in-flight `.part`
many        five unseen — crosses MAX_INJECT and the "… N more" line
long        one message at exactly 220 chars and one at 221 — the excerpt edge
sortorder   senders `mac` and `mac-pro`, which order differently under a
            full-path string sort than under a (parent, name) sort

Usage:  build_telephone_state.py <out-dir> [--force]
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

# The excerpt cap in `agent_inbox`. Duplicated deliberately: if the reference
# moves it, the `long` seed stops straddling the edge and the differ's `long`
# rows go quiet — so the constant is asserted below rather than imported.
TEXT_CHARS = 220


def msg(mid: str, sender: str, ts: str, text: str) -> bytes:
    """One message file's bytes, in the reference's key order."""
    def enc(value: str) -> str:
        out = value
        for raw, escaped in (("\\", "\\\\"), ('"', '\\"'), ("\n", "\\n"), ("\t", "\\t")):
            out = out.replace(raw, escaped)
        return f'"{out}"'

    return (
        "{"
        f'"id": {enc(mid)}, "from": {enc(sender)}, '
        f'"ts": {enc(ts)}, "text": {enc(text)}'
        "}"
    ).encode()


def write(home: Path, sender: str, name: str, body: bytes) -> None:
    target = home / "inbox" / sender / name
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(body)


def seed_empty(home: Path) -> None:
    home.mkdir(parents=True, exist_ok=True)


def seed_plain(home: Path) -> None:
    write(home, "mac", "0018000000001-aaaaaa.json",
          msg("0018000000001-aaaaaa", "mac", "2026-08-01T09:00:00-0400", "ship the differ"))
    write(home, "mac", "0018000000002-bbbbbb.json",
          msg("0018000000002-bbbbbb", "mac", "2026-08-01T09:05:00-0400", "second from mac"))
    write(home, "linux-box", "0018000000003-cccccc.json",
          msg("0018000000003-cccccc", "linux-box", "2026-08-01T13:10:00+0000", "from the fleet"))
    write(home, "linux-box", "0018000000000-dddddd.seen.json",
          msg("0018000000000-dddddd", "linux-box", "2026-08-01T08:00:00+0000", "already read"))


def seed_unicode(home: Path) -> None:
    write(home, "mac", "0018000000010-aaaaaa.json",
          msg("0018000000010-aaaaaa", "mac", "2026-08-01T09:00:00-0400",
              "an em-dash — a café, 日本語, and \U0001f680 astral"))
    write(home, "mac", "0018000000011-bbbbbb.json",
          msg("0018000000011-bbbbbb", "mac", "2026-08-01T09:01:00-0400",
              "combining é and a\ttab"))
    # A sender name that is itself non-ASCII: it reaches the output through the
    # `from` field AND through the directory name in the fallback path.
    write(home, "mächine", "0018000000012-cccccc.json",
          msg("0018000000012-cccccc", "mächine", "2026-08-01T09:02:00+0200", "grüße"))


def seed_corrupt(home: Path) -> None:
    write(home, "mac", "0018000000020-aaaaaa.json", b"{not json at all")
    write(home, "mac", "0018000000021-bbbbbb.json", b"[1, 2, 3]")
    write(home, "mac", "0018000000022-cccccc.json", b"{}")
    write(home, "mac", "0018000000023-dddddd.json",
          b'{"id": 5, "from": true, "ts": 1.5, "text": [1, "x"]}')
    write(home, "mac", "0018000000024-eeeeee.json", b"")
    write(home, "mac", "0018000000025-ffffff.json",
          b'{"id": "", "from": "", "ts": "", "text": ""}')
    # Neither implementation's glob sees a leading dot, nor a `.part`.
    write(home, "mac", ".draft.json", b'{"id": "hidden", "text": "invisible"}')
    write(home, ".hidden", "0018000000026-111111.json",
          msg("x", ".hidden", "T", "in a hidden sender dir"))
    write(home, "mac", "0018000000027-222222.part",
          msg("x", "mac", "T", "an in-flight put"))
    # A real message, so a corrupt neighbour is proven not to block the channel.
    write(home, "mac", "0018000000028-333333.json",
          msg("0018000000028-333333", "mac", "2026-08-01T10:00:00-0400", "survivor"))


def seed_many(home: Path) -> None:
    for index in range(5):
        write(home, "mac", f"001800000003{index}-aaaaa{index}.json",
              msg(f"001800000003{index}-aaaaa{index}", "mac",
                  f"2026-08-01T10:0{index}:00-0400", f"message number {index}"))


def seed_long(home: Path) -> None:
    assert TEXT_CHARS == 220, "the excerpt edge moved; re-derive these two seeds"
    write(home, "mac", "0018000000040-aaaaaa.json",
          msg("0018000000040-aaaaaa", "mac", "2026-08-01T11:00:00-0400", "a" * TEXT_CHARS))
    write(home, "mac", "0018000000041-bbbbbb.json",
          msg("0018000000041-bbbbbb", "mac", "2026-08-01T11:01:00-0400", "b" * (TEXT_CHARS + 1)))
    # 221 CHARACTERS of a 3-byte character: 663 bytes. A byte-counted clip cuts
    # this at 220 bytes and lands mid-character; a char-counted one does not.
    write(home, "mac", "0018000000042-cccccc.json",
          msg("0018000000042-cccccc", "mac", "2026-08-01T11:02:00-0400", "—" * (TEXT_CHARS + 1)))


def seed_sortorder(home: Path) -> None:
    write(home, "mac", "zzz.json", msg("zzz", "mac", "T-mac", "from mac"))
    write(home, "mac-pro", "aaa.json", msg("aaa", "mac-pro", "T-macpro", "from mac-pro"))
    write(home, "mac2", "mmm.json", msg("mmm", "mac2", "T-mac2", "from mac2"))


SEEDS = {
    "empty": seed_empty,
    "plain": seed_plain,
    "unicode": seed_unicode,
    "corrupt": seed_corrupt,
    "many": seed_many,
    "long": seed_long,
    "sortorder": seed_sortorder,
}


def main(argv: list[str]) -> int:
    if not argv:
        sys.stderr.write(__doc__ or "")
        return 2
    out = Path(argv[0])
    force = "--force" in argv[1:]
    if out.exists() and not force:
        sys.stderr.write(f"{out} exists (pass --force to rebuild)\n")
        return 1
    if out.exists():
        shutil.rmtree(out)
    for name, builder in SEEDS.items():
        home = out / name
        home.mkdir(parents=True, exist_ok=True)
        builder(home)
    (out / ".built").write_text("\n".join(sorted(SEEDS)) + "\n")
    print(f"telephone seeds: {len(SEEDS)} homes under {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

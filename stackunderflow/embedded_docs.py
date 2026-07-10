"""Offline documentation, embedded in the installed package.

``stackunderflow docs list`` / ``stackunderflow docs show <topic>`` (and the
``stax`` short alias) read from here — no network, no repo checkout, no running
server. The content is embedded as **string constants**, not loaded from the
repo's top-level ``docs/`` tree: that tree lives outside the ``stackunderflow``
package and is not guaranteed to ship in a wheel, whereas Python source always
does. So these pages are available from *any* install, fully offline.

Each topic is **audience-tagged** (``agent`` / ``user`` / ``all``) so an agent
can filter to the pages written for it (``docs list --audience agent``).

One topic — ``support-matrix`` — is rendered live from
:mod:`stackunderflow.services.support_matrix` so it can never drift from the
adapters actually installed. All others are static text.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

AUDIENCES: tuple[str, ...] = ("all", "agent", "user")


@dataclass(frozen=True)
class Doc:
    """One embedded documentation page.

    Exactly one of ``text`` / ``renderer`` is set: static pages carry ``text``;
    the live pages carry a zero-arg ``renderer`` computed on demand.
    """

    slug: str
    title: str
    audience: str
    summary: str
    text: str | None = None
    renderer: Callable[[], str] | None = None

    def body(self) -> str:
        """Return the page body, computing it if this page is rendered live."""
        if self.renderer is not None:
            return self.renderer().rstrip() + "\n"
        return (self.text or "").strip("\n") + "\n"


# ── static content ───────────────────────────────────────────────────────────

_OVERVIEW = """
# StackUnderflow

StackUnderflow is a local-first knowledge base for your AI coding sessions. It
ingests the on-disk transcripts your coding tools already write, normalizes them
into one store, and turns them into cost analytics, session history, and — the
part that matters to an agent — a queryable memory of what you've decided,
what's broken before, and what worked.

Everything runs on your machine. The store is a single SQLite database at
`~/.stackunderflow/store.db`; nothing is sent anywhere.

## The pieces

- **Adapters** read each source tool's transcripts (see the `adapters` topic and
  the live `support-matrix`).
- **The dashboard** (`stackunderflow start`) is a local web UI for cost,
  sessions, projects, forks, and more.
- **The memory CLI** (`stackunderflow memory ...`, see the `memory` topic) is the
  agent-facing surface: ask the store questions from inside a coding session.
- **doctor** (see the `doctor` topic) is a read-only health check for the store.

## Where to start

- New here? See the `quickstart` topic.
- Writing an agent integration? See the `memory` topic.
- Care about what leaves your machine? See the `privacy` topic.
"""

_QUICKSTART = """
# Quickstart

## Install and launch

Once installed, launch the dashboard:

    stackunderflow start

It serves a local web UI (default `http://127.0.0.1:8081`) and opens your
browser. Add `--headless` to skip the browser, `--port` / `--host` to change the
bind address, and `--fresh` to clear the disk cache first. Run
`stackunderflow start --help` for the full list.

On first launch StackUnderflow discovers the transcripts your enabled adapters
can see and builds the store at `~/.stackunderflow/store.db`.

## Everyday commands

- `stackunderflow start` — the dashboard.
- `stackunderflow memory ...` — ask the store questions from the terminal (see
  the `memory` topic).
- `stackunderflow doctor` — read-only store health check (see the `doctor`
  topic).
- `stackunderflow backup create` / `list` / `restore` — snapshot the store.
- `stackunderflow cfg ls` / `set` / `rm` — inspect and change configuration.
- `stackunderflow resume [PATH]` — session/resume ids for every coding agent
  under a path (default cwd), with each agent's real resume command rendered
  (e.g. `claude --resume <id>`, `codex resume <id>`). `--json` for agents.

Every command supports `--help`. The short alias `stax` runs the same CLI:
`stax start`, `stax doctor`, `stax docs list`.

## Adapters

Every supported coding agent's adapter is enabled by default — there are no
opt-in flags. The live `support-matrix` topic lists each adapter and the
fidelity of what it captures.
"""

_MEMORY = """
# Memory CLI — query your past coding sessions

`stackunderflow memory` is the agent-facing namespace. Before re-deriving
something, ask whether the answer is already recorded. Every query is local and
read-only — nothing leaves the machine.

## Commands

- `stackunderflow memory file <path>` — a file's history: past edits, failure
  modes, and the sessions that touched it. Worth a look before a non-trivial
  edit.
- `stackunderflow memory decisions "<topic>"` — past decisions on a topic.
- `stackunderflow memory worked "<action>"` — past sessions where an action
  succeeded, with evidence.
- `stackunderflow memory sessions` — recent sessions in this project.
- `stackunderflow memory ask "<question>"` — natural-language query over history.
- `stackunderflow resume [PATH] --json` (`-p <agent>` to narrow) — session/
  resume ids for EVERY coding
  agent under a path (claude, codex, grok, …), each with its real resume
  invocation rendered (`claude --resume <id>`, `codex resume <id>`). Use it
  when the user wants to pick up prior work in some tool; present the command,
  don't launch interactive CLIs yourself.

## JSON for programmatic callers

Pass `--format json` (or `--json` where offered) for a stable, token-bounded
envelope tagged `schema: stackunderflow.memory/1`. The envelope's outer shape is
a versioned, conformance-tested contract — safe to parse from a hook or another
tool. Prefer text for a human reading the terminal; JSON is more expensive in
tokens, so reach for it only when a program consumes the output.

## Citations

Results carry the session and file they came from. When you act on a memory
result, cite the evidence rather than asserting it — the store records what
happened, not a guarantee about what will.
"""

_DOCTOR = """
# doctor — read-only store health check

`stackunderflow doctor` (short: `stax doctor`) checks the integrity of your
store without changing anything. It opens `~/.stackunderflow/store.db`
**read-only**: it never migrates, never writes, and never repairs.

## What it checks

- **Integrity** — SQLite's own `integrity_check` for page/index corruption.
- **Foreign keys** — dangling references across the declared relationships
  (projects → sessions → messages → events).
- **Watermarks** — that no mart claims to have processed an event id newer than
  the newest event that exists (a sign a rebuild was interrupted).
- **Orphans** — denormalized mart rows that point at a project that is no longer
  present.

## Output

By default it prints `ok`, or one finding per line. With `--json` it prints
`{"ok": <bool>, "findings": [...], "store_path": "..."}`. It exits non-zero when
there are findings, so it drops cleanly into a script or CI step.

A missing store is reported as a finding, not a crash — so a fresh machine with
no store yet gets a clear message instead of a traceback.
"""

_PRIVACY = """
# Privacy — local-first by construction

StackUnderflow is built to keep your data on your machine.

- **The store is local.** Everything lives in one SQLite file at
  `~/.stackunderflow/store.db`, built from transcripts already on disk.
- **The memory CLI is read-only and offline.** `stackunderflow memory ...` and
  `stackunderflow doctor` open the store locally and send nothing over the
  network.
- **doctor never writes.** It opens the store read-only; it can report a problem
  but it will not migrate, repair, or otherwise change your data.
- **Backups stay local.** `stackunderflow backup` snapshots the store under
  `~/.stackunderflow/backups/`.

Some features can be pointed at a network endpoint that you configure (for
example, an optional embedding backend for semantic search). Those are opt-in and
governed by environment variables you set; the default posture is fully local.
"""


def _render_support_matrix() -> str:
    # Imported lazily so ``embedded_docs`` stays cheap to import and free of an
    # adapters import cycle.
    from stackunderflow.services import support_matrix

    return support_matrix.render_markdown()


_DOCS: tuple[Doc, ...] = (
    Doc(
        slug="overview",
        title="StackUnderflow overview",
        audience="all",
        summary="What StackUnderflow is and how the pieces fit together.",
        text=_OVERVIEW,
    ),
    Doc(
        slug="quickstart",
        title="Quickstart",
        audience="user",
        summary="Install, launch the dashboard, and the everyday commands.",
        text=_QUICKSTART,
    ),
    Doc(
        slug="memory",
        title="Memory CLI",
        audience="agent",
        summary="Query past sessions from inside a coding session (agent-facing).",
        text=_MEMORY,
    ),
    Doc(
        slug="support-matrix",
        title="Adapter support matrix",
        audience="all",
        summary="Per-adapter, per-field capture fidelity (rendered live).",
        renderer=_render_support_matrix,
    ),
    Doc(
        slug="doctor",
        title="Store health check (doctor)",
        audience="all",
        summary="What `doctor` checks and how to read its output.",
        text=_DOCTOR,
    ),
    Doc(
        slug="privacy",
        title="Privacy and local-first design",
        audience="all",
        summary="What stays on your machine and what is opt-in.",
        text=_PRIVACY,
    ),
)

_BY_SLUG: dict[str, Doc] = {d.slug: d for d in _DOCS}


# ── public API ───────────────────────────────────────────────────────────────


def topics() -> list[str]:
    """Return every doc slug, in registry order."""
    return [d.slug for d in _DOCS]


def list_docs(audience: str | None = None) -> list[dict[str, str]]:
    """Return doc metadata (no bodies), optionally filtered by *audience*.

    ``audience="agent"`` returns agent-tagged pages plus the ``all`` pages
    (which are for everyone); ``None`` returns every page.
    """
    if audience is not None and audience not in AUDIENCES:
        raise ValueError(
            f"unknown audience {audience!r}; choose one of {', '.join(AUDIENCES)}"
        )
    out: list[dict[str, str]] = []
    for d in _DOCS:
        if audience is None or audience == "all" or d.audience in (audience, "all"):
            out.append(
                {
                    "slug": d.slug,
                    "title": d.title,
                    "audience": d.audience,
                    "summary": d.summary,
                }
            )
    return out


def get_doc(slug: str) -> dict[str, Any] | None:
    """Return the full page ``{slug,title,audience,summary,body}`` or ``None``."""
    doc = _BY_SLUG.get(slug)
    if doc is None:
        return None
    return {
        "slug": doc.slug,
        "title": doc.title,
        "audience": doc.audience,
        "summary": doc.summary,
        "body": doc.body(),
    }

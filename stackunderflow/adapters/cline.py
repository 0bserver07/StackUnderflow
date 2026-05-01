"""Cline-family (VS Code globalStorage) session adapters.

Reads tasks Cline-compatible VS Code extensions write under
``~/Library/Application Support/Code/User/globalStorage/{extensionId}/tasks/``.
Each task is a directory ``{taskId}/`` containing two JSON files:

- ``ui_messages.json`` — flat array of UI events. We treat ``type=="say"``
  events with ``say=="api_req_started"`` as one assistant turn each. The
  ``text`` field on those events is JSON-stringified and carries
  ``{ tokensIn, tokensOut, cacheWrites, cacheReads, cost }``.
- ``api_conversation_history.json`` — flat array of ``{role, content}``
  Anthropic-shape messages. The first user message is expected to embed
  ``<model>...</model>`` declaring the model used for the run.

Storage / resumption note (spec §3.2):
This adapter is a **hybrid** — it reads files (so ``source_kind="file"``),
but ``seq`` is the *event index* within ``ui_messages.json`` (0, 1, 2, …),
not a byte offset. ``read(ref, since_offset=N)`` therefore means "skip
events at-or-before index N", not "seek to byte N". The
``test_read_since_offset_is_storage_aware`` contract test still holds —
it only checks monotonic ``seq`` and that resume yields strictly fewer
records past a midpoint.

The on-disk layout is identical for Cline, KiloCode and Roo Code — only
the extension-id directory differs. All three adapters subclass
:class:`_VsCodeClineAdapter` and override only the class-level identifiers.
"""

from __future__ import annotations

import json
import logging
import re
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

# Root of VS Code's globalStorage on macOS — extension directories live below.
_VSCODE_GLOBAL_STORAGE_MACOS = (
    Path.home() / "Library" / "Application Support" / "Code" / "User"
    / "globalStorage"
)
# Windows / Linux roots kept here for documentation only — untested on this
# machine and not exercised by enumerate() in v1. See spec §5.
# _VSCODE_GLOBAL_STORAGE_WINDOWS = Path(...)  # untested
# _VSCODE_GLOBAL_STORAGE_LINUX = Path(...)    # untested

# Anthropic-shape default when no <model> tag is present.
_DEFAULT_MODEL = "cline-auto"

# Inline <model>...</model> declaration in the first user message.
_MODEL_TAG_RE = re.compile(r"<model>([^<]+)</model>", re.IGNORECASE)


def _default_tasks_root(extension_id: str) -> Path:
    """Return the macOS tasks root for the given VS Code extension id."""
    return _VSCODE_GLOBAL_STORAGE_MACOS / extension_id / "tasks"


class _VsCodeClineAdapter:
    """Shared parser for Cline-compatible VS Code extensions.

    Subclasses override :attr:`name`, :attr:`_extension_id` and
    :attr:`_project_slug` to point at their own globalStorage directory.
    The on-disk format (``tasks/{taskId}/{ui_messages.json,
    api_conversation_history.json}``) is identical across the family.
    """

    # Subclasses MUST override these three class attributes.
    name: str = ""
    _extension_id: str = ""
    _project_slug: str = ""

    def __init__(self, tasks_root: Path | None = None) -> None:
        # tasks_root is overridable so tests can point at synthetic fixtures
        # without monkey-patching Path.home().
        self._root = tasks_root or _default_tasks_root(self._extension_id)

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            # Not installed / never used — clean no-op rather than raise.
            return

        for task_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            ui_messages = task_dir / "ui_messages.json"
            if not ui_messages.is_file():
                continue
            try:
                stat = ui_messages.stat()
            except OSError as exc:
                _log.warning("Cannot stat Cline task %s: %s", ui_messages, exc)
                continue

            yield SessionRef(
                provider=self.name,
                # Cline-family adapters don't carry a per-project context
                # the way Claude does — every task lands under the same
                # logical project (the provider name).
                project_slug=self._project_slug,
                session_id=task_dir.name,
                file_path=ui_messages,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
                source_kind="file",
                source_hint=None,
            )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        ui_path = ref.file_path
        history_path = ui_path.parent / "api_conversation_history.json"

        ui_events = _load_json_array(ui_path)
        if ui_events is None:
            return

        history = _load_json_array(history_path) or []

        # Model is declared once on the first user message in api_conversation_history.
        model = _extract_model_tag(history) or _DEFAULT_MODEL

        # Keep an alignment cursor over assistant messages in the history file.
        # Each api_req_started event corresponds to the next assistant turn.
        # We use the user message *preceding* api_req_started in ui_messages
        # for content_text — that's what the user typed/saw, which is more
        # reliable than guessing assistant text from history (history may be
        # partial when the task was interrupted; ui_messages.json is the user-
        # facing source of truth and is always present).
        last_user_text = ""

        for idx, event in enumerate(ui_events):
            # since_offset semantics for Cline: event index, not byte offset.
            # Caller already saw the record at exactly ``since_offset`` so we
            # yield strictly past it (matches Codex's byte-floor semantics).
            if since_offset > 0 and idx <= since_offset:
                # Still update last_user_text so post-resume turns can attach
                # the right content; otherwise the user text we saw before
                # the resume point would be silently lost.
                if isinstance(event, dict) and event.get("type") == "say":
                    say = event.get("say")
                    if say in ("user_feedback", "text"):
                        text = event.get("text")
                        if isinstance(text, str):
                            last_user_text = text
                continue

            if not isinstance(event, dict):
                continue
            if event.get("type") != "say":
                continue

            say = event.get("say")
            if say in ("user_feedback", "text"):
                text = event.get("text")
                if isinstance(text, str):
                    last_user_text = text
                continue

            if say != "api_req_started":
                continue

            tokens = _parse_api_req_text(event.get("text"))
            timestamp = _ts_to_iso(event.get("ts"))

            yield Record(
                provider=self.name,
                session_id=ref.session_id,
                # seq is the event index in ui_messages.json. Using the index
                # means resumption is "skip first N events", not "skip first
                # N bytes" — see module docstring.
                seq=idx,
                timestamp=timestamp,
                role="assistant",
                model=model,
                input_tokens=tokens["tokensIn"],
                output_tokens=tokens["tokensOut"],
                cache_create_tokens=tokens["cacheWrites"],
                cache_read_tokens=tokens["cacheReads"],
                content_text=last_user_text,
                tools=(),
                cwd=None,
                is_sidechain=False,
                uuid=f"{ref.session_id}:{idx}",
                parent_uuid=None,
                raw=event,
            )


# ── helpers ───────────────────────────────────────────────────────────


def _load_json_array(path: Path) -> list | None:
    """Return the JSON array at ``path`` or None on any failure.

    Cline writes both ui_messages.json and api_conversation_history.json as
    top-level JSON arrays. Anything else (object, malformed) is treated as
    "no usable data" rather than raising — keeps a single broken task from
    poisoning a batch enumerate.
    """
    try:
        with path.open("rb") as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        _log.warning("Cannot read Cline JSON %s: %s", path, exc)
        return None
    if not isinstance(data, list):
        _log.warning("Cline JSON %s is not a list (got %s)", path, type(data).__name__)
        return None
    return data


def _extract_model_tag(history: list) -> str | None:
    """Return the model declared in ``<model>...</model>`` in the first
    user message, or None.

    Cline's first user message embeds the model the run will use as an
    XML-style tag. We scan only the first user message — model changes
    mid-task aren't supported in v1.
    """
    for entry in history:
        if not isinstance(entry, dict):
            continue
        if entry.get("role") != "user":
            continue
        content = entry.get("content")
        text = _content_to_text(content)
        if not text:
            continue
        match = _MODEL_TAG_RE.search(text)
        if match:
            return match.group(1).strip() or None
        # Stop after the first user message — only the opening message
        # carries the model declaration.
        return None
    return None


def _content_to_text(content: object) -> str:
    """Flatten Anthropic-shape content (string or list of blocks) into one string."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        pieces: list[str] = []
        for blk in content:
            if isinstance(blk, dict):
                t = blk.get("text")
                if isinstance(t, str):
                    pieces.append(t)
            elif isinstance(blk, str):
                pieces.append(blk)
        return "\n".join(pieces)
    return ""


def _parse_api_req_text(text: object) -> dict[str, int]:
    """Parse the JSON-stringified ``text`` field on an ``api_req_started`` event.

    Shape:
        {"tokensIn": int, "tokensOut": int, "cacheWrites": int,
         "cacheReads": int, "cost": float}

    Missing or malformed values default to 0 — pricing happens later in
    the cost layer, so a partial event still produces a valid Record.
    """
    out = {"tokensIn": 0, "tokensOut": 0, "cacheWrites": 0, "cacheReads": 0}
    if not isinstance(text, str) or not text:
        return out
    try:
        parsed = json.loads(text)
    except (json.JSONDecodeError, ValueError):
        return out
    if not isinstance(parsed, dict):
        return out
    for key in out:
        val = parsed.get(key, 0)
        try:
            out[key] = max(int(val or 0), 0)
        except (TypeError, ValueError):
            out[key] = 0
    return out


def _ts_to_iso(ts: object) -> str:
    """Convert Cline's ``ts`` (epoch milliseconds) to ISO 8601 UTC.

    Empty / missing / unparseable timestamps fall back to "" — the
    contract test only requires the field to parse-as-iso when present
    and non-empty (records still emit one even when ts is absent).
    """
    if ts is None or ts == "":
        return ""
    try:
        millis = float(ts)
    except (TypeError, ValueError):
        return ""
    if millis <= 0:
        return ""
    return datetime.fromtimestamp(millis / 1000.0, tz=UTC).isoformat()


# ── concrete Cline-family adapters ────────────────────────────────────


class ClineAdapter(_VsCodeClineAdapter):
    """Source adapter for the Cline VS Code extension.

    Extension id: ``saoudrizwan.claude-dev``.
    """

    name = "cline"
    _extension_id = "saoudrizwan.claude-dev"
    _project_slug = "cline"


class KiloCodeAdapter(_VsCodeClineAdapter):
    """Source adapter for the KiloCode VS Code extension.

    Extension id: ``kilocode.kilo-code``. KiloCode wraps the same Cline
    parser surface — only the globalStorage directory differs.
    """

    name = "kilocode"
    _extension_id = "kilocode.kilo-code"
    _project_slug = "kilocode"


class RooCodeAdapter(_VsCodeClineAdapter):
    """Source adapter for the Roo Code VS Code extension.

    Extension id: ``rooveterinaryinc.roo-cline``. Roo Code wraps the same
    Cline parser surface — only the globalStorage directory differs.
    """

    name = "roocode"
    _extension_id = "rooveterinaryinc.roo-cline"
    _project_slug = "roocode"

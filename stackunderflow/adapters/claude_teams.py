"""Claude Code agent-teams discovery — read ``~/.claude/teams/`` + ``~/.claude/tasks/``.

Claude Code's agent-teams feature spawns parallel sub-agents that each
write their own JSONL transcript under ``~/.claude/projects/...``. The
team's metadata — member roster, spawn prompts, lead session id, raw
config — lives in ``~/.claude/teams/{team-name}/config.json``, and the
per-team task assignments live in ``~/.claude/tasks/{team-name}/{N}.json``.

This module is deliberately split out from ``claude.py`` so the pure
parse/link functions are unit-testable against a synthetic ``~/.claude/``
fixture without dragging in the message-row construction path.

Public API
----------

* :func:`discover_teams` — scan ``~/.claude/teams/`` → ``list[TeamRecord]``.
* :func:`discover_tasks` — scan ``~/.claude/tasks/{team}/`` → ``list[TaskRecord]``.
* :func:`link_sessions_to_team` — map ``session_id`` → :class:`SessionTeamLink`
  using (1) the team config's lead session, (2) ``teamName`` /
  ``agentId`` carried in the session's own JSONL, (3) a ``parent_uuid``
  chain fallback for older transcripts that pre-date ``teamName``.
* :func:`materialize_team_metadata` — orchestrator. Scans the filesystem,
  matches teams to ingested projects, writes the ``sessions`` team
  columns + upserts ``agent_teams`` rows. Idempotent; called from the
  ingest pass (see :meth:`ClaudeAdapter.materialize_metadata`).

Privacy note: ``~/.claude/tasks/`` task descriptions and team member
``prompt`` fields can carry user prompts with sensitive content. Same
posture as ``messages``: local-only, never surfaced via telemetry.
"""

from __future__ import annotations

import json
import logging
import re
import sqlite3
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

_log = logging.getLogger(__name__)

__all__ = [
    "MemberRecord",
    "TeamRecord",
    "TaskRecord",
    "SessionTeamHint",
    "SessionTeamLink",
    "MaterializeReport",
    "discover_teams",
    "discover_tasks",
    "link_sessions_to_team",
    "materialize_team_metadata",
]

ROLE_LEAD = "lead"
ROLE_SUBAGENT = "subagent"

# Files inside a ``~/.claude/tasks/{team}/`` dir that are not task JSON.
_TASK_SKIP_FILES = {".lock", ".highwatermark"}


# ── dataclasses ──────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class MemberRecord:
    """One ``members[]`` entry from a team's ``config.json``."""

    agent_id: str
    name: str
    agent_type: str | None
    model: str | None
    cwd: str | None
    is_lead: bool
    prompt: str | None  # the full spawn prompt (sub-agents only)


@dataclass(frozen=True, slots=True)
class TeamRecord:
    """One Claude Code team, parsed from ``~/.claude/teams/{name}/config.json``."""

    team_id: str  # the team name == the directory name
    created_ts: str  # ISO 8601 (converted from the config's epoch-ms ``createdAt``)
    description: str | None
    lead_session_id: str | None
    lead_agent_id: str | None
    project_path: str | None  # the lead member's cwd (best-effort)
    members: tuple[MemberRecord, ...]
    config_json: str  # the verbatim config.json text


@dataclass(frozen=True, slots=True)
class TaskRecord:
    """One task assignment from ``~/.claude/tasks/{team}/{N}.json``."""

    task_id: str
    owner_name: str | None  # matches a member's ``name``
    subject: str | None
    description: str | None
    status: str | None


@dataclass(frozen=True, slots=True)
class SessionTeamHint:
    """What :func:`link_sessions_to_team` needs to know about one session.

    Built from the session's own JSONL records (``teamName`` / ``agentId``
    live in the raw record, ``uuid`` / ``parent_uuid`` chain back to the
    spawning message). ``uuids`` / ``parent_uuids`` are only needed for
    the chain fallback — callers may pass empty frozensets when they
    don't carry that data.
    """

    session_id: str
    team_name: str | None = None
    agent_id: str | None = None
    has_sidechain: bool = False
    uuids: frozenset[str] = field(default_factory=frozenset)
    parent_uuids: frozenset[str] = field(default_factory=frozenset)


@dataclass(frozen=True, slots=True)
class SessionTeamLink:
    """Resolved team affiliation for one session — what gets written to ``sessions``."""

    team_id: str
    role: str  # ROLE_LEAD | ROLE_SUBAGENT
    spawn_prompt: str | None
    parent_session_id: str | None


@dataclass(frozen=True, slots=True)
class MaterializeReport:
    """Summary of a :func:`materialize_team_metadata` run (for logging/tests)."""

    teams_seen: int = 0
    teams_materialized: int = 0
    sessions_linked: int = 0


# ── private helpers ──────────────────────────────────────────────────────────


def _safe_json_load_text(text: str | None) -> object:
    if not text:
        return None
    try:
        return json.loads(text)
    except (json.JSONDecodeError, TypeError, ValueError):
        return None


def _safe_json_load_file(path: Path) -> object:
    try:
        text = path.read_text()
    except OSError:
        return None
    return _safe_json_load_text(text)


def _epoch_ms_to_iso(value: object) -> str:
    """Best-effort ISO 8601 from an epoch-ms int. Empty string on garbage."""
    try:
        ms = int(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return ""
    if ms <= 0:
        return ""
    try:
        return datetime.fromtimestamp(ms / 1000, tz=UTC).isoformat()
    except (OverflowError, OSError, ValueError):
        return ""


def slug_for_path(path: str) -> str:
    """Claude Code's project-directory slug for an absolute *path*.

    Claude Code names ``~/.claude/projects/`` subdirs by replacing every
    non-alphanumeric run-character with ``-`` — e.g.
    ``/Users/me/dev_dev/x/.worktrees/y`` → ``-Users-me-dev-dev-x--worktrees-y``.
    Mirrors that so we can map a team member's ``cwd`` back to an ingested
    project slug.
    """
    return re.sub(r"[^A-Za-z0-9]", "-", path or "")


def _strip_team_suffix(agent_id: str | None) -> str | None:
    """``"worker-1@my-team"`` → ``"worker-1"``; pass non-suffixed ids through."""
    if not agent_id:
        return None
    return agent_id.split("@", 1)[0]


# ── discover_teams ───────────────────────────────────────────────────────────


def discover_teams(claude_root: Path) -> list[TeamRecord]:
    """Scan ``{claude_root}/teams/`` for ``config.json`` files.

    Returns one :class:`TeamRecord` per team directory that has a
    parseable ``config.json``. Team directories with only an ``inboxes/``
    subdir (e.g. the implicit ``default`` team) carry no useful metadata
    and are skipped. Sorted by ``team_id`` for deterministic output.
    Never raises on a malformed config — that team is just skipped.
    """
    teams_dir = claude_root / "teams"
    if not teams_dir.is_dir():
        return []

    out: list[TeamRecord] = []
    try:
        entries = sorted(teams_dir.iterdir(), key=lambda p: p.name)
    except OSError:
        return []

    for team_dir in entries:
        if not team_dir.is_dir():
            continue
        config_path = team_dir / "config.json"
        if not config_path.is_file():
            continue
        try:
            config_text = config_path.read_text()
        except OSError:
            continue
        config = _safe_json_load_text(config_text)
        if not isinstance(config, dict):
            continue

        team_id = team_dir.name
        lead_agent_id = config.get("leadAgentId")
        lead_session_id = config.get("leadSessionId")
        description = config.get("description")
        created_ts = _epoch_ms_to_iso(config.get("createdAt"))

        members: list[MemberRecord] = []
        raw_members = config.get("members")
        lead_cwd: str | None = None
        first_cwd: str | None = None
        if isinstance(raw_members, list):
            for raw_m in raw_members:
                if not isinstance(raw_m, dict):
                    continue
                m_agent_id = raw_m.get("agentId")
                if not isinstance(m_agent_id, str) or not m_agent_id:
                    continue
                m_name = raw_m.get("name") or _strip_team_suffix(m_agent_id) or m_agent_id
                m_cwd = raw_m.get("cwd") if isinstance(raw_m.get("cwd"), str) else None
                is_lead = bool(
                    (lead_agent_id and m_agent_id == lead_agent_id)
                    or raw_m.get("agentType") in ("team-lead", "orchestrator")
                    or m_name == "team-lead"
                )
                if first_cwd is None and m_cwd:
                    first_cwd = m_cwd
                if is_lead and m_cwd and lead_cwd is None:
                    lead_cwd = m_cwd
                members.append(
                    MemberRecord(
                        agent_id=m_agent_id,
                        name=str(m_name),
                        agent_type=raw_m.get("agentType") if isinstance(raw_m.get("agentType"), str) else None,
                        model=raw_m.get("model") if isinstance(raw_m.get("model"), str) else None,
                        cwd=m_cwd,
                        is_lead=is_lead,
                        prompt=raw_m.get("prompt") if isinstance(raw_m.get("prompt"), str) else None,
                    )
                )

        out.append(
            TeamRecord(
                team_id=team_id,
                created_ts=created_ts,
                description=description if isinstance(description, str) else None,
                lead_session_id=lead_session_id if isinstance(lead_session_id, str) else None,
                lead_agent_id=lead_agent_id if isinstance(lead_agent_id, str) else None,
                project_path=lead_cwd or first_cwd,
                members=tuple(members),
                config_json=config_text,
            )
        )
    return out


BUILDER_RE = re.compile(
    r'You are `([^`]+)`\s*(?:,?\s*(?:teammate\s+)?(?:on|in\s+team)\s*)`([^`]+)`'
)


def discover_teams_from_jsonl(
    claude_root: Path,
) -> tuple[list[TeamRecord], dict[str, tuple[str, str]]]:
    """Reconstruct TeamRecords by parsing tool-use blocks in session JSONLs.

    Walks {claude_root}/projects/*/*.jsonl. For each file:
      - Find every assistant record carrying a TeamCreate tool_use block.
        Register {team_name, description, lead_session_id=this session, created_ts=record timestamp}.
      - Find every Agent tool_use with that team_name. Register a member
        {name=input.name, agent_type=input.subagent_type, prompt=input.prompt}.
      - Optional but useful: count SendMessage(to=member_name) per member.

    Walk worker JSONLs in the same projects: extract (session_id, teammate_name, team_name)
    triples from each worker's first user message using BUILDER_RE.

    Returns:
      - list of synthetic TeamRecords (one per unique team_name seen in a TeamCreate)
      - dict[worker_session_id, (teammate_name, team_name)] — used by the linker
        to map worker JSONLs to teams.
    """
    projects_dir = claude_root / "projects"
    if not projects_dir.is_dir():
        return [], {}

    teams_data: dict[str, dict[str, Any]] = {}
    worker_map: dict[str, tuple[str, str]] = {}

    try:
        paths = list(projects_dir.glob("*/*.jsonl"))
    except OSError:
        return [], {}

    for path in paths:
        if not path.is_file():
            continue
        session_id = path.stem

        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue

        first_user_text = None
        for line in text.splitlines():
            if not line.strip():
                continue
            # Fast pre-filter to avoid expensive json.loads on every line
            if first_user_text is not None:
                if '"TeamCreate"' not in line and '"Agent"' not in line:
                    continue
            else:
                if '"user"' not in line and '"TeamCreate"' not in line and '"Agent"' not in line:
                    continue

            try:
                rec = json.loads(line)
            except Exception:
                continue

            t = rec.get("type")
            if t == "user" and first_user_text is None:
                msg = rec.get("message") or {}
                content = msg.get("content")
                if isinstance(content, str):
                    if content.strip():
                        first_user_text = content
                elif isinstance(content, list):
                    text_parts = []
                    for blk in content:
                        if isinstance(blk, dict) and blk.get("type") == "text":
                            text_parts.append(blk.get("text", ""))
                    concatenated = "\n".join(text_parts)
                    if concatenated.strip():
                        first_user_text = concatenated

            if t == "assistant":
                msg = rec.get("message") or {}
                content = msg.get("content") or []
                if isinstance(content, list):
                    for blk in content:
                        if isinstance(blk, dict) and blk.get("type") == "tool_use":
                            name = blk.get("name")
                            inp = blk.get("input") or {}
                            if name == "TeamCreate":
                                team_name = inp.get("team_name")
                                if isinstance(team_name, str) and team_name:
                                    desc = inp.get("description")
                                    created_ts = rec.get("timestamp") or ""
                                    teams_data.setdefault(
                                        team_name,
                                        {
                                            "lead_session_id": session_id,
                                            "description": desc if isinstance(desc, str) else None,
                                            "created_ts": created_ts,
                                            "members": {},
                                        },
                                    )
                            elif name == "Agent":
                                team_name = inp.get("team_name")
                                member_name = inp.get("name")
                                subagent_type = inp.get("subagent_type")
                                if subagent_type == "Explore":
                                    continue
                                if (
                                    isinstance(team_name, str)
                                    and team_name
                                    and isinstance(member_name, str)
                                    and member_name
                                ):
                                    prompt = inp.get("prompt")
                                    teams_data.setdefault(
                                        team_name,
                                        {
                                            "lead_session_id": None,
                                            "description": None,
                                            "created_ts": rec.get("timestamp") or "",
                                            "members": {},
                                        },
                                    )
                                    teams_data[team_name]["members"][member_name] = MemberRecord(
                                        agent_id=member_name,
                                        name=member_name,
                                        agent_type=subagent_type if isinstance(subagent_type, str) else None,
                                        model=None,
                                        cwd=None,
                                        is_lead=False,
                                        prompt=prompt if isinstance(prompt, str) else None,
                                    )

        if first_user_text:
            m = BUILDER_RE.search(first_user_text)
            if m:
                teammate_name = m.group(1)
                team_name = m.group(2)
                worker_map[session_id] = (teammate_name, team_name)

    synthetic_teams: list[TeamRecord] = []
    for team_name, data in sorted(teams_data.items()):
        if not data["lead_session_id"]:
            continue

        lead_member = MemberRecord(
            agent_id="team-lead",
            name="team-lead",
            agent_type="orchestrator",
            model=None,
            cwd=None,
            is_lead=True,
            prompt=None,
        )
        members_list = [lead_member]
        for m_name, m_record in sorted(data["members"].items()):
            members_list.append(m_record)

        created_epoch = 0
        if data["created_ts"]:
            try:
                ts_str = data["created_ts"].replace("Z", "+00:00")
                created_epoch = int(datetime.fromisoformat(ts_str).timestamp() * 1000)
            except Exception:
                pass

        config_dict = {
            "_source": "jsonl_fallback",
            "leadSessionId": data["lead_session_id"],
            "description": data["description"],
            "createdAt": created_epoch,
            "members": [
                {
                    "agentId": m.agent_id,
                    "name": m.name,
                    "agentType": m.agent_type,
                    "model": m.model,
                    "cwd": m.cwd,
                    "isLead": m.is_lead,
                    "prompt": m.prompt,
                }
                for m in members_list
            ],
        }

        synthetic_teams.append(
            TeamRecord(
                team_id=team_name,
                created_ts=data["created_ts"],
                description=data["description"],
                lead_session_id=data["lead_session_id"],
                lead_agent_id="team-lead",
                project_path=None,
                members=tuple(members_list),
                config_json=json.dumps(config_dict),
            )
        )

    return synthetic_teams, worker_map


# ── discover_tasks ───────────────────────────────────────────────────────────


def discover_tasks(claude_root: Path, team_id: str) -> list[TaskRecord]:
    """Scan ``{claude_root}/tasks/{team_id}/`` for task-assignment JSON files.

    Each ``{N}.json`` carries ``{id, subject, description, status, owner?}``
    where ``owner`` matches a team member's ``name``. Returns
    :class:`TaskRecord` list sorted by numeric task id. A missing tasks
    directory → empty list. Malformed / non-dict files are skipped.
    """
    tasks_dir = claude_root / "tasks" / team_id
    if not tasks_dir.is_dir():
        return []

    out: list[TaskRecord] = []
    try:
        entries = list(tasks_dir.iterdir())
    except OSError:
        return []

    for task_path in entries:
        if not task_path.is_file():
            continue
        if task_path.name in _TASK_SKIP_FILES or task_path.name.startswith("."):
            continue
        if task_path.suffix != ".json":
            continue
        obj = _safe_json_load_file(task_path)
        if not isinstance(obj, dict):
            continue
        task_id = obj.get("id")
        if task_id is None:
            task_id = task_path.stem
        owner = obj.get("owner")
        out.append(
            TaskRecord(
                task_id=str(task_id),
                owner_name=str(owner) if isinstance(owner, str) and owner else None,
                subject=obj.get("subject") if isinstance(obj.get("subject"), str) else None,
                description=obj.get("description") if isinstance(obj.get("description"), str) else None,
                status=obj.get("status") if isinstance(obj.get("status"), str) else None,
            )
        )

    def _sort_key(t: TaskRecord) -> tuple[int, str]:
        try:
            return (int(t.task_id), "")
        except (TypeError, ValueError):
            return (1 << 30, t.task_id)

    out.sort(key=_sort_key)
    return out


# ── link_sessions_to_team ────────────────────────────────────────────────────


def _spawn_prompt_for(
    hint: SessionTeamHint,
    team: TeamRecord,
    tasks: list[TaskRecord],
) -> str | None:
    """Pick the richest spawn prompt for a sub-agent session.

    Preference order: the team member's ``prompt`` field (the verbatim
    spawn prompt) → the owning task's ``description`` → ``None``.
    """
    aid = hint.agent_id
    aid_bare = _strip_team_suffix(aid)
    member: MemberRecord | None = None
    if aid:
        for m in team.members:
            if m.agent_id == aid or m.name == aid or (aid_bare and (m.name == aid_bare or m.agent_id == aid_bare)):
                member = m
                break
    if member is not None and member.prompt:
        return member.prompt

    # Fall back to a task owned by this member's name.
    member_name = member.name if member is not None else aid_bare
    if member_name:
        for t in tasks:
            if t.owner_name == member_name and t.description:
                return t.description
    # Single-task teams sometimes omit ``owner`` — use the lone task.
    if len(tasks) == 1 and tasks[0].description and not any(t.owner_name for t in tasks):
        return tasks[0].description
    return None


def link_sessions_to_team(
    session_hints: list[SessionTeamHint],
    teams: list[TeamRecord],
    tasks_by_team: dict[str, list[TaskRecord]] | None = None,
    worker_map: dict[str, tuple[str, str]] | None = None,
) -> dict[str, SessionTeamLink]:
    """Map ``session_id`` → :class:`SessionTeamLink` for every linkable session.

    Resolution order:

    1. **Lead** — ``team.lead_session_id`` from ``config.json`` → ``role=lead``.
    2. **teamName** — a session whose JSONL carries ``teamName == team.team_id``
       → ``role=subagent``, parent = the team's lead session. ``spawn_prompt``
       comes from the matching member's ``prompt`` (or owning task's
       ``description``); ``None`` when the config doesn't list a matching
       ``agentId``.
    2.5 **worker_map** — fallback for deleted team configs. Links worker JSONLs
        using first-user prompt matched classmate metadata.
    3. **parent_uuid chain** — for sidechain sessions that carry no
       ``teamName`` (older transcripts), if the session's first message's
       ``parent_uuid`` resolves to a ``uuid`` inside an already-linked
       session, inherit that session's team + treat it as a sub-agent of
       it. Iterated to a fixpoint so grand-children resolve too.

    Sessions that match none of the above are absent from the result
    (they keep NULL team metadata).
    """
    tasks_by_team = tasks_by_team or {}
    team_by_name: dict[str, TeamRecord] = {t.team_id: t for t in teams}
    out: dict[str, SessionTeamLink] = {}

    # 1. leads
    for team in teams:
        if team.lead_session_id:
            out[team.lead_session_id] = SessionTeamLink(
                team_id=team.team_id,
                role=ROLE_LEAD,
                spawn_prompt=None,
                parent_session_id=None,
            )

    # 2. teamName matches
    for hint in session_hints:
        if not hint.team_name:
            continue
        team = team_by_name.get(hint.team_name)
        if team is None:
            continue
        if hint.session_id == team.lead_session_id:
            continue  # already a lead
        # Don't downgrade a session we already pinned as a lead of some team.
        if out.get(hint.session_id) and out[hint.session_id].role == ROLE_LEAD:
            continue
        out[hint.session_id] = SessionTeamLink(
            team_id=team.team_id,
            role=ROLE_SUBAGENT,
            spawn_prompt=_spawn_prompt_for(hint, team, tasks_by_team.get(team.team_id, [])),
            parent_session_id=team.lead_session_id,
        )

    # 2.5 worker_map fallback matches
    if worker_map:
        for hint in session_hints:
            if hint.session_id in out:
                continue
            if hint.session_id not in worker_map:
                continue
            teammate_name, team_name = worker_map[hint.session_id]
            team = team_by_name.get(team_name)
            if team is None:
                continue
            if hint.session_id == team.lead_session_id:
                continue  # already lead
            if out.get(hint.session_id) and out[hint.session_id].role == ROLE_LEAD:
                continue

            spawn_prompt = None
            for m in team.members:
                if m.name == teammate_name or m.agent_id == teammate_name:
                    spawn_prompt = m.prompt
                    break

            out[hint.session_id] = SessionTeamLink(
                team_id=team.team_id,
                role=ROLE_SUBAGENT,
                spawn_prompt=spawn_prompt,
                parent_session_id=team.lead_session_id,
            )

    # 3. parent_uuid chain fallback (older transcripts without teamName)
    hint_by_id = {h.session_id: h for h in session_hints}
    # uuid → owning session_id, seeded from sessions already linked.
    uuid_owner: dict[str, str] = {}
    for sid in list(out.keys()):
        h = hint_by_id.get(sid)
        if h is None:
            continue
        for u in h.uuids:
            uuid_owner.setdefault(u, sid)

    changed = True
    while changed:
        changed = False
        for hint in session_hints:
            if hint.session_id in out:
                continue
            if not hint.has_sidechain:
                continue
            owner_sid: str | None = None
            for pu in hint.parent_uuids:
                if pu in uuid_owner:
                    owner_sid = uuid_owner[pu]
                    break
            if owner_sid is None:
                continue
            owner_link = out.get(owner_sid)
            if owner_link is None:
                continue
            out[hint.session_id] = SessionTeamLink(
                team_id=owner_link.team_id,
                role=ROLE_SUBAGENT,
                spawn_prompt=None,
                parent_session_id=owner_sid,
            )
            for u in hint.uuids:
                uuid_owner.setdefault(u, hint.session_id)
            changed = True

    return out


# ── materialize_team_metadata (ingest-time orchestrator) ─────────────────────


def _project_id_for_session(conn: sqlite3.Connection, session_id: str) -> int | None:
    row = conn.execute(
        "SELECT project_id FROM sessions WHERE session_id = ? LIMIT 1",
        (session_id,),
    ).fetchone()
    return int(row["project_id"]) if row else None


def _project_id_for_slug(conn: sqlite3.Connection, provider: str, slug: str) -> int | None:
    row = conn.execute(
        "SELECT id FROM projects WHERE provider = ? AND slug = ? LIMIT 1",
        (provider, slug),
    ).fetchone()
    return int(row["id"]) if row else None


def _build_hints_for_projects(
    conn: sqlite3.Connection,
    project_ids: set[int],
    team: TeamRecord,
) -> list[SessionTeamHint]:
    """Build :class:`SessionTeamHint`s for every session in *project_ids*.

    Cheap path: peek each session's first message for ``teamName`` /
    ``agentId`` / ``parent_uuid``. Only the handful of sessions that look
    team-related (the lead, anything carrying this team's ``teamName``,
    or anything with sidechain rows) gets the heavier "fetch every uuid"
    pass that powers the chain fallback.
    """
    if not project_ids:
        return []
    placeholders = ",".join("?" * len(project_ids))
    sess_rows = conn.execute(
        f"SELECT id, session_id FROM sessions WHERE project_id IN ({placeholders})",  # noqa: S608 — placeholders are '?'
        tuple(project_ids),
    ).fetchall()

    hints: list[SessionTeamHint] = []
    for sr in sess_rows:
        sfk = int(sr["id"])
        sid = sr["session_id"]
        first = conn.execute(
            "SELECT raw_json, parent_uuid FROM messages WHERE session_fk = ? ORDER BY seq LIMIT 1",
            (sfk,),
        ).fetchone()
        raw = _safe_json_load_text(first["raw_json"]) if first else None
        raw = raw if isinstance(raw, dict) else {}
        team_name = raw.get("teamName") if isinstance(raw.get("teamName"), str) else None
        agent_id = raw.get("agentId") if isinstance(raw.get("agentId"), str) else None
        has_sc = bool(
            conn.execute(
                "SELECT 1 FROM messages WHERE session_fk = ? AND is_sidechain = 1 LIMIT 1",
                (sfk,),
            ).fetchone()
        )
        uuids: frozenset[str] = frozenset()
        parent_uuids: frozenset[str] = frozenset()
        if sid == team.lead_session_id or team_name == team.team_id or has_sc:
            ur = conn.execute(
                "SELECT uuid, parent_uuid FROM messages WHERE session_fk = ?",
                (sfk,),
            ).fetchall()
            uuids = frozenset(r["uuid"] for r in ur if r["uuid"])
            parent_uuids = frozenset(r["parent_uuid"] for r in ur if r["parent_uuid"])
        hints.append(
            SessionTeamHint(
                session_id=sid,
                team_name=team_name,
                agent_id=agent_id,
                has_sidechain=has_sc,
                uuids=uuids,
                parent_uuids=parent_uuids,
            )
        )
    return hints


def materialize_team_metadata(
    conn: sqlite3.Connection,
    *,
    claude_root: Path | None = None,
    provider: str = "claude",
) -> MaterializeReport:
    """Scan ``~/.claude/teams/`` + ``~/.claude/tasks/`` and write the indexed
    team metadata: ``agent_teams`` rows + ``sessions.{team_id,
    spawned_by_session_id, spawn_prompt, agent_role}`` columns.

    Idempotent — re-running over the same filesystem state produces the
    same rows (``agent_teams`` upserts on ``team_id``; the ``sessions``
    UPDATE is a straight overwrite). Safe to call every ingest pass; a
    missing / empty ``~/.claude/teams/`` is a no-op. Never raises on
    filesystem or DB hiccups — logs and returns what it managed.

    Teams whose lead transcript hasn't been ingested yet (and whose
    member ``cwd``s don't map to a known project) are skipped this pass;
    they get picked up once those sessions land.
    """
    orig_row_factory = conn.row_factory
    conn.row_factory = sqlite3.Row
    claude_root = claude_root or (Path.home() / ".claude")
    report_teams_seen = 0
    report_materialized = 0
    report_linked = 0

    try:
        config_teams = discover_teams(claude_root)
    except OSError as exc:  # pragma: no cover - defensive
        _log.debug("claude_teams: discover_teams failed under %s: %s", claude_root, exc)
        config_teams = []

    try:
        fallback_teams, worker_map = discover_teams_from_jsonl(claude_root)
    except Exception as exc:
        _log.debug("claude_teams: discover_teams_from_jsonl failed under %s: %s", claude_root, exc)
        fallback_teams, worker_map = [], {}

    # Merge: config wins on conflict
    teams_dict: dict[str, TeamRecord] = {t.team_id: t for t in fallback_teams}
    for t in config_teams:
        teams_dict[t.team_id] = t
    teams = list(teams_dict.values())

    if not teams:
        conn.row_factory = orig_row_factory
        return MaterializeReport()

    conn.execute("BEGIN")
    try:
        for team in teams:
            report_teams_seen += 1

            # Locate the candidate projects this team's sessions live in.
            candidate_pids: set[int] = set()
            lead_pid: int | None = None
            if team.lead_session_id:
                lead_pid = _project_id_for_session(conn, team.lead_session_id)
                if lead_pid is not None:
                    candidate_pids.add(lead_pid)
            for m in team.members:
                if not m.cwd:
                    continue
                pid = _project_id_for_slug(conn, provider, slug_for_path(m.cwd))
                if pid is not None:
                    candidate_pids.add(pid)

            if worker_map:
                for w_sid, (m_name, t_name) in worker_map.items():
                    if t_name == team.team_id:
                        w_pid = _project_id_for_session(conn, w_sid)
                        if w_pid is not None:
                            candidate_pids.add(w_pid)

            if not candidate_pids:
                continue  # nothing ingested for this team yet
            team_project_id = lead_pid if lead_pid is not None else min(candidate_pids)

            hints = _build_hints_for_projects(conn, candidate_pids, team)
            tasks = discover_tasks(claude_root, team.team_id)
            links = link_sessions_to_team(hints, [team], {team.team_id: tasks}, worker_map=worker_map)
            if not links:
                continue

            conn.execute(
                "INSERT INTO agent_teams "
                "(team_id, project_id, created_ts, description, lead_session_id, config_json) "
                "VALUES (?, ?, ?, ?, ?, ?) "
                "ON CONFLICT(team_id) DO UPDATE SET "
                "  project_id = excluded.project_id, "
                "  created_ts = excluded.created_ts, "
                "  description = excluded.description, "
                "  lead_session_id = excluded.lead_session_id, "
                "  config_json = excluded.config_json",
                (
                    team.team_id,
                    team_project_id,
                    team.created_ts or "",
                    team.description,
                    team.lead_session_id,
                    team.config_json,
                ),
            )
            report_materialized += 1

            # Claude session ids are UUIDs (effectively globally unique), so a
            # plain ``session_id = ?`` overwrite is enough — no need to scope
            # the UPDATE to ``candidate_pids``.
            for sid, link in links.items():
                cur = conn.execute(
                    "UPDATE sessions SET team_id = ?, spawned_by_session_id = ?, "
                    "spawn_prompt = ?, agent_role = ? WHERE session_id = ?",
                    (link.team_id, link.parent_session_id, link.spawn_prompt, link.role, sid),
                )
                report_linked += cur.rowcount or 0

        conn.execute("COMMIT")
    except sqlite3.Error as exc:
        conn.execute("ROLLBACK")
        _log.warning("claude_teams: materialize_team_metadata rolled back: %s", exc)
        conn.row_factory = orig_row_factory
        return MaterializeReport()

    conn.row_factory = orig_row_factory
    if report_materialized:
        _log.info(
            "claude_teams: materialized %d/%d team(s), linked %d session(s)",
            report_materialized, report_teams_seen, report_linked,
        )
    return MaterializeReport(
        teams_seen=report_teams_seen,
        teams_materialized=report_materialized,
        sessions_linked=report_linked,
    )

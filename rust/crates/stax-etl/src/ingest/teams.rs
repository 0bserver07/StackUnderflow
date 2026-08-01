//! `adapters/claude_teams.py` — Claude Code agent-team discovery (RS-2-004).
//!
//! The 926-line module that fills `sessions.{team_id, spawned_by_session_id,
//! spawn_prompt, agent_role}` and the `agent_teams` table. It is the body of
//! [`super::hooks::ClaudeHook`], and closing it is what closes **DIV-042** —
//! the gap the wave-4 gate counted at *41 of 162 sessions in Python, 0 in the
//! port* on the 1 GB corpus.
//!
//! ```text
//! discover_teams          ~/.claude/teams/{name}/config.json   → TeamRecord
//! discover_teams_from_jsonl  ~/.claude/projects/*/*.jsonl       → TeamRecord + worker map
//! discover_tasks          ~/.claude/tasks/{team}/{N}.json      → TaskRecord
//! link_sessions_to_team   hints × teams × tasks                → session → link
//! materialize_team_metadata  all of the above → agent_teams + sessions UPDATE
//! ```
//!
//! # Why this lives in `stax-etl` and not in `stax-adapters`
//!
//! `rust/TASKS-RS.md` files RS-2-004 against `crate: stax-adapters`, because the
//! inventory assigned crates by Python module path and this file sits under
//! `stackunderflow/adapters/`. The architect's binding ruling overrides that:
//! *"materialize_metadata becomes a PostIngestHook trait owned by
//! stax-etl/stax-core — adapters stay storage-free"* (ARCHITECT-STATE,
//! "ARCHITECT DECISIONS (E's questions, binding)", item 1). The orchestrator
//! here takes a `&Connection`, issues an `INSERT … ON CONFLICT` and an `UPDATE`,
//! and knows the `sessions` / `messages` / `projects` schema; putting it in
//! `stax-adapters` would make that crate storage-aware for the sake of one
//! provider — exactly what the ruling refused. The pure discovery half could
//! have been split across the crate line, but it has exactly one consumer, and
//! a module split across two crates is two places to drift.
//!
//! # The three orderings that had to be decided, and how
//!
//! 1. **`teams/` is sorted** (`sorted(teams_dir.iterdir(), key=…name)`), so this
//!    sorts. Python sorts `str`s by code point; Rust sorts `OsStr` by byte, and
//!    for UTF-8 those agree.
//! 2. **`projects/*/*.jsonl` is NOT sorted.** `Path.glob` is `os.scandir` order
//!    — raw `readdir` — and the traversal is order-*sensitive*: the first
//!    `TeamCreate` for a team name wins its `lead_session_id` (`setdefault`)
//!    while a member's record is last-write-wins. Sorting here would be a
//!    different answer on a corpus where one team name is created twice, not
//!    merely a different order, so [`read_dir_entries`] deliberately does not
//!    sort: `std::fs::read_dir` is the same `getdents` walk `os.scandir` makes.
//! 3. **`frozenset` iteration is replaced by a `BTreeSet`.** Python picks the
//!    chain-fallback owner with `for pu in hint.parent_uuids: if pu in
//!    uuid_owner: … break` over a *set of strings*, whose order depends on
//!    `PYTHONHASHSEED` and therefore changes between two runs of the reference
//!    itself. A sorted set is deterministic; where more than one parent uuid
//!    resolves to a *different* owner the two implementations may disagree, and
//!    so would two Python runs. Recorded as DIV-311.
//!
//! # Two error paths that are load-bearing and look like typos
//!
//! * `discover_teams_from_jsonl` reads each transcript with
//!   `path.read_text(encoding="utf-8")` inside `except OSError: continue`. A
//!   file that is not valid UTF-8 raises `UnicodeDecodeError`, which is **not**
//!   an `OSError`, so it escapes the per-file loop and is caught by
//!   `materialize_team_metadata`'s `except Exception` — abandoning *every*
//!   JSONL-derived team, not just that file. Ported exactly: read bytes (an I/O
//!   error skips the file), then decode strictly (a decode error abandons the
//!   whole discovery).
//! * `_safe_json_load_file` has the same `except OSError` around `read_text()`,
//!   so a non-UTF-8 *task* file escapes `discover_tasks` — and `discover_tasks`
//!   is called **inside** the transaction, where the only handler is `except
//!   sqlite3.Error`. The exception therefore leaves the `BEGIN` open and
//!   propagates to `run_ingest`'s hook fence. Ported bug-for-bug: [`discover_tasks`]
//!   returns `Result` and the error propagates without a `ROLLBACK`, which the
//!   connection's drop performs anyway.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use stax_adapters::pyval;
use stax_core::queries::{pyjson, pytime};

use crate::marts::json as pyloads;

/// `ROLE_LEAD` — `sessions.agent_role` for a team's lead session.
pub const ROLE_LEAD: &str = "lead";
/// `ROLE_SUBAGENT` — `sessions.agent_role` for everything else linked.
pub const ROLE_SUBAGENT: &str = "subagent";

/// Files inside a `~/.claude/tasks/{team}/` dir that are not task JSON.
const TASK_SKIP_FILES: [&str; 2] = [".lock", ".highwatermark"];

// ── dataclasses ──────────────────────────────────────────────────────────────

/// One `members[]` entry from a team's `config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRecord {
    /// `agentId` — required and non-empty, or the member is skipped.
    pub agent_id: String,
    /// `name`, falling back to the `@team`-stripped agent id.
    pub name: String,
    /// `agentType`, when it is a string.
    pub agent_type: Option<String>,
    /// `model`, when it is a string.
    pub model: Option<String>,
    /// `cwd`, when it is a string — how a team maps onto an ingested project.
    pub cwd: Option<String>,
    /// Whether this member leads the team.
    pub is_lead: bool,
    /// The verbatim spawn prompt (sub-agents only).
    pub prompt: Option<String>,
}

/// One Claude Code team, from `config.json` or reconstructed from transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRecord {
    /// The team name — the directory name, or the `TeamCreate` `team_name`.
    pub team_id: String,
    /// ISO 8601, converted from the config's epoch-ms `createdAt`.
    pub created_ts: String,
    /// The team's blurb.
    pub description: Option<String>,
    /// The lead's session id, when the config names one.
    pub lead_session_id: Option<String>,
    /// The lead's agent id.
    pub lead_agent_id: Option<String>,
    /// The lead member's `cwd`, else the first member's — best effort.
    pub project_path: Option<String>,
    /// The roster.
    pub members: Vec<MemberRecord>,
    /// The verbatim `config.json` text, or the synthesised stand-in.
    pub config_json: String,
}

/// One task assignment from `~/.claude/tasks/{team}/{N}.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// `id`, or the file stem when the file carries none.
    pub task_id: String,
    /// `owner`, when it is a non-empty string — matches a member's `name`.
    pub owner_name: Option<String>,
    /// `subject`.
    pub subject: Option<String>,
    /// `description` — the spawn-prompt fallback.
    pub description: Option<String>,
    /// `status`.
    pub status: Option<String>,
}

/// What [`link_sessions_to_team`] needs to know about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTeamHint {
    /// The session's own id.
    pub session_id: String,
    /// `teamName` from the session's first message, when present.
    pub team_name: Option<String>,
    /// `agentId` from the session's first message, when present.
    pub agent_id: Option<String>,
    /// Whether any message in the session is a sidechain row.
    pub has_sidechain: bool,
    /// Every `uuid` in the session — only populated for team-shaped sessions.
    pub uuids: BTreeSet<String>,
    /// Every `parent_uuid` in the session.
    pub parent_uuids: BTreeSet<String>,
}

/// Resolved team affiliation for one session — what gets written to `sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTeamLink {
    /// `sessions.team_id`.
    pub team_id: String,
    /// `sessions.agent_role` — [`ROLE_LEAD`] or [`ROLE_SUBAGENT`].
    pub role: &'static str,
    /// `sessions.spawn_prompt`.
    pub spawn_prompt: Option<String>,
    /// `sessions.spawned_by_session_id`.
    pub parent_session_id: Option<String>,
}

/// Summary of a [`materialize_team_metadata`] run (for logging/tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    /// Teams the filesystem scan produced.
    pub teams_seen: usize,
    /// Teams that reached the `agent_teams` upsert.
    pub teams_materialized: usize,
    /// `cur.rowcount` summed over the `sessions` UPDATEs.
    pub sessions_linked: i64,
    /// The `_log.warning("… rolled back: %s")` line, when the transaction was
    /// rolled back.
    ///
    /// The reference logs it and returns a zeroed report, so a caller that only
    /// reads the counts cannot tell a rolled-back pass from an empty one. The
    /// counts stay zeroed here for exactly that parity, and the reason is
    /// carried alongside rather than thrown away — the campaign's rule that
    /// diagnostics are *returned*, not logged (`writer.rs`).
    pub rollback_note: Option<String>,
}

// ── private helpers ──────────────────────────────────────────────────────────

/// `_safe_json_load_text` — `json.loads`, with the reference's `except
/// (JSONDecodeError, TypeError, ValueError)` folded into the return type.
///
/// The falsy guard is Python's: `if not text: return None`, so an empty string
/// never reaches the parser.
fn safe_json_load_text(text: Option<&str>) -> Option<Value> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    pyloads::loads(Some(text))
}

/// `int(value)` → `datetime.fromtimestamp(ms / 1000, tz=UTC).isoformat()`.
///
/// Every failure mode collapses to `""`: a `TypeError`/`ValueError` from
/// `int()`, a non-positive millisecond count, and the
/// `(OverflowError, OSError, ValueError)` branch of `fromtimestamp`.
///
/// The division is `ms / 1000` — **float** true division, not the integer
/// arithmetic `pyval::epoch_ms_to_iso` uses for `claude.py`'s same-named helper.
/// They agree on every millisecond value that is exactly representable, which is
/// all of them below 2^53; the float path is used anyway because it is what this
/// module's source line says, and the two would part company on a `createdAt`
/// large enough to lose the millisecond.
fn epoch_ms_to_iso(value: Option<&Value>) -> String {
    // `pyval::safe_int` is `int(x)` with every raise mapped to 0 and negatives
    // clamped — and 0 is the `ms <= 0` branch, so the collapse is exact.
    let ms = pyval::safe_int(value);
    if ms <= 0 {
        return String::new();
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "CPython's int/int true division rounds once to the nearest \
        double; below 2^53 the conversion is exact and this is that same rounding"
    )]
    let seconds = ms as f64 / 1000.0;
    pyval::epoch_seconds_to_iso(seconds).unwrap_or_default()
}

/// Claude Code's project-directory slug for an absolute *path*.
///
/// `re.sub(r"[^A-Za-z0-9]", "-", path or "")` — per *code point*, so a non-ASCII
/// character is one dash and not one dash per UTF-8 byte.
#[must_use]
pub fn slug_for_path(path: &str) -> String {
    path.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

/// `"worker-1@my-team"` → `"worker-1"`; a non-suffixed id passes through.
///
/// `None` for the falsy input (`if not agent_id`), which includes `""`.
fn strip_team_suffix(agent_id: Option<&str>) -> Option<&str> {
    let agent_id = agent_id?;
    if agent_id.is_empty() {
        return None;
    }
    Some(agent_id.split_once('@').map_or(agent_id, |(head, _)| head))
}

/// `value.get(key)` when `value` is an object — `dict.get` with `None` for
/// every other type.
fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|object| object.get(key))
}

/// `x if isinstance(x, str) else None`.
fn as_str_or_none(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
}

/// One directory's entries in `readdir` order — `os.scandir`, not `sorted`.
///
/// An `OSError` is the reference's `except OSError: return []`, so the caller
/// gets an empty list and never a partial one.
fn read_dir_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let reader = std::fs::read_dir(dir).ok()?;
    Some(
        reader
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect(),
    )
}

// ── discover_teams ───────────────────────────────────────────────────────────

/// Scan `{claude_root}/teams/` for `config.json` files.
///
/// One [`TeamRecord`] per team directory with a parseable config, sorted by
/// directory name. Never raises: a malformed config is a skipped team.
#[must_use]
pub fn discover_teams(claude_root: &Path) -> Vec<TeamRecord> {
    let teams_dir = claude_root.join("teams");
    if !teams_dir.is_dir() {
        return Vec::new();
    }
    let Some(mut entries) = read_dir_entries(&teams_dir) else {
        return Vec::new();
    };
    // `sorted(teams_dir.iterdir(), key=lambda p: p.name)`.
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let mut out = Vec::new();
    for team_dir in entries {
        if !team_dir.is_dir() {
            continue;
        }
        let config_path = team_dir.join("config.json");
        if !config_path.is_file() {
            continue;
        }
        // `except OSError: continue`. A non-UTF-8 config would raise
        // `UnicodeDecodeError` in the reference and escape this loop entirely;
        // it is unreachable in practice (Claude Code writes JSON) and the one
        // reachable half — the I/O error — is what `read_to_string` reports here
        // alongside it. Recorded rather than emulated: see DIV-312.
        let Ok(config_text) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        let Some(config) = safe_json_load_text(Some(&config_text)) else {
            continue;
        };
        if !config.is_object() {
            continue;
        }

        let team_id = team_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lead_agent_id_raw = get(&config, "leadAgentId");
        let created_ts = epoch_ms_to_iso(get(&config, "createdAt"));

        let mut members: Vec<MemberRecord> = Vec::new();
        let mut lead_cwd: Option<String> = None;
        let mut first_cwd: Option<String> = None;
        if let Some(Value::Array(raw_members)) = get(&config, "members") {
            for raw_m in raw_members {
                if !raw_m.is_object() {
                    continue;
                }
                let Some(m_agent_id) = get(raw_m, "agentId").and_then(Value::as_str) else {
                    continue;
                };
                if m_agent_id.is_empty() {
                    continue;
                }
                // `raw_m.get("name") or _strip_team_suffix(agent_id) or agent_id`
                // — an `or` chain on TRUTHINESS, and the winner is `str()`-ed,
                // so a numeric `name` becomes its decimal form.
                let name_value = get(raw_m, "name");
                let m_name = match name_value {
                    Some(value) if pyval::py_truthy(value) => pyval::py_str(value),
                    _ => strip_team_suffix(Some(m_agent_id))
                        .unwrap_or(m_agent_id)
                        .to_string(),
                };
                let m_cwd = as_str_or_none(get(raw_m, "cwd"));
                let agent_type = as_str_or_none(get(raw_m, "agentType"));
                let is_lead =
                    lead_agent_id_raw.is_some_and(|value| {
                        pyval::py_truthy(value) && value.as_str() == Some(m_agent_id)
                    }) || matches!(agent_type.as_deref(), Some("team-lead" | "orchestrator"))
                        || m_name == "team-lead";
                if first_cwd.is_none()
                    && let Some(cwd) = m_cwd.as_ref()
                    && !cwd.is_empty()
                {
                    first_cwd = Some(cwd.clone());
                }
                if is_lead
                    && lead_cwd.is_none()
                    && let Some(cwd) = m_cwd.as_ref()
                    && !cwd.is_empty()
                {
                    lead_cwd = Some(cwd.clone());
                }
                members.push(MemberRecord {
                    agent_id: m_agent_id.to_string(),
                    name: m_name,
                    agent_type,
                    model: as_str_or_none(get(raw_m, "model")),
                    cwd: m_cwd,
                    is_lead,
                    prompt: as_str_or_none(get(raw_m, "prompt")),
                });
            }
        }

        out.push(TeamRecord {
            team_id,
            created_ts,
            description: as_str_or_none(get(&config, "description")),
            lead_session_id: as_str_or_none(get(&config, "leadSessionId")),
            lead_agent_id: as_str_or_none(lead_agent_id_raw),
            project_path: lead_cwd.or(first_cwd),
            members,
            config_json: config_text,
        });
    }
    out
}

// ── discover_teams_from_jsonl ────────────────────────────────────────────────

/// What one team name accumulated across the transcript walk.
#[derive(Debug, Default)]
struct JsonlTeam {
    lead_session_id: Option<String>,
    description: Option<String>,
    created_ts: String,
    /// `data["members"]` — a dict keyed by member name, LAST write wins, and
    /// emitted `sorted(...)`, so a `BTreeMap` is both semantics and order.
    members: BTreeMap<String, MemberRecord>,
}

/// `dict[worker_session_id, (teammate_name, team_name)]` — the linker's
/// deleted-config fallback, keyed by the worker's own session id.
pub type WorkerMap = BTreeMap<String, (String, String)>;

/// Reconstruct [`TeamRecord`]s by parsing tool-use blocks in session JSONLs.
///
/// Returns the synthetic teams (one per team name that some `TeamCreate`
/// created, sorted by name) and the [`WorkerMap`].
///
/// `None` is the reference's `except Exception` path — see this module's header
/// on why a single non-UTF-8 transcript abandons the whole walk.
#[must_use]
pub fn discover_teams_from_jsonl(claude_root: &Path) -> Option<(Vec<TeamRecord>, WorkerMap)> {
    let projects_dir = claude_root.join("projects");
    if !projects_dir.is_dir() {
        return Some((Vec::new(), BTreeMap::new()));
    }

    let mut teams_data: BTreeMap<String, JsonlTeam> = BTreeMap::new();
    let mut worker_map: BTreeMap<String, (String, String)> = BTreeMap::new();

    // `projects_dir.glob("*/*.jsonl")` — readdir order at both levels, and the
    // `*/` component matches directories only.
    let Some(project_dirs) = read_dir_entries(&projects_dir) else {
        return Some((Vec::new(), BTreeMap::new()));
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for project_dir in project_dirs {
        if !project_dir.is_dir() {
            continue;
        }
        let Some(files) = read_dir_entries(&project_dir) else {
            continue;
        };
        paths.extend(
            files
                .into_iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")),
        );
    }

    for path in paths {
        if !path.is_file() {
            continue;
        }
        let session_id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();

        // `except OSError: continue` covers the read; the decode does not have a
        // handler at all, and its failure abandons everything (module header).
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8(bytes).ok()?;

        let mut first_user_text: Option<String> = None;
        for line in text.split('\n') {
            // `str.splitlines()` also splits on \r, \v, \f, \x1c-\x1e, \x85 and
            // the two Unicode separators; a JSONL line containing any of those
            // raw would already have failed `json.loads`, so the cheap split is
            // equivalent here. The trailing `\r` of a CRLF file is left on the
            // line by neither: `splitlines` strips it, and `json.loads` ignores
            // trailing whitespace.
            if line.trim().is_empty() {
                continue;
            }
            // The reference's pre-filter, transcribed exactly — it is what keeps
            // this walk from `json.loads`-ing a gigabyte.
            if first_user_text.is_some() {
                if !line.contains("\"TeamCreate\"") && !line.contains("\"Agent\"") {
                    continue;
                }
            } else if !line.contains("\"user\"")
                && !line.contains("\"TeamCreate\"")
                && !line.contains("\"Agent\"")
            {
                continue;
            }

            let Some(rec) = pyloads::loads(Some(line)) else {
                continue; // `except Exception: continue`
            };
            let record_type = get(&rec, "type").and_then(Value::as_str);

            if record_type == Some("user") && first_user_text.is_none() {
                let message = get(&rec, "message");
                let content = message.and_then(|m| get(m, "content"));
                match content {
                    Some(Value::String(text)) => {
                        if !text.trim().is_empty() {
                            first_user_text = Some(text.clone());
                        }
                    }
                    Some(Value::Array(blocks)) => {
                        let mut parts: Vec<String> = Vec::new();
                        for block in blocks {
                            if block.is_object()
                                && get(block, "type").and_then(Value::as_str) == Some("text")
                            {
                                // `blk.get("text", "")` — a non-string `text`
                                // would raise in `"\n".join`; the reference
                                // would crash the whole walk, so a non-string is
                                // the abandon path, not a coerced value.
                                match get(block, "text") {
                                    None => parts.push(String::new()),
                                    Some(Value::String(value)) => parts.push(value.clone()),
                                    Some(_) => return None,
                                }
                            }
                        }
                        let concatenated = parts.join("\n");
                        if !concatenated.trim().is_empty() {
                            first_user_text = Some(concatenated);
                        }
                    }
                    _ => {}
                }
            }

            if record_type == Some("assistant") {
                let content = get(&rec, "message").and_then(|m| get(m, "content"));
                let Some(Value::Array(blocks)) = content else {
                    continue;
                };
                for block in blocks {
                    if !block.is_object()
                        || get(block, "type").and_then(Value::as_str) != Some("tool_use")
                    {
                        continue;
                    }
                    let name = get(block, "name").and_then(Value::as_str);
                    let input = get(block, "input");
                    let team_name = input
                        .and_then(|inp| get(inp, "team_name"))
                        .and_then(Value::as_str);
                    match name {
                        Some("TeamCreate") => {
                            let Some(team_name) = team_name.filter(|name| !name.is_empty()) else {
                                continue;
                            };
                            let created_ts = get(&rec, "timestamp")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            // `setdefault` — the FIRST TeamCreate for a name
                            // owns the lead session; a later one changes nothing.
                            teams_data
                                .entry(team_name.to_string())
                                .or_insert(JsonlTeam {
                                    lead_session_id: Some(session_id.clone()),
                                    description: input
                                        .and_then(|inp| get(inp, "description"))
                                        .and_then(Value::as_str)
                                        .map(std::string::ToString::to_string),
                                    created_ts,
                                    members: BTreeMap::new(),
                                });
                        }
                        Some("Agent") => {
                            let member_name = input
                                .and_then(|inp| get(inp, "name"))
                                .and_then(Value::as_str);
                            let subagent_type = input
                                .and_then(|inp| get(inp, "subagent_type"))
                                .and_then(Value::as_str);
                            // The reference `continue`s on Explore agents — they
                            // are read-only fan-out, not team members.
                            if subagent_type == Some("Explore") {
                                continue;
                            }
                            let (Some(team_name), Some(member_name)) = (
                                team_name.filter(|name| !name.is_empty()),
                                member_name.filter(|name| !name.is_empty()),
                            ) else {
                                continue;
                            };
                            let prompt = input
                                .and_then(|inp| get(inp, "prompt"))
                                .and_then(Value::as_str)
                                .map(std::string::ToString::to_string);
                            let entry =
                                teams_data.entry(team_name.to_string()).or_insert_with(|| {
                                    JsonlTeam {
                                        lead_session_id: None,
                                        description: None,
                                        created_ts: get(&rec, "timestamp")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        members: BTreeMap::new(),
                                    }
                                });
                            // Plain assignment, NOT setdefault: the last `Agent`
                            // call for a member name owns its prompt.
                            entry.members.insert(
                                member_name.to_string(),
                                MemberRecord {
                                    agent_id: member_name.to_string(),
                                    name: member_name.to_string(),
                                    agent_type: subagent_type.map(std::string::ToString::to_string),
                                    model: None,
                                    cwd: None,
                                    is_lead: false,
                                    prompt,
                                },
                            );
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(first_user_text) = first_user_text
            && let Some((teammate_name, team_name)) = builder_match(&first_user_text)
        {
            worker_map.insert(
                session_id,
                (teammate_name.to_string(), team_name.to_string()),
            );
        }
    }

    let mut synthetic_teams = Vec::new();
    for (team_name, data) in teams_data {
        let Some(lead_session_id) = data.lead_session_id else {
            continue; // a team nobody created is not a team
        };

        let mut members_list = vec![MemberRecord {
            agent_id: "team-lead".to_string(),
            name: "team-lead".to_string(),
            agent_type: Some("orchestrator".to_string()),
            model: None,
            cwd: None,
            is_lead: true,
            prompt: None,
        }];
        members_list.extend(data.members.into_values());

        // `int(datetime.fromisoformat(ts).timestamp() * 1000)` — truncation
        // toward zero, and any parse failure leaves the 0.
        let created_epoch = if data.created_ts.is_empty() {
            0
        } else {
            pytime::parse_iso(&data.created_ts).map_or(0, |seconds| {
                let millis = seconds * 1000.0;
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "int() truncates toward zero; an out-of-range value \
                    saturates here where CPython would keep it, and no epoch in \
                    a transcript is within 12 orders of magnitude of that"
                )]
                let truncated = millis.trunc() as i64;
                truncated
            })
        };

        let config_json = pyjson::dumps_default(&pyjson::Value::Object(vec![
            (
                "_source".to_string(),
                pyjson::Value::Str("jsonl_fallback".to_string()),
            ),
            (
                "leadSessionId".to_string(),
                pyjson::Value::Str(lead_session_id.clone()),
            ),
            (
                "description".to_string(),
                opt_str(data.description.as_deref()),
            ),
            ("createdAt".to_string(), pyjson::Value::Int(created_epoch)),
            (
                "members".to_string(),
                pyjson::Value::Array(
                    members_list
                        .iter()
                        .map(|m| {
                            pyjson::Value::Object(vec![
                                (
                                    "agentId".to_string(),
                                    pyjson::Value::Str(m.agent_id.clone()),
                                ),
                                ("name".to_string(), pyjson::Value::Str(m.name.clone())),
                                ("agentType".to_string(), opt_str(m.agent_type.as_deref())),
                                ("model".to_string(), opt_str(m.model.as_deref())),
                                ("cwd".to_string(), opt_str(m.cwd.as_deref())),
                                ("isLead".to_string(), pyjson::Value::Bool(m.is_lead)),
                                ("prompt".to_string(), opt_str(m.prompt.as_deref())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]));

        synthetic_teams.push(TeamRecord {
            team_id: team_name,
            created_ts: data.created_ts,
            description: data.description,
            lead_session_id: Some(lead_session_id),
            lead_agent_id: Some("team-lead".to_string()),
            project_path: None,
            members: members_list,
            config_json,
        });
    }

    Some((synthetic_teams, worker_map))
}

/// `str | None` → a JSON value.
fn opt_str(value: Option<&str>) -> pyjson::Value {
    value.map_or(pyjson::Value::Null, |text| {
        pyjson::Value::Str(text.to_string())
    })
}

// ── BUILDER_RE ───────────────────────────────────────────────────────────────

/// `BUILDER_RE.search(text)` — the worker-transcript preamble matcher.
///
/// ```text
/// r'You are `([^`]+)`\s*(?:,?\s*(?:teammate\s+)?(?:on|in\s+team)\s*)`([^`]+)`'
/// ```
///
/// Hand-written rather than pulled in with the `regex` crate: this is the only
/// regular expression in the module, the pattern is fixed at compile time, and a
/// new third-party crate in the lock file is a campaign-wide cost (finding 9,
/// and the `notify` entry in `Cargo.toml` documents the bar). Every backtracking
/// point the engine has is reproduced below and pinned by tests:
///
/// * `[^`]+` cannot cross a backtick, so its greedy run is its only run — the
///   following `` ` `` matches at exactly one place or the attempt fails.
/// * `\s*` runs are greedy, and a shorter run can never help: the token after
///   each one is either non-whitespace or another `\s*`.
/// * `(?:teammate\s+)?` is greedy — present is tried first, absent second.
/// * `(?:on|in\s+team)` is ordered — `on` first.
///
/// `re.search` walks start positions left to right, and every match must begin
/// with the literal `` You are ` ``, so the scan is over those occurrences.
fn builder_match(text: &str) -> Option<(&str, &str)> {
    const PREFIX: &str = "You are `";
    let mut from = 0;
    while let Some(offset) = text[from..].find(PREFIX) {
        let start = from + offset + PREFIX.len();
        from = from + offset + 1;
        let rest = &text[start..];
        let Some(close) = rest.find('`') else {
            continue;
        };
        if close == 0 {
            continue; // `[^`]+` needs at least one character
        }
        let teammate = &rest[..close];
        let after = &rest[close + 1..];
        if let Some(team) = builder_tail(after) {
            return Some((teammate, team));
        }
    }
    None
}

/// The `\s*(?:,?\s*(?:teammate\s+)?(?:on|in\s+team)\s*)`…`` tail.
fn builder_tail(after: &str) -> Option<&str> {
    let after = after.trim_start_matches(char::is_whitespace);
    let after = after.strip_prefix(',').unwrap_or(after);
    let after = after.trim_start_matches(char::is_whitespace);
    // `(?:teammate\s+)?` is greedy: with it first, without it second.
    for body in ["teammate", ""] {
        let candidate = if body.is_empty() {
            Some(after)
        } else {
            after.strip_prefix(body).and_then(|rest| {
                // `\s+` — at least one.
                let trimmed = rest.trim_start_matches(char::is_whitespace);
                (trimmed.len() < rest.len()).then_some(trimmed)
            })
        };
        let Some(candidate) = candidate else { continue };
        for keyword in ["on", "in"] {
            let Some(rest) = candidate.strip_prefix(keyword) else {
                continue;
            };
            let rest = if keyword == "in" {
                // `in\s+team`
                let trimmed = rest.trim_start_matches(char::is_whitespace);
                if trimmed.len() == rest.len() {
                    continue;
                }
                let Some(rest) = trimmed.strip_prefix("team") else {
                    continue;
                };
                rest
            } else {
                rest
            };
            let rest = rest.trim_start_matches(char::is_whitespace);
            let Some(rest) = rest.strip_prefix('`') else {
                continue;
            };
            let Some(close) = rest.find('`') else {
                continue;
            };
            if close == 0 {
                continue;
            }
            return Some(&rest[..close]);
        }
    }
    None
}

// ── discover_tasks ───────────────────────────────────────────────────────────

/// Scan `{claude_root}/tasks/{team_id}/` for task-assignment JSON files.
///
/// Sorted by numeric task id, with non-numeric ids after the numeric ones and
/// in string order among themselves — `(1 << 30, task_id)` in the reference.
///
/// # Errors
/// A task file that is not valid UTF-8: the reference's `except OSError` around
/// `read_text()` does not catch `UnicodeDecodeError`, and the exception escapes
/// this function. See the module header.
pub fn discover_tasks(claude_root: &Path, team_id: &str) -> Result<Vec<TaskRecord>> {
    let tasks_dir = claude_root.join("tasks").join(team_id);
    if !tasks_dir.is_dir() {
        return Ok(Vec::new());
    }
    let Some(entries) = read_dir_entries(&tasks_dir) else {
        return Ok(Vec::new());
    };

    let mut out: Vec<TaskRecord> = Vec::new();
    for task_path in entries {
        if !task_path.is_file() {
            continue;
        }
        let name = task_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if TASK_SKIP_FILES.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        if task_path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // `_safe_json_load_file`: `except OSError: return None` around the read,
        // and a decode failure propagates.
        let obj = match std::fs::read(&task_path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|err| anyhow::anyhow!("{}: {err}", task_path.display()))?;
                safe_json_load_text(Some(&text))
            }
            Err(_) => None,
        };
        let Some(obj) = obj else { continue };
        if !obj.is_object() {
            continue;
        }
        // `task_id = obj.get("id")`; `if task_id is None: task_id = stem`, then
        // `str(task_id)` — so a numeric id keeps Python's `str()` form.
        let task_id = match get(&obj, "id") {
            None | Some(Value::Null) => task_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Some(value) => pyval::py_str(value),
        };
        let owner_name = get(&obj, "owner")
            .and_then(Value::as_str)
            .filter(|owner| !owner.is_empty())
            .map(std::string::ToString::to_string);
        out.push(TaskRecord {
            task_id,
            owner_name,
            subject: as_str_or_none(get(&obj, "subject")),
            description: as_str_or_none(get(&obj, "description")),
            status: as_str_or_none(get(&obj, "status")),
        });
    }

    // `list.sort` is stable on both sides, so equal keys keep readdir order.
    out.sort_by_key(sort_key);
    Ok(out)
}

/// `(int(task_id), "")` when it parses, `(1 << 30, task_id)` when it does not.
fn sort_key(task: &TaskRecord) -> (i64, String) {
    task.task_id.trim().parse::<i64>().map_or_else(
        |_| (1 << 30, task.task_id.clone()),
        |number| (number, String::new()),
    )
}

// ── link_sessions_to_team ────────────────────────────────────────────────────

/// An insertion-ordered `dict[str, SessionTeamLink]`.
///
/// `out` in the reference is a plain dict, and its **insertion order** is read
/// back by `for sid in list(out.keys())` when the chain fallback seeds
/// `uuid_owner`. A `HashMap` would lose that; a `BTreeMap` would replace it with
/// a different one.
#[derive(Debug, Default)]
struct LinkMap(Vec<(String, SessionTeamLink)>);

impl LinkMap {
    fn get(&self, key: &str) -> Option<&SessionTeamLink> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, link)| link)
    }

    fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// `d[k] = v` — replace in place, or append.
    fn insert(&mut self, key: String, value: SessionTeamLink) {
        match self.0.iter_mut().find(|(name, _)| *name == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }
}

/// Pick the richest spawn prompt for a sub-agent session.
///
/// The member's `prompt` → the owning task's `description` → the lone unowned
/// task's `description` → `None`.
fn spawn_prompt_for(
    hint: &SessionTeamHint,
    team: &TeamRecord,
    tasks: &[TaskRecord],
) -> Option<String> {
    let aid = hint.agent_id.as_deref();
    let aid_bare = strip_team_suffix(aid);
    let member = aid.and_then(|aid| {
        team.members.iter().find(|m| {
            m.agent_id == aid
                || m.name == aid
                || aid_bare.is_some_and(|bare| m.name == bare || m.agent_id == bare)
        })
    });
    if let Some(member) = member
        && let Some(prompt) = member.prompt.as_ref()
        && !prompt.is_empty()
    {
        return Some(prompt.clone());
    }

    let member_name = member.map_or(aid_bare, |m| Some(m.name.as_str()));
    if let Some(member_name) = member_name.filter(|name| !name.is_empty()) {
        for task in tasks {
            if task.owner_name.as_deref() == Some(member_name)
                && let Some(description) = task.description.as_ref()
                && !description.is_empty()
            {
                return Some(description.clone());
            }
        }
    }
    // Single-task teams sometimes omit `owner` — use the lone task.
    if tasks.len() == 1
        && let Some(description) = tasks[0].description.as_ref()
        && !description.is_empty()
        && !tasks
            .iter()
            .any(|t| t.owner_name.as_ref().is_some_and(|owner| !owner.is_empty()))
    {
        return Some(description.clone());
    }
    None
}

/// Map `session_id` → [`SessionTeamLink`] for every linkable session.
///
/// Resolution order: the config's lead → a `teamName` match → the worker-map
/// fallback → the `parent_uuid` chain, iterated to a fixpoint.
#[must_use]
fn link_sessions_to_team(
    session_hints: &[SessionTeamHint],
    teams: &[TeamRecord],
    tasks_by_team: &BTreeMap<String, Vec<TaskRecord>>,
    worker_map: &WorkerMap,
) -> Vec<(String, SessionTeamLink)> {
    let team_by_name: HashMap<&str, &TeamRecord> = teams
        .iter()
        .map(|team| (team.team_id.as_str(), team))
        .collect();
    let mut out = LinkMap::default();

    // 1. leads
    for team in teams {
        if let Some(lead) = team.lead_session_id.as_ref().filter(|id| !id.is_empty()) {
            out.insert(
                lead.clone(),
                SessionTeamLink {
                    team_id: team.team_id.clone(),
                    role: ROLE_LEAD,
                    spawn_prompt: None,
                    parent_session_id: None,
                },
            );
        }
    }

    // 2. teamName matches
    for hint in session_hints {
        let Some(team_name) = hint.team_name.as_deref().filter(|name| !name.is_empty()) else {
            continue;
        };
        let Some(team) = team_by_name.get(team_name) else {
            continue;
        };
        if Some(hint.session_id.as_str()) == team.lead_session_id.as_deref() {
            continue; // already a lead
        }
        if out
            .get(&hint.session_id)
            .is_some_and(|link| link.role == ROLE_LEAD)
        {
            continue; // never downgrade a lead
        }
        let tasks = tasks_by_team
            .get(&team.team_id)
            .map_or(&[][..], Vec::as_slice);
        out.insert(
            hint.session_id.clone(),
            SessionTeamLink {
                team_id: team.team_id.clone(),
                role: ROLE_SUBAGENT,
                spawn_prompt: spawn_prompt_for(hint, team, tasks),
                parent_session_id: team.lead_session_id.clone(),
            },
        );
    }

    // 2.5 worker_map fallback matches
    if !worker_map.is_empty() {
        for hint in session_hints {
            if out.contains(&hint.session_id) {
                continue;
            }
            let Some((teammate_name, team_name)) = worker_map.get(&hint.session_id) else {
                continue;
            };
            let Some(team) = team_by_name.get(team_name.as_str()) else {
                continue;
            };
            if Some(hint.session_id.as_str()) == team.lead_session_id.as_deref() {
                continue;
            }
            let spawn_prompt = team
                .members
                .iter()
                .find(|m| &m.name == teammate_name || &m.agent_id == teammate_name)
                .and_then(|m| m.prompt.clone());
            out.insert(
                hint.session_id.clone(),
                SessionTeamLink {
                    team_id: team.team_id.clone(),
                    role: ROLE_SUBAGENT,
                    spawn_prompt,
                    parent_session_id: team.lead_session_id.clone(),
                },
            );
        }
    }

    // 3. parent_uuid chain fallback (older transcripts without teamName)
    let hint_by_id: HashMap<&str, &SessionTeamHint> = session_hints
        .iter()
        .map(|hint| (hint.session_id.as_str(), hint))
        .collect();
    let mut uuid_owner: HashMap<String, String> = HashMap::new();
    let seeded: Vec<String> = out.0.iter().map(|(sid, _)| sid.clone()).collect();
    for sid in seeded {
        let Some(hint) = hint_by_id.get(sid.as_str()) else {
            continue;
        };
        for uuid in &hint.uuids {
            uuid_owner
                .entry(uuid.clone())
                .or_insert_with(|| sid.clone());
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for hint in session_hints {
            if out.contains(&hint.session_id) || !hint.has_sidechain {
                continue;
            }
            // DIV-311: the reference breaks on the first parent uuid that has an
            // owner, over a `frozenset` — hash order, and `str` hashing is
            // seeded per process. Sorted order is deterministic instead.
            let owner_sid = hint
                .parent_uuids
                .iter()
                .find_map(|parent| uuid_owner.get(parent).cloned());
            let Some(owner_sid) = owner_sid else { continue };
            let Some(owner_link) = out.get(&owner_sid) else {
                continue;
            };
            let link = SessionTeamLink {
                team_id: owner_link.team_id.clone(),
                role: ROLE_SUBAGENT,
                spawn_prompt: None,
                parent_session_id: Some(owner_sid),
            };
            out.insert(hint.session_id.clone(), link);
            for uuid in &hint.uuids {
                uuid_owner
                    .entry(uuid.clone())
                    .or_insert_with(|| hint.session_id.clone());
            }
            changed = true;
        }
    }

    out.0
}

// ── materialize_team_metadata (ingest-time orchestrator) ─────────────────────

fn project_id_for_session(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT project_id FROM sessions WHERE session_id = ? LIMIT 1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
}

fn project_id_for_slug(
    conn: &Connection,
    provider: &str,
    slug: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM projects WHERE provider = ? AND slug = ? LIMIT 1",
        [provider, slug],
        |row| row.get(0),
    )
    .optional()
}

/// Build a [`SessionTeamHint`] for every session in `project_ids`.
///
/// The cheap path peeks each session's first message; only a session that looks
/// team-related pays for the "fetch every uuid" pass the chain fallback needs.
fn build_hints_for_projects(
    conn: &Connection,
    project_ids: &BTreeSet<i64>,
    team: &TeamRecord,
) -> rusqlite::Result<Vec<SessionTeamHint>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; project_ids.len()].join(",");
    let sql = format!("SELECT id, session_id FROM sessions WHERE project_id IN ({placeholders})");
    let params: Vec<i64> = project_ids.iter().copied().collect();
    let mut stmt = conn.prepare(&sql)?;
    let sessions: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut hints = Vec::with_capacity(sessions.len());
    for (session_fk, session_id) in sessions {
        let first: Option<String> = conn
            .query_row(
                "SELECT raw_json, parent_uuid FROM messages WHERE session_fk = ? ORDER BY seq LIMIT 1",
                [session_fk],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let raw = safe_json_load_text(first.as_deref()).filter(Value::is_object);
        let team_name = raw
            .as_ref()
            .and_then(|raw| as_str_or_none(get(raw, "teamName")));
        let agent_id = raw
            .as_ref()
            .and_then(|raw| as_str_or_none(get(raw, "agentId")));
        let has_sidechain = conn
            .query_row(
                "SELECT 1 FROM messages WHERE session_fk = ? AND is_sidechain = 1 LIMIT 1",
                [session_fk],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        let mut uuids = BTreeSet::new();
        let mut parent_uuids = BTreeSet::new();
        if Some(session_id.as_str()) == team.lead_session_id.as_deref()
            || team_name.as_deref() == Some(team.team_id.as_str())
            || has_sidechain
        {
            let mut stmt =
                conn.prepare("SELECT uuid, parent_uuid FROM messages WHERE session_fk = ?")?;
            let rows: Vec<(Option<String>, Option<String>)> = stmt
                .query_map([session_fk], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (uuid, parent_uuid) in rows {
                // `frozenset(r["uuid"] for r in ur if r["uuid"])` — TRUTHINESS,
                // so the empty string is dropped along with NULL.
                if let Some(uuid) = uuid.filter(|value| !value.is_empty()) {
                    uuids.insert(uuid);
                }
                if let Some(parent) = parent_uuid.filter(|value| !value.is_empty()) {
                    parent_uuids.insert(parent);
                }
            }
        }
        hints.push(SessionTeamHint {
            session_id,
            team_name,
            agent_id,
            has_sidechain,
            uuids,
            parent_uuids,
        });
    }
    Ok(hints)
}

/// Scan `~/.claude/teams/` + `~/.claude/tasks/` and write the indexed team
/// metadata: `agent_teams` rows + the four `sessions` team columns.
///
/// Idempotent — re-running over the same filesystem state produces the same
/// rows (`agent_teams` upserts on `team_id`; the `sessions` UPDATE is a straight
/// overwrite). A missing / empty `~/.claude/teams/` with no team-shaped
/// transcripts is a no-op.
///
/// # Errors
/// Only the paths the reference does not catch: a non-UTF-8 task file (see the
/// module header). A `sqlite3.Error` is caught, rolled back, and reported as an
/// empty [`MaterializeReport`] — as it is there.
pub fn materialize_team_metadata(
    conn: &Connection,
    claude_root: &Path,
    provider: &str,
) -> Result<MaterializeReport> {
    let config_teams = discover_teams(claude_root);
    let (fallback_teams, worker_map) =
        discover_teams_from_jsonl(claude_root).unwrap_or_else(|| (Vec::new(), BTreeMap::new()));

    // `teams_dict = {t.team_id: t for t in fallback}` then the config teams
    // overwrite — a dict UPDATE keeps the original position of an existing key
    // and appends a new one, which is what this does.
    let mut teams: Vec<TeamRecord> = fallback_teams;
    for team in config_teams {
        match teams.iter_mut().find(|t| t.team_id == team.team_id) {
            Some(slot) => *slot = team,
            None => teams.push(team),
        }
    }
    if teams.is_empty() {
        return Ok(MaterializeReport::default());
    }

    conn.execute_batch("BEGIN")?;
    match materialize_inner(conn, claude_root, provider, &teams, &worker_map) {
        Ok(report) => {
            conn.execute_batch("COMMIT")?;
            Ok(report)
        }
        Err(MaterializeError::Sqlite(err)) => {
            // `except sqlite3.Error: ROLLBACK; return MaterializeReport()` — the
            // error is logged as a warning and swallowed.
            conn.execute_batch("ROLLBACK")?;
            Ok(MaterializeReport {
                rollback_note: Some(format!(
                    "claude_teams: materialize_team_metadata rolled back: {err}"
                )),
                ..MaterializeReport::default()
            })
        }
        // Everything else escapes the way it does in the reference: no
        // ROLLBACK, the transaction left to the connection's drop.
        Err(MaterializeError::Other(err)) => Err(err),
    }
}

/// The two failure classes the reference distinguishes with `except
/// sqlite3.Error`.
enum MaterializeError {
    Sqlite(rusqlite::Error),
    Other(anyhow::Error),
}

impl From<rusqlite::Error> for MaterializeError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<anyhow::Error> for MaterializeError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

fn materialize_inner(
    conn: &Connection,
    claude_root: &Path,
    provider: &str,
    teams: &[TeamRecord],
    worker_map: &WorkerMap,
) -> std::result::Result<MaterializeReport, MaterializeError> {
    let mut report = MaterializeReport::default();

    for team in teams {
        report.teams_seen += 1;

        // Locate the candidate projects this team's sessions live in.
        let mut candidate_pids: BTreeSet<i64> = BTreeSet::new();
        let mut lead_pid: Option<i64> = None;
        if let Some(lead) = team.lead_session_id.as_ref().filter(|id| !id.is_empty()) {
            lead_pid = project_id_for_session(conn, lead)?;
            if let Some(pid) = lead_pid {
                candidate_pids.insert(pid);
            }
        }
        for member in &team.members {
            let Some(cwd) = member.cwd.as_ref().filter(|cwd| !cwd.is_empty()) else {
                continue;
            };
            if let Some(pid) = project_id_for_slug(conn, provider, &slug_for_path(cwd))? {
                candidate_pids.insert(pid);
            }
        }
        for (worker_sid, (_, team_name)) in worker_map {
            if team_name == &team.team_id
                && let Some(pid) = project_id_for_session(conn, worker_sid)?
            {
                candidate_pids.insert(pid);
            }
        }

        if candidate_pids.is_empty() {
            continue; // nothing ingested for this team yet
        }
        let team_project_id = lead_pid.or_else(|| candidate_pids.iter().copied().next());
        let Some(team_project_id) = team_project_id else {
            continue;
        };

        let hints = build_hints_for_projects(conn, &candidate_pids, team)?;
        let tasks = discover_tasks(claude_root, &team.team_id)?;
        let mut tasks_by_team = BTreeMap::new();
        tasks_by_team.insert(team.team_id.clone(), tasks);
        let team_slice = std::slice::from_ref(team);
        let links = link_sessions_to_team(&hints, team_slice, &tasks_by_team, worker_map);
        if links.is_empty() {
            continue;
        }

        conn.execute(
            "INSERT INTO agent_teams \
             (team_id, project_id, created_ts, description, lead_session_id, config_json) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(team_id) DO UPDATE SET \
               project_id = excluded.project_id, \
               created_ts = excluded.created_ts, \
               description = excluded.description, \
               lead_session_id = excluded.lead_session_id, \
               config_json = excluded.config_json",
            rusqlite::params![
                team.team_id,
                team_project_id,
                // `team.created_ts or ""` — the column is NOT NULL.
                team.created_ts,
                team.description,
                team.lead_session_id,
                team.config_json,
            ],
        )?;
        report.teams_materialized += 1;

        // Claude session ids are UUIDs, so a plain `session_id = ?` overwrite is
        // enough — the reference does not scope the UPDATE to the candidates.
        for (session_id, link) in &links {
            let updated = conn.execute(
                "UPDATE sessions SET team_id = ?, spawned_by_session_id = ?, \
                 spawn_prompt = ?, agent_role = ? WHERE session_id = ?",
                rusqlite::params![
                    link.team_id,
                    link.parent_session_id,
                    link.spawn_prompt,
                    link.role,
                    session_id,
                ],
            )?;
            report.sessions_linked += i64::try_from(updated).unwrap_or(0);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── BUILDER_RE ───────────────────────────────────────────────────────────

    #[test]
    fn builder_re_matches_the_shapes_the_reference_matches() {
        for (text, expected) in [
            ("You are `worker-1` on `my-team`", ("worker-1", "my-team")),
            ("You are `w`, on `t`", ("w", "t")),
            ("You are `w` teammate on `t`", ("w", "t")),
            ("You are `w`, teammate on `t`", ("w", "t")),
            ("You are `w` in team `t`", ("w", "t")),
            ("You are `w`,  teammate  in   team  `t`", ("w", "t")),
            ("prefix\nYou are `w` on `t` suffix", ("w", "t")),
            ("You are `a b c` on `x y`", ("a b c", "x y")),
        ] {
            assert_eq!(builder_match(text), Some(expected), "{text:?}");
        }
    }

    #[test]
    fn builder_re_refuses_the_shapes_the_reference_refuses() {
        for text in [
            "You are `worker-1` of `my-team`",
            "You are `worker-1` in `my-team`", // `in` needs `team`
            "You are `` on `t`",               // `[^`]+` needs one character
            "You are `w` on ``",
            "You are `w` on `t",
            "you are `w` on `t`", // the literal is case-sensitive
            "You are w on t",
        ] {
            assert_eq!(builder_match(text), None, "{text:?}");
        }
    }

    #[test]
    fn builder_re_takes_the_leftmost_match_and_retries_after_a_failure() {
        // `re.search` walks start positions left to right: the first `You are`
        // cannot complete, so the second one answers.
        let text = "You are `alpha` of `beta`. You are `gamma` on `delta`";
        assert_eq!(builder_match(text), Some(("gamma", "delta")));
    }

    #[test]
    fn builder_re_does_not_let_on_swallow_a_word_that_starts_with_it() {
        // `on` matches, then `\s*` and the backtick fail, and the alternation
        // has nothing else — no match, rather than a wrong one.
        assert_eq!(builder_match("You are `w` once `t`"), None);
    }

    // ── slugs, suffixes, epochs ──────────────────────────────────────────────

    #[test]
    fn slug_for_path_replaces_every_non_alphanumeric_code_point() {
        assert_eq!(
            slug_for_path("/Users/me/dev_dev/x/.worktrees/y"),
            "-Users-me-dev-dev-x--worktrees-y"
        );
        assert_eq!(slug_for_path(""), "");
        // One dash per CODE POINT, not per UTF-8 byte.
        assert_eq!(slug_for_path("a—b"), "a-b");
    }

    #[test]
    fn strip_team_suffix_splits_once_and_passes_the_rest_through() {
        assert_eq!(
            strip_team_suffix(Some("worker-1@my-team")),
            Some("worker-1")
        );
        assert_eq!(strip_team_suffix(Some("worker-1")), Some("worker-1"));
        assert_eq!(strip_team_suffix(Some("a@b@c")), Some("a"));
        assert_eq!(strip_team_suffix(Some("")), None);
        assert_eq!(strip_team_suffix(None), None);
    }

    #[test]
    fn epoch_ms_to_iso_collapses_every_failure_to_the_empty_string() {
        assert_eq!(
            epoch_ms_to_iso(Some(&serde_json::json!(1_700_000_000_123_i64))),
            "2023-11-14T22:13:20.123000+00:00"
        );
        // Whole seconds omit the fraction, exactly as `isoformat()` does.
        assert_eq!(
            epoch_ms_to_iso(Some(&serde_json::json!(1_700_000_000_000_i64))),
            "2023-11-14T22:13:20+00:00"
        );
        for garbage in [
            serde_json::json!(null),
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("nope"),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            assert_eq!(epoch_ms_to_iso(Some(&garbage)), "", "{garbage:?}");
        }
        assert_eq!(epoch_ms_to_iso(None), "");
    }

    // ── discover_teams ───────────────────────────────────────────────────────

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-teams-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn discover_teams_reads_a_config_and_picks_the_lead() {
        let root = scratch("cfg");
        write(
            &root.join("teams/alpha/config.json"),
            r#"{"leadAgentId":"lead@alpha","leadSessionId":"S1","description":"d",
                "createdAt":1700000000000,
                "members":[
                  {"agentId":"lead@alpha","name":"team-lead","cwd":"/w/lead","model":"m"},
                  {"agentId":"w1@alpha","cwd":"/w/one","prompt":"do the thing"}
                ]}"#,
        );
        let teams = discover_teams(&root);
        assert_eq!(teams.len(), 1);
        let team = &teams[0];
        assert_eq!(team.team_id, "alpha");
        assert_eq!(team.created_ts, "2023-11-14T22:13:20+00:00");
        assert_eq!(team.lead_session_id.as_deref(), Some("S1"));
        assert_eq!(team.project_path.as_deref(), Some("/w/lead"));
        assert_eq!(team.members.len(), 2);
        assert!(team.members[0].is_lead);
        // No `name` → the `@team`-stripped agent id.
        assert_eq!(team.members[1].name, "w1");
        assert!(!team.members[1].is_lead);
        assert_eq!(team.members[1].prompt.as_deref(), Some("do the thing"));
        // The config text is stored VERBATIM, whitespace and all.
        assert!(team.config_json.contains("leadAgentId"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_teams_skips_the_unparseable_and_the_configless() {
        let root = scratch("skip");
        write(&root.join("teams/broken/config.json"), "{not json");
        write(&root.join("teams/listy/config.json"), "[1,2,3]");
        std::fs::create_dir_all(root.join("teams/inboxes-only/inboxes")).unwrap();
        write(&root.join("teams/good/config.json"), r#"{"members":[]}"#);
        let teams = discover_teams(&root);
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].team_id, "good");
        // A garbage `createdAt` is "" and the column is NOT NULL — that is what
        // `team.created_ts or ""` in the upsert is for.
        assert_eq!(teams[0].created_ts, "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_teams_is_a_no_op_without_a_teams_dir() {
        let root = scratch("noteams");
        assert!(discover_teams(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── discover_teams_from_jsonl ────────────────────────────────────────────

    fn team_create_line(team: &str, description: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"content":[{{"type":"tool_use","name":"TeamCreate","input":{{"team_name":"{team}","description":"{description}"}}}}]}}}}"#
        )
    }

    fn agent_line(team: &str, name: &str, subagent: &str, prompt: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{{"content":[{{"type":"tool_use","name":"Agent","input":{{"team_name":"{team}","name":"{name}","subagent_type":"{subagent}","prompt":"{prompt}"}}}}]}}}}"#
        )
    }

    #[test]
    fn discover_teams_from_jsonl_reconstructs_a_team_and_its_workers() {
        let root = scratch("jsonl");
        let lead = format!(
            "{}\n{}\n{}\n",
            team_create_line("t1", "the blurb", "2026-01-01T00:00:00Z"),
            agent_line("t1", "worker-a", "claude", "worker a prompt"),
            agent_line("t1", "scout", "Explore", "ignored"),
        );
        write(&root.join("projects/-p/LEAD.jsonl"), &lead);
        write(
            &root.join("projects/-p/WORKER.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"You are `worker-a` on `t1`. Go.\"}}\n",
        );

        let (teams, worker_map) = discover_teams_from_jsonl(&root).unwrap();
        assert_eq!(teams.len(), 1);
        let team = &teams[0];
        assert_eq!(team.team_id, "t1");
        assert_eq!(team.lead_session_id.as_deref(), Some("LEAD"));
        assert_eq!(team.description.as_deref(), Some("the blurb"));
        // The synthetic lead is prepended, and `Explore` never became a member.
        assert_eq!(team.members.len(), 2);
        assert_eq!(team.members[0].name, "team-lead");
        assert_eq!(team.members[1].name, "worker-a");
        assert_eq!(team.members[1].prompt.as_deref(), Some("worker a prompt"));
        assert_eq!(
            worker_map.get("WORKER"),
            Some(&("worker-a".to_string(), "t1".to_string()))
        );

        // `json.dumps(config_dict)` — default separators, insertion order, and
        // the epoch round-tripped back to millis.
        assert!(
            team.config_json.starts_with(
                r#"{"_source": "jsonl_fallback", "leadSessionId": "LEAD", "description": "the blurb", "createdAt": 1767225600000, "members": ["#
            ),
            "{}",
            team.config_json
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_team_name_nobody_created_is_not_a_team() {
        // An `Agent` call with no matching `TeamCreate` leaves
        // `lead_session_id` None, and the reference skips it.
        let root = scratch("noleader");
        write(
            &root.join("projects/-p/S.jsonl"),
            &format!("{}\n", agent_line("orphan", "w", "claude", "p")),
        );
        let (teams, _) = discover_teams_from_jsonl(&root).unwrap();
        assert!(teams.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_team_create_owns_the_lead_and_the_last_agent_owns_the_prompt() {
        let root = scratch("firstwins");
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            team_create_line("t", "first", "2026-01-01T00:00:00Z"),
            team_create_line("t", "second", "2026-02-02T00:00:00Z"),
            agent_line("t", "w", "claude", "prompt one"),
            agent_line("t", "w", "claude", "prompt two"),
        );
        write(&root.join("projects/-p/A.jsonl"), &text);
        let (teams, _) = discover_teams_from_jsonl(&root).unwrap();
        assert_eq!(teams[0].description.as_deref(), Some("first"));
        assert_eq!(teams[0].members[1].prompt.as_deref(), Some("prompt two"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_utf8_transcript_abandons_the_whole_walk() {
        // The module header's first error path: `UnicodeDecodeError` is not an
        // `OSError`, so it escapes the per-file `continue` and the reference's
        // `except Exception` throws away every team found so far.
        let root = scratch("badutf8");
        write(
            &root.join("projects/-p/A.jsonl"),
            &format!("{}\n", team_create_line("t", "d", "2026-01-01T00:00:00Z")),
        );
        std::fs::write(root.join("projects/-p/B.jsonl"), [0xff, 0xfe, b'\n']).unwrap();
        assert!(discover_teams_from_jsonl(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── discover_tasks ───────────────────────────────────────────────────────

    #[test]
    fn discover_tasks_sorts_numerically_and_skips_the_non_json() {
        let root = scratch("tasks");
        for (name, body) in [
            ("2.json", r#"{"id":2,"owner":"b","description":"two"}"#),
            ("10.json", r#"{"id":10,"owner":"c","description":"ten"}"#),
            ("1.json", r#"{"id":1,"owner":"a","description":"one"}"#),
            ("x.json", r#"{"subject":"no id"}"#),
            (".lock", "whatever"),
            (".highwatermark", "3"),
            ("notes.txt", "ignored"),
            ("broken.json", "{{{"),
        ] {
            write(&root.join("tasks/t").join(name), body);
        }
        let tasks = discover_tasks(&root, "t").unwrap();
        let ids: Vec<&str> = tasks.iter().map(|t| t.task_id.as_str()).collect();
        // 1, 2, 10 numerically (not lexically), then the file-stem id last.
        assert_eq!(ids, ["1", "2", "10", "x"]);
        assert_eq!(tasks[0].description.as_deref(), Some("one"));
        assert_eq!(tasks[3].owner_name, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_tasks_dir_is_an_empty_list() {
        let root = scratch("notasks");
        assert!(discover_tasks(&root, "nope").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── link_sessions_to_team ────────────────────────────────────────────────

    fn hint(session_id: &str) -> SessionTeamHint {
        SessionTeamHint {
            session_id: session_id.to_string(),
            team_name: None,
            agent_id: None,
            has_sidechain: false,
            uuids: BTreeSet::new(),
            parent_uuids: BTreeSet::new(),
        }
    }

    fn team_with(members: Vec<MemberRecord>, lead: Option<&str>) -> TeamRecord {
        TeamRecord {
            team_id: "T".to_string(),
            created_ts: String::new(),
            description: None,
            lead_session_id: lead.map(std::string::ToString::to_string),
            lead_agent_id: None,
            project_path: None,
            members,
            config_json: "{}".to_string(),
        }
    }

    fn member(agent_id: &str, name: &str, prompt: Option<&str>) -> MemberRecord {
        MemberRecord {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
            agent_type: None,
            model: None,
            cwd: None,
            is_lead: false,
            prompt: prompt.map(std::string::ToString::to_string),
        }
    }

    fn links_of(
        hints: &[SessionTeamHint],
        team: &TeamRecord,
        tasks: Vec<TaskRecord>,
        worker_map: &WorkerMap,
    ) -> BTreeMap<String, SessionTeamLink> {
        let mut by_team = BTreeMap::new();
        by_team.insert(team.team_id.clone(), tasks);
        link_sessions_to_team(hints, std::slice::from_ref(team), &by_team, worker_map)
            .into_iter()
            .collect()
    }

    #[test]
    fn the_lead_is_a_lead_and_a_team_name_match_is_a_subagent() {
        let team = team_with(vec![member("w1", "w1", Some("spawn!"))], Some("LEAD"));
        let mut sub = hint("SUB");
        sub.team_name = Some("T".to_string());
        sub.agent_id = Some("w1".to_string());
        let links = links_of(
            &[hint("LEAD"), sub, hint("OTHER")],
            &team,
            Vec::new(),
            &BTreeMap::new(),
        );
        assert_eq!(links["LEAD"].role, ROLE_LEAD);
        assert_eq!(links["LEAD"].parent_session_id, None);
        assert_eq!(links["SUB"].role, ROLE_SUBAGENT);
        assert_eq!(links["SUB"].parent_session_id.as_deref(), Some("LEAD"));
        assert_eq!(links["SUB"].spawn_prompt.as_deref(), Some("spawn!"));
        assert!(!links.contains_key("OTHER"), "unlinked sessions stay NULL");
    }

    #[test]
    fn a_lead_is_never_downgraded_by_its_own_team_name() {
        // The lead's transcript also carries `teamName`; step 2 must not
        // rewrite it into a sub-agent of itself.
        let team = team_with(Vec::new(), Some("LEAD"));
        let mut lead = hint("LEAD");
        lead.team_name = Some("T".to_string());
        let links = links_of(&[lead], &team, Vec::new(), &BTreeMap::new());
        assert_eq!(links["LEAD"].role, ROLE_LEAD);
    }

    #[test]
    fn the_task_description_is_the_spawn_prompt_of_last_resort() {
        let team = team_with(vec![member("w1", "w1", None)], Some("LEAD"));
        let mut sub = hint("SUB");
        sub.team_name = Some("T".to_string());
        sub.agent_id = Some("w1@T".to_string()); // suffixed — must be stripped
        let owned = TaskRecord {
            task_id: "1".to_string(),
            owner_name: Some("w1".to_string()),
            subject: None,
            description: Some("from the task".to_string()),
            status: None,
        };
        let links = links_of(&[sub.clone()], &team, vec![owned], &BTreeMap::new());
        assert_eq!(links["SUB"].spawn_prompt.as_deref(), Some("from the task"));

        // A single UNOWNED task is used even when nothing matches by name.
        let lone = TaskRecord {
            task_id: "1".to_string(),
            owner_name: None,
            subject: None,
            description: Some("the only task".to_string()),
            status: None,
        };
        let links = links_of(&[sub], &team, vec![lone], &BTreeMap::new());
        assert_eq!(links["SUB"].spawn_prompt.as_deref(), Some("the only task"));
    }

    #[test]
    fn the_worker_map_links_a_session_whose_team_config_is_gone() {
        let team = team_with(vec![member("w-a", "w-a", Some("the prompt"))], Some("LEAD"));
        let mut worker_map = BTreeMap::new();
        worker_map.insert("W".to_string(), ("w-a".to_string(), "T".to_string()));
        let links = links_of(&[hint("W")], &team, Vec::new(), &worker_map);
        assert_eq!(links["W"].role, ROLE_SUBAGENT);
        assert_eq!(links["W"].spawn_prompt.as_deref(), Some("the prompt"));
        assert_eq!(links["W"].parent_session_id.as_deref(), Some("LEAD"));
    }

    #[test]
    fn the_parent_uuid_chain_reaches_grandchildren() {
        // LEAD owns u1; CHILD is a sidechain whose parent is u1 and which owns
        // u2; GRAND is a sidechain whose parent is u2. One pass links CHILD, and
        // the fixpoint loop is what reaches GRAND.
        let team = team_with(Vec::new(), Some("LEAD"));
        let mut lead = hint("LEAD");
        lead.uuids = ["u1".to_string()].into_iter().collect();
        let mut child = hint("CHILD");
        child.has_sidechain = true;
        child.parent_uuids = ["u1".to_string()].into_iter().collect();
        child.uuids = ["u2".to_string()].into_iter().collect();
        let mut grand = hint("GRAND");
        grand.has_sidechain = true;
        grand.parent_uuids = ["u2".to_string()].into_iter().collect();

        let links = links_of(&[lead, child, grand], &team, Vec::new(), &BTreeMap::new());
        assert_eq!(links["CHILD"].parent_session_id.as_deref(), Some("LEAD"));
        assert_eq!(links["GRAND"].parent_session_id.as_deref(), Some("CHILD"));
        assert_eq!(links["GRAND"].team_id, "T");
        assert_eq!(links["GRAND"].spawn_prompt, None);
    }

    #[test]
    fn a_session_without_a_sidechain_never_joins_by_chain() {
        let team = team_with(Vec::new(), Some("LEAD"));
        let mut lead = hint("LEAD");
        lead.uuids = ["u1".to_string()].into_iter().collect();
        let mut other = hint("OTHER");
        other.parent_uuids = ["u1".to_string()].into_iter().collect();
        let links = links_of(&[lead, other], &team, Vec::new(), &BTreeMap::new());
        assert!(!links.contains_key("OTHER"));
    }

    // ── materialize_team_metadata ────────────────────────────────────────────

    fn store_with_session(root: &Path, session_id: &str) -> Connection {
        let conn = crate::ingest::testdb::store();
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
             VALUES ('claude', '-p', ?, 'p', 0.0, 0.0)",
            [root.to_string_lossy().as_ref()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) \
             VALUES (1, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)",
            [session_id],
        )
        .unwrap();
        conn
    }

    #[test]
    fn materialize_writes_the_team_row_and_the_four_session_columns() {
        let root = scratch("materialize");
        write(
            &root.join("projects/-p/LEAD.jsonl"),
            &format!(
                "{}\n{}\n",
                team_create_line("t1", "d", "2026-01-01T00:00:00Z"),
                agent_line("t1", "w", "claude", "go")
            ),
        );
        let conn = store_with_session(&root, "LEAD");

        let report = materialize_team_metadata(&conn, &root, "claude").unwrap();
        assert_eq!(report.teams_seen, 1);
        assert_eq!(report.teams_materialized, 1);
        assert_eq!(report.sessions_linked, 1);

        let (team_id, spawned_by, prompt, role): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT team_id, spawned_by_session_id, spawn_prompt, agent_role FROM sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(team_id.as_deref(), Some("t1"));
        assert_eq!(spawned_by, None);
        assert_eq!(prompt, None);
        assert_eq!(role.as_deref(), Some(ROLE_LEAD));

        let (stored_team, lead, project_id): (String, Option<String>, i64) = conn
            .query_row(
                "SELECT team_id, lead_session_id, project_id FROM agent_teams",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored_team, "t1");
        assert_eq!(lead.as_deref(), Some("LEAD"));
        assert_eq!(project_id, 1);

        // Idempotent: a second pass over the same filesystem changes nothing.
        let again = materialize_team_metadata(&conn, &root, "claude").unwrap();
        assert_eq!(again, report);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM agent_teams", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_team_whose_sessions_are_not_ingested_is_skipped_this_pass() {
        let root = scratch("notyet");
        write(
            &root.join("projects/-p/LEAD.jsonl"),
            &format!("{}\n", team_create_line("t1", "d", "2026-01-01T00:00:00Z")),
        );
        // The store knows a DIFFERENT session, so no candidate project resolves.
        let conn = store_with_session(&root, "SOMETHING-ELSE");
        let report = materialize_team_metadata(&conn, &root, "claude").unwrap();
        assert_eq!(report.teams_seen, 1);
        assert_eq!(report.teams_materialized, 0);
        assert_eq!(report.sessions_linked, 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM agent_teams", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_machine_with_no_teams_at_all_is_a_no_op() {
        let root = scratch("empty");
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let conn = store_with_session(&root, "S");
        let report = materialize_team_metadata(&conn, &root, "claude").unwrap();
        assert_eq!(report, MaterializeReport::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_config_team_overwrites_the_jsonl_team_of_the_same_name() {
        let root = scratch("merge");
        write(
            &root.join("projects/-p/LEAD.jsonl"),
            &format!(
                "{}\n",
                team_create_line("t1", "from jsonl", "2026-01-01T00:00:00Z")
            ),
        );
        write(
            &root.join("teams/t1/config.json"),
            r#"{"leadSessionId":"LEAD","description":"from config","createdAt":1700000000000,"members":[]}"#,
        );
        let conn = store_with_session(&root, "LEAD");
        materialize_team_metadata(&conn, &root, "claude").unwrap();
        let (description, config_json): (Option<String>, String) = conn
            .query_row(
                "SELECT description, config_json FROM agent_teams WHERE team_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(description.as_deref(), Some("from config"));
        assert!(!config_json.contains("jsonl_fallback"), "{config_json}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

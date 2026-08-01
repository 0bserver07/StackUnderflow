//! `services/agent_teams.py` (847 ln) — the Claude Code parallel-agent topology.
//!
//! | Python | Here |
//! |---|---|
//! | `list_team_sessions` | [`list_team_sessions`] |
//! | `build_team_graph` | [`build_team_graph`] |
//! | `get_agent_transcript` | [`get_agent_transcript`] |
//! | `team_summary_to_dict` / `agent_summary_to_dict` / `team_graph_to_dict` | the `to_dict` methods |
//!
//! # Three detection strategies, and the order is the contract
//!
//! `list_team_sessions` tries, in order:
//!
//! 1. **indexed (v013)** — `agent_teams` JOIN `sessions.team_id`. Taken only
//!    when [`indexed_teams_available`] AND `_indexed_teams_match_project` both
//!    say yes; the second gate exists so a store whose `agent_teams` table is
//!    populated *globally* but empty *for the asked project* falls through
//!    rather than answering `[]`.
//! 2. **sidechain scan** — `messages.is_sidechain = 1`, `teamName`/`agentId`
//!    parsed out of `raw_json`. Its result is tested for TRUTHINESS, so an
//!    empty list falls through to (3); a non-empty one wins.
//! 3. **task-tool scan** — `tools_json LIKE '%"Task"%'`, one row per parent
//!    session.
//!
//! `build_team_graph` has only two: indexed, then — on a `None` — the sidechain
//! heuristic. There is no task-tool graph.
//!
//! # What is load-bearing
//!
//! * **No wall clock anywhere.** Nothing in this module reads `time.time()` or
//!   `datetime.now()`; every timestamp on the wire is a stored `first_ts` /
//!   `last_ts`. That is why the whole surface can go byte-identical, unlike
//!   `/api/compare` (DIV-085).
//! * **`round(total, 4)` over a `+=` chain.** `_session_cost_usd` accumulates
//!   with `total += …` and is NOT `sum()`, so it must not be Neumaier-
//!   compensated (law 3). The rounding is `round_py`, the deduped owner.
//! * **`provider` is never passed to `compute_cost`,** so every session prices
//!   as `anthropic` regardless of which adapter ingested it. Reproduced; noted.
//! * **`text[:300]`** on the first user prompt is CODE POINTS
//!   ([`crate::pyops::char_prefix`]), not bytes.
//! * **`agent_role` / `team_name` / `spawned_by_session_id` are matched on
//!   Python TRUTHINESS,** not on `IS NOT NULL`: an empty-string `agent_role`
//!   falls back to the positional default exactly as `x or default` does.
//! * **The `messages` object is a VIEW** on this store (`messages_YYYYMM UNION
//!   ALL …`). Python guards nothing here — no `_table_exists` call appears in
//!   the module — so neither does this port. Law 7 is satisfied by *not*
//!   inventing a guard.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::stats::aggregator::round_py;

use crate::pyops::{char_prefix, sql_value};

/// `_ROLE_LEAD`.
const ROLE_LEAD: &str = "lead";
/// `_ROLE_SUBAGENT`.
const ROLE_SUBAGENT: &str = "subagent";

/// `_session_first_user_prompt`'s slice width, in code points.
const PROMPT_PREVIEW_CHARS: usize = 300;

/// The columns `get_agent_transcript` selects, in `SELECT`-list order.
///
/// The order is the payload's: `dict(sqlite3.Row)` is insertion-ordered by the
/// cursor's `description`, and the `is_sidechain` override that follows reuses
/// an existing key, so it does NOT move to the end.
const TRANSCRIPT_COLUMNS: [&str; 16] = [
    "id",
    "seq",
    "timestamp",
    "role",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_create_tokens",
    "cache_read_tokens",
    "content_text",
    "tools_json",
    "raw_json",
    "is_sidechain",
    "uuid",
    "parent_uuid",
    "speed",
];

// ── the three dataclasses ────────────────────────────────────────────────────

/// `@dataclass TeamSummary` — one row of the `GET /api/agent-teams` list view.
#[derive(Debug, Clone)]
pub struct TeamSummary {
    /// `session_id`.
    pub session_id: String,
    /// `project_slug`.
    pub project_slug: String,
    /// `project_display_name`.
    pub project_display_name: String,
    /// `team_name` — a `str` on both SQL paths and WHATEVER `raw_json` held on
    /// the heuristic one, so it is carried as a `Value`. `Null` is Python's
    /// `None`, which covers both "key absent" and "key present and null".
    pub team_name: Value,
    /// `first_ts`.
    pub first_ts: Option<String>,
    /// `last_ts`.
    pub last_ts: Option<String>,
    /// `agent_count`.
    pub agent_count: i64,
    /// `sub_agent_message_count`.
    pub sub_agent_message_count: i64,
    /// `lead_message_count`.
    pub lead_message_count: i64,
    /// `description`.
    pub description: Option<String>,
}

impl TeamSummary {
    /// `team_summary_to_dict` — `asdict(t)`, so the key order is the
    /// dataclass's FIELD DECLARATION order, `description` last.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "session_id".to_owned(),
            Value::from(self.session_id.clone()),
        );
        obj.insert(
            "project_slug".to_owned(),
            Value::from(self.project_slug.clone()),
        );
        obj.insert(
            "project_display_name".to_owned(),
            Value::from(self.project_display_name.clone()),
        );
        obj.insert("team_name".to_owned(), self.team_name.clone());
        obj.insert("first_ts".to_owned(), opt_str(self.first_ts.as_deref()));
        obj.insert("last_ts".to_owned(), opt_str(self.last_ts.as_deref()));
        obj.insert("agent_count".to_owned(), Value::from(self.agent_count));
        obj.insert(
            "sub_agent_message_count".to_owned(),
            Value::from(self.sub_agent_message_count),
        );
        obj.insert(
            "lead_message_count".to_owned(),
            Value::from(self.lead_message_count),
        );
        obj.insert(
            "description".to_owned(),
            opt_str(self.description.as_deref()),
        );
        Value::Object(obj)
    }
}

/// `@dataclass AgentSummary` — one agent (lead or spawned) inside a graph.
#[derive(Debug, Clone)]
pub struct AgentSummary {
    /// `session_id`.
    pub session_id: String,
    /// `agent_id`.
    pub agent_id: Option<String>,
    /// `agent_name` — every construction site produces a `str`, never `None`.
    pub agent_name: String,
    /// `is_lead`.
    pub is_lead: bool,
    /// `parent_session_id`.
    pub parent_session_id: Option<String>,
    /// `message_count`.
    pub message_count: i64,
    /// `first_ts`.
    pub first_ts: Option<String>,
    /// `last_ts`.
    pub last_ts: Option<String>,
    /// `first_user_prompt`.
    pub first_user_prompt: Option<String>,
    /// `model`.
    pub model: Option<String>,
    /// `cost_usd` — always a float, including the `round(0.0, 4)` zero case, so
    /// the wire byte is `0.0` and never `0`.
    pub cost_usd: f64,
    /// `spawn_prompt`.
    pub spawn_prompt: Option<String>,
    /// `agent_role` — `row["agent_role"] or <positional default>`, so never
    /// empty and never `None`.
    pub agent_role: String,
}

impl AgentSummary {
    /// `agent_summary_to_dict` — `asdict(a)`, field-declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "session_id".to_owned(),
            Value::from(self.session_id.clone()),
        );
        obj.insert("agent_id".to_owned(), opt_str(self.agent_id.as_deref()));
        obj.insert(
            "agent_name".to_owned(),
            Value::from(self.agent_name.clone()),
        );
        obj.insert("is_lead".to_owned(), Value::Bool(self.is_lead));
        obj.insert(
            "parent_session_id".to_owned(),
            opt_str(self.parent_session_id.as_deref()),
        );
        obj.insert("message_count".to_owned(), Value::from(self.message_count));
        obj.insert("first_ts".to_owned(), opt_str(self.first_ts.as_deref()));
        obj.insert("last_ts".to_owned(), opt_str(self.last_ts.as_deref()));
        obj.insert(
            "first_user_prompt".to_owned(),
            opt_str(self.first_user_prompt.as_deref()),
        );
        obj.insert("model".to_owned(), opt_str(self.model.as_deref()));
        obj.insert("cost_usd".to_owned(), Value::from(self.cost_usd));
        obj.insert(
            "spawn_prompt".to_owned(),
            opt_str(self.spawn_prompt.as_deref()),
        );
        obj.insert(
            "agent_role".to_owned(),
            Value::from(self.agent_role.clone()),
        );
        Value::Object(obj)
    }
}

/// `@dataclass TeamGraph` — lead first, agents in order.
#[derive(Debug, Clone)]
pub struct TeamGraph {
    /// `session_id`.
    pub session_id: String,
    /// `team_name`.
    pub team_name: Value,
    /// `project_slug`.
    pub project_slug: String,
    /// `project_display_name`.
    pub project_display_name: String,
    /// `lead`.
    pub lead: AgentSummary,
    /// `agents`.
    pub agents: Vec<AgentSummary>,
    /// `description`.
    pub description: Option<String>,
}

impl TeamGraph {
    /// `team_graph_to_dict` — a HAND-WRITTEN dict literal, **not** `asdict`, so
    /// its key order is the literal's and `description` sits THIRD rather than
    /// last. That asymmetry with [`TeamSummary::to_dict`] is the reference's.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "session_id".to_owned(),
            Value::from(self.session_id.clone()),
        );
        obj.insert("team_name".to_owned(), self.team_name.clone());
        obj.insert(
            "description".to_owned(),
            opt_str(self.description.as_deref()),
        );
        obj.insert(
            "project_slug".to_owned(),
            Value::from(self.project_slug.clone()),
        );
        obj.insert(
            "project_display_name".to_owned(),
            Value::from(self.project_display_name.clone()),
        );
        obj.insert("lead".to_owned(), self.lead.to_dict());
        obj.insert(
            "agents".to_owned(),
            Value::Array(self.agents.iter().map(AgentSummary::to_dict).collect()),
        );
        Value::Object(obj)
    }
}

/// `None` → JSON `null`.
fn opt_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

// ── private helpers ──────────────────────────────────────────────────────────

/// `_safe_json_loads` — a malformed or non-object blob is `{}`, never a raise.
fn safe_json_loads(blob: Option<&str>) -> Map<String, Value> {
    let Some(blob) = blob.filter(|blob| !blob.is_empty()) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(blob) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Python truthiness for a decoded JSON value.
///
/// `if candidate:` and `if lead_team_name and sub_team_name` are truthiness
/// tests, not `is not None` tests, so `""` / `0` / `false` / `[]` / `{}` all
/// behave as absent. Reproduced rather than narrowed to "is null".
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `str(candidate)` for the shapes a decoded `agentId` can take.
///
/// A `str` is itself, `True`/`False` are capitalised, and an int renders
/// without a decimal point. A container reaches `str()` as a Python `repr`
/// whose quoting rules are NOT reproduced here — no store has ever carried a
/// list-valued `agentId`, and inventing a repr would be law 6's guess. Those
/// render through the JSON writer instead, and the narrowing is recorded.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

/// `_extract_team_name` — `raw_json["teamName"]`, whatever type that is.
fn extract_team_name(raw_json: Option<&str>) -> Value {
    safe_json_loads(raw_json)
        .get("teamName")
        .cloned()
        .unwrap_or(Value::Null)
}

/// `_extract_agent_id` — `agentId` up to the first `@`, else the `agent-…`
/// filename convention, else `None`.
fn extract_agent_id(raw_json: Option<&str>, fallback_session_id: Option<&str>) -> Option<String> {
    let candidate = safe_json_loads(raw_json)
        .get("agentId")
        .cloned()
        .unwrap_or(Value::Null);
    if py_truthy(&candidate) {
        // `str(candidate).split("@", 1)[0]` — the part BEFORE the first `@`,
        // and the whole string when there is none.
        let rendered = py_str(&candidate);
        return Some(
            rendered
                .split_once('@')
                .map_or_else(|| rendered.clone(), |(head, _)| head.to_owned()),
        );
    }
    // `if fallback_session_id and fallback_session_id.startswith("agent-")`.
    let fallback = fallback_session_id.filter(|sid| !sid.is_empty())?;
    fallback
        .strip_prefix("agent-")
        .map(std::borrow::ToOwned::to_owned)
}

/// `_session_first_message_raw` — the first `raw_json` blob, in `seq` order.
fn session_first_message_raw(
    conn: &Connection,
    session_fk: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT raw_json FROM messages WHERE session_fk = ? ORDER BY seq LIMIT 1",
        [session_fk],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    // `row["raw_json"] if row else None` — a row whose blob is NULL is still a
    // row, and both spellings collapse to the same `None` here.
    .map(Option::flatten)
}

/// `_session_first_user_prompt` — the first non-empty user message, 300 code
/// points of it.
fn session_first_user_prompt(
    conn: &Connection,
    session_fk: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT content_text FROM messages \
         WHERE session_fk = ? AND role = 'user' \
           AND content_text IS NOT NULL AND content_text != '' \
         ORDER BY seq LIMIT 1",
        [session_fk],
        |row| {
            // `text[:300] if isinstance(text, str) else None` — a cell whose
            // STORAGE CLASS is not TEXT is `None`, even though it passed the
            // `!= ''` filter (SQLite orders every INTEGER below every TEXT).
            Ok(match row.get_ref(0)? {
                ValueRef::Text(bytes) => Some(char_prefix(
                    &String::from_utf8_lossy(bytes),
                    PROMPT_PREVIEW_CHARS,
                )),
                _ => None,
            })
        },
    )
    .optional()
    .map(Option::flatten)
}

/// One `(model, speed)` group from `_session_token_totals`.
struct TokenTotals {
    model: String,
    speed: String,
    input: i64,
    output: i64,
    cache_create: i64,
    cache_read: i64,
}

/// `_session_token_totals` — per-`(model, speed)` sums, `<synthetic>` excluded.
fn session_token_totals(conn: &Connection, session_fk: i64) -> rusqlite::Result<Vec<TokenTotals>> {
    let mut stmt = conn.prepare_cached(
        "SELECT COALESCE(model, '') AS model, \
                COALESCE(speed, 'standard') AS speed, \
                SUM(input_tokens) AS input, \
                SUM(output_tokens) AS output, \
                SUM(cache_create_tokens) AS cache_create, \
                SUM(cache_read_tokens) AS cache_read \
         FROM messages \
         WHERE session_fk = ? AND model IS NOT NULL AND model != '' \
           AND model != '<synthetic>' \
         GROUP BY model, speed",
    )?;
    let rows = stmt
        .query_map([session_fk], |row| {
            Ok(TokenTotals {
                model: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                speed: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                // `int(r["input"] or 0)` — a NULL SUM is 0.
                input: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                output: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cache_create: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                cache_read: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `_session_dominant_model` — the model on the most assistant messages.
fn session_dominant_model(conn: &Connection, session_fk: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT model, COUNT(*) AS c FROM messages \
         WHERE session_fk = ? AND role = 'assistant' \
           AND model IS NOT NULL AND model != '' AND model != '<synthetic>' \
         GROUP BY model ORDER BY c DESC LIMIT 1",
        [session_fk],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
}

/// `_session_cost_usd` — `round(total, 4)` over a `+=` chain.
///
/// **Law 2**: the engine is `crate::pricing::engine`'s, which carries the
/// store's `price_book` overlay the running reference primes at startup.
///
/// **Law 3**: `total += float(...)` is a plain accumulation. Python's `sum()`
/// is Neumaier-compensated and this is NOT `sum()`, so compensating here would
/// be a divergence dressed as an improvement.
fn session_cost_usd(
    conn: &Connection,
    engine: &PricingEngine,
    session_fk: i64,
) -> rusqlite::Result<f64> {
    let mut total = 0.0_f64;
    for row in session_token_totals(conn, session_fk)? {
        // `if not r["model"]: continue` — unreachable behind the SQL filter,
        // ported because removing it is a judgement call about the SQL.
        if row.model.is_empty() {
            continue;
        }
        // `provider` is NOT passed, so `compute_cost`'s default wins: every
        // session prices as anthropic whatever adapter produced it.
        let cost = engine.compute_cost(
            &RawTokens::canonical(row.input, row.output, row.cache_create, row.cache_read),
            &row.model,
            "anthropic",
            // `speed=r["speed"] or "standard"` — the COALESCE already handled
            // NULL, this handles the empty string.
            if row.speed.is_empty() {
                "standard"
            } else {
                &row.speed
            },
            None,
        );
        total += cost.total_cost;
    }
    Ok(round_py(total, 4))
}

/// `_session_message_count`.
fn session_message_count(conn: &Connection, session_fk: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) AS c FROM messages WHERE session_fk = ?",
        [session_fk],
        |row| row.get::<_, i64>(0),
    )
}

/// `_indexed_teams_available` — migration v013 ran *and* something is
/// materialised.
///
/// Python wraps the probe in `except sqlite3.OperationalError: return False`,
/// which swallows a missing COLUMN (pre-v013) and a missing TABLE alike. Any
/// SQLite error is `false` here for the same reason — this is a capability
/// probe, not a query.
#[must_use]
pub fn indexed_teams_available(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sessions WHERE team_id IS NOT NULL LIMIT 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .is_ok_and(|row| row.is_some())
}

/// `_indexed_teams_match_project` — does the indexed path have rows for the
/// asked project?
///
/// `if project_slug is None: return True` is an **identity** test, not a
/// truthiness test, so `?project=` (the empty string) does NOT short-circuit:
/// it runs the JOIN with `slug = ''`, finds nothing, and forces the fall-through
/// to the heuristic paths — which then treat `""` as "no filter" because THEIR
/// guards are `if project_slug:`. One request, two readings of one empty
/// string. Reproduced; see the ledger.
fn indexed_teams_match_project(
    conn: &Connection,
    project_slug: Option<&str>,
) -> rusqlite::Result<bool> {
    let Some(slug) = project_slug else {
        return Ok(true);
    };
    Ok(conn
        .query_row(
            "SELECT 1 FROM agent_teams t \
             JOIN projects p ON p.id = t.project_id \
             WHERE p.slug = ? LIMIT 1",
            [slug],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

/// `_agent_summary_for_session` — the four per-session probes, bundled.
#[allow(clippy::too_many_arguments, reason = "one argument per Python keyword")]
fn agent_summary_for_session(
    conn: &Connection,
    engine: &PricingEngine,
    session_fk: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
    is_lead: bool,
    parent_session_id: Option<String>,
    agent_id: Option<String>,
    agent_name: String,
    spawn_prompt: Option<String>,
    agent_role: String,
) -> rusqlite::Result<AgentSummary> {
    Ok(AgentSummary {
        session_id,
        agent_id,
        agent_name,
        is_lead,
        parent_session_id,
        message_count: session_message_count(conn, session_fk)?,
        first_ts,
        last_ts,
        first_user_prompt: session_first_user_prompt(conn, session_fk)?,
        model: session_dominant_model(conn, session_fk)?,
        cost_usd: session_cost_usd(conn, engine, session_fk)?,
        spawn_prompt,
        agent_role,
    })
}

// ── public API: list_team_sessions ───────────────────────────────────────────

/// `list_team_sessions` — the three strategies, in order.
///
/// `project_slug` is `None` for an absent `?project=` and `Some("")` for a
/// present-but-empty one; the two are NOT the same request here.
///
/// # Errors
/// Any SQLite error other than the ones the capability probes swallow.
pub fn list_team_sessions(
    conn: &Connection,
    limit: i64,
    project_slug: Option<&str>,
) -> rusqlite::Result<Vec<TeamSummary>> {
    if indexed_teams_available(conn) && indexed_teams_match_project(conn, project_slug)? {
        return list_team_sessions_indexed(conn, limit, project_slug);
    }
    let sidechain_results = list_team_sessions_scan(conn, limit, project_slug)?;
    // `if sidechain_results:` — a TRUTHINESS test on the list, so an empty
    // result falls through to the task-tool path instead of being the answer.
    if !sidechain_results.is_empty() {
        return Ok(sidechain_results);
    }
    list_team_sessions_task_tool(conn, limit, project_slug)
}

/// `_list_team_sessions_indexed`.
fn list_team_sessions_indexed(
    conn: &Connection,
    limit: i64,
    project_slug: Option<&str>,
) -> rusqlite::Result<Vec<TeamSummary>> {
    // `if project_slug:` — truthiness, so `""` adds no WHERE clause even though
    // it is what forced this function to be skipped a moment ago.
    let filtered = project_slug.is_some_and(|slug| !slug.is_empty());
    let sql = format!(
        "SELECT \
           t.team_id, \
           t.description, \
           t.lead_session_id, \
           p.slug          AS project_slug, \
           p.display_name  AS project_display_name, \
           MIN(s.first_ts) AS first_ts, \
           MAX(s.last_ts)  AS last_ts, \
           SUM(CASE WHEN COALESCE(s.agent_role, '') = 'subagent' THEN 1 ELSE 0 END) AS agent_count, \
           SUM(CASE WHEN COALESCE(s.agent_role, '') = 'subagent' THEN s.message_count ELSE 0 END) AS sub_msgs, \
           SUM(CASE WHEN s.session_id = t.lead_session_id THEN s.message_count ELSE 0 END) AS lead_msgs \
         FROM agent_teams t \
         JOIN projects p ON p.id = t.project_id \
         JOIN sessions s ON s.team_id = t.team_id \
         {} \
         GROUP BY t.team_id, t.description, t.lead_session_id, p.slug, p.display_name \
         ORDER BY MAX(s.last_ts) DESC, t.team_id ASC \
         LIMIT ?",
        if filtered { "WHERE p.slug = ?" } else { "" }
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if filtered {
        params.push(rusqlite::types::Value::Text(
            project_slug.unwrap_or_default().to_owned(),
        ));
    }
    params.push(rusqlite::types::Value::Integer(limit));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let team_id: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
            let lead_session_id: Option<String> = row.get(2)?;
            Ok(TeamSummary {
                // `r["lead_session_id"] or r["team_id"]` — truthiness, so an
                // empty string falls back to the team id just as NULL does.
                session_id: lead_session_id
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| team_id.clone()),
                project_slug: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                project_display_name: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                team_name: Value::from(team_id),
                first_ts: row.get(5)?,
                last_ts: row.get(6)?,
                agent_count: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                sub_agent_message_count: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                lead_message_count: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                description: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `_list_team_sessions_task_tool` — `tools_json LIKE '%"Task"%'`.
fn list_team_sessions_task_tool(
    conn: &Connection,
    limit: i64,
    project_slug: Option<&str>,
) -> rusqlite::Result<Vec<TeamSummary>> {
    let filtered = project_slug.is_some_and(|slug| !slug.is_empty());
    let sql = format!(
        "SELECT \
           s.id            AS session_fk, \
           s.session_id, \
           s.first_ts, \
           s.last_ts, \
           s.message_count AS lead_msgs, \
           p.slug          AS project_slug, \
           p.display_name  AS project_display_name, \
           SUM(CASE \
             WHEN m.tools_json LIKE '%\"Task\"%' OR m.tools_json LIKE '%\"Agent\"%' \
             THEN 1 ELSE 0 END) AS subagent_call_count \
         FROM sessions s \
         JOIN projects p ON p.id = s.project_id \
         JOIN messages m ON m.session_fk = s.id \
         WHERE 1=1 {} \
         GROUP BY s.id, s.session_id, s.first_ts, s.last_ts, \
                  s.message_count, p.slug, p.display_name \
         HAVING subagent_call_count > 0 \
         ORDER BY subagent_call_count DESC, s.last_ts DESC \
         LIMIT ?",
        if filtered { "AND p.slug = ?" } else { "" }
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if filtered {
        params.push(rusqlite::types::Value::Text(
            project_slug.unwrap_or_default().to_owned(),
        ));
    }
    params.push(rusqlite::types::Value::Integer(limit));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let calls = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
            Ok(TeamSummary {
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                project_slug: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                project_display_name: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                team_name: Value::Null,
                first_ts: row.get(2)?,
                last_ts: row.get(3)?,
                agent_count: calls,
                sub_agent_message_count: 0,
                lead_message_count: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                // The f-string interpolates `int(r['subagent_call_count'])` —
                // the UNGUARDED int, not the `or 0` one two lines above. Same
                // value on every reachable row (the HAVING keeps it positive).
                description: Some(format!(
                    "{calls} Task/Agent sub-agent invocations (inline within parent session)"
                )),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One candidate row of the sidechain scan.
struct ScanCandidate {
    session_fk: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
    project_id: i64,
    project_slug: String,
    project_display_name: String,
    lead_msgs: i64,
}

/// `_list_team_sessions_scan` — the pre-v013 heuristic.
fn list_team_sessions_scan(
    conn: &Connection,
    limit: i64,
    project_slug: Option<&str>,
) -> rusqlite::Result<Vec<TeamSummary>> {
    let has_sidechain = conn
        .query_row(
            "SELECT 1 FROM messages WHERE is_sidechain = 1 LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if !has_sidechain {
        return Ok(Vec::new());
    }

    let filtered = project_slug.is_some_and(|slug| !slug.is_empty());
    let sql = format!(
        "SELECT \
           s.id AS session_fk, \
           s.session_id, \
           s.first_ts, \
           s.last_ts, \
           s.project_id, \
           p.slug AS project_slug, \
           p.display_name AS project_display_name, \
           COALESCE(SUM(CASE WHEN m.is_sidechain = 0 THEN 1 ELSE 0 END), 0) AS lead_msgs, \
           COALESCE(SUM(CASE WHEN m.is_sidechain = 1 THEN 1 ELSE 0 END), 0) AS own_sub_msgs \
         FROM sessions s \
         JOIN projects p ON p.id = s.project_id \
         JOIN messages m ON m.session_fk = s.id \
         WHERE s.project_id IN ( \
           SELECT DISTINCT s2.project_id \
           FROM sessions s2 \
           JOIN messages m2 ON m2.session_fk = s2.id \
           WHERE m2.is_sidechain = 1 \
         ) \
         {} \
         GROUP BY s.id, s.session_id, s.first_ts, s.last_ts, \
                  s.project_id, p.slug, p.display_name \
         HAVING lead_msgs > 0 \
         ORDER BY s.last_ts DESC",
        if filtered { "AND p.slug = ?" } else { "" }
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if filtered {
        params.push(rusqlite::types::Value::Text(
            project_slug.unwrap_or_default().to_owned(),
        ));
    }
    let mut stmt = conn.prepare(&sql)?;
    let candidates: Vec<ScanCandidate> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(ScanCandidate {
                session_fk: row.get(0)?,
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                first_ts: row.get(2)?,
                last_ts: row.get(3)?,
                project_id: row.get(4)?,
                project_slug: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                project_display_name: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                lead_msgs: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    // NOTE the asymmetry: this second query carries NO project filter. A
    // project-scoped request still pays for the whole store's sidechain rollup,
    // and the lookup below throws all of it away but the one project.
    // Inherited; it is also why the scan is the slow path.
    let mut sub_stmt = conn.prepare(
        "SELECT s.project_id, \
                s.id AS session_fk, \
                COUNT(*) AS sub_msgs \
         FROM sessions s \
         JOIN messages m ON m.session_fk = s.id \
         WHERE m.is_sidechain = 1 \
         GROUP BY s.project_id, s.id",
    )?;
    // `setdefault(...).append(...)` on a plain dict — insertion-ordered, and
    // the per-project list order is the query's. Only membership and a SUM are
    // read out of it, so a `Vec` of pairs is the same answer.
    let mut sub_by_project: Vec<(i64, Vec<(i64, i64)>)> = Vec::new();
    let mut sub_rows = sub_stmt.query([])?;
    while let Some(row) = sub_rows.next()? {
        let pid: i64 = row.get(0)?;
        let sfk: i64 = row.get(1)?;
        let count: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        match sub_by_project.iter_mut().find(|(seen, _)| *seen == pid) {
            Some((_, bucket)) => bucket.push((sfk, count)),
            None => sub_by_project.push((pid, vec![(sfk, count)])),
        }
    }

    let mut out: Vec<TeamSummary> = Vec::new();
    let mut seen_session_ids: Vec<String> = Vec::new();
    let empty: Vec<(i64, i64)> = Vec::new();

    for candidate in candidates {
        // The break is at the TOP of the body, so `limit` bounds the OUTPUT and
        // the loop stops the moment it is reached — no work is done for the
        // candidate that would have been number `limit + 1`.
        if i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
        let project_subs = sub_by_project
            .iter()
            .find(|(pid, _)| *pid == candidate.project_id)
            .map_or(&empty, |(_, bucket)| bucket);
        let other_subs: Vec<(i64, i64)> = project_subs
            .iter()
            .copied()
            .filter(|(sfk, _)| *sfk != candidate.session_fk)
            .collect();
        if other_subs.is_empty() {
            continue;
        }
        if seen_session_ids.contains(&candidate.session_id) {
            continue;
        }
        seen_session_ids.push(candidate.session_id.clone());

        let team_name =
            extract_team_name(session_first_message_raw(conn, candidate.session_fk)?.as_deref());

        // A `set` of agent ids — only its LENGTH reaches the payload, so a
        // dedup-preserving vec is the same answer at a deterministic cost.
        let mut agent_ids: Vec<String> = Vec::new();
        for (sfk, _) in &other_subs {
            let mut raw_stmt = conn.prepare_cached(
                "SELECT raw_json FROM messages WHERE session_fk = ? AND is_sidechain = 1",
            )?;
            let blobs: Vec<Option<String>> = raw_stmt
                .query_map([sfk], |row| row.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<_>>()?;
            for blob in blobs {
                // `_extract_agent_id(ar["raw_json"])` — NO fallback id here,
                // unlike every other call site in the module.
                if let Some(aid) = extract_agent_id(blob.as_deref(), None)
                    && !agent_ids.contains(&aid)
                {
                    agent_ids.push(aid);
                }
            }
        }
        let agent_count = if agent_ids.is_empty() {
            i64::try_from(other_subs.len()).unwrap_or(i64::MAX)
        } else {
            i64::try_from(agent_ids.len()).unwrap_or(i64::MAX)
        };
        // `sum(c for _, c in other_subs)` over ints — exact integer addition,
        // not a float accumulation, so law 3's compensation question does not
        // arise.
        let sub_msg_total: i64 = other_subs.iter().map(|(_, count)| *count).sum();

        out.push(TeamSummary {
            session_id: candidate.session_id,
            project_slug: candidate.project_slug,
            project_display_name: candidate.project_display_name,
            team_name,
            first_ts: candidate.first_ts,
            last_ts: candidate.last_ts,
            agent_count,
            sub_agent_message_count: sub_msg_total,
            lead_message_count: candidate.lead_msgs,
            description: None,
        });
    }
    Ok(out)
}

// ── public API: build_team_graph ─────────────────────────────────────────────

/// `build_team_graph` — indexed first, heuristic on a miss.
///
/// # Errors
/// Any SQLite error.
pub fn build_team_graph(
    conn: &Connection,
    engine: &PricingEngine,
    lead_session_id: &str,
) -> rusqlite::Result<Option<TeamGraph>> {
    if indexed_teams_available(conn) {
        let graph = build_team_graph_indexed(conn, engine, lead_session_id)?;
        if graph.is_some() {
            return Ok(graph);
        }
        // Fall through — the session may belong to a team ingested before v013.
    }
    build_team_graph_scan(conn, engine, lead_session_id)
}

/// The `agent_teams` JOIN `projects` row both lookups return.
struct TeamRow {
    team_id: String,
    description: Option<String>,
    lead_session_id: Option<String>,
    slug: String,
    display_name: String,
}

/// The two `SELECT`s `_build_team_graph_indexed` issues differ only in their
/// `WHERE` column, and Python spells both out. `column` is one of two literals
/// chosen here, never user input.
fn team_row_by(conn: &Connection, column: &str, value: &str) -> rusqlite::Result<Option<TeamRow>> {
    let sql = format!(
        "SELECT t.team_id, t.description, t.lead_session_id, t.project_id, \
                p.slug, p.display_name \
         FROM agent_teams t JOIN projects p ON p.id = t.project_id \
         WHERE t.{column} = ?"
    );
    conn.query_row(&sql, [value], |row| {
        Ok(TeamRow {
            team_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            description: row.get(1)?,
            lead_session_id: row.get(2)?,
            slug: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            display_name: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        })
    })
    .optional()
}

/// One `sessions` row of the materialised team.
struct MemberRow {
    id: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
    spawn_prompt: Option<String>,
    agent_role: Option<String>,
    spawned_by_session_id: Option<String>,
}

/// `_build_team_graph_indexed`.
fn build_team_graph_indexed(
    conn: &Connection,
    engine: &PricingEngine,
    lead_session_id: &str,
) -> rusqlite::Result<Option<TeamGraph>> {
    let team_row = match team_row_by(conn, "lead_session_id", lead_session_id)? {
        Some(row) => row,
        None => {
            // Not a known lead — is it a MEMBER carrying a team_id?
            let member: Option<String> = conn
                .query_row(
                    "SELECT team_id FROM sessions \
                     WHERE session_id = ? AND team_id IS NOT NULL LIMIT 1",
                    [lead_session_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let Some(team_id) = member else {
                return Ok(None);
            };
            match team_row_by(conn, "team_id", &team_id)? {
                Some(row) => row,
                None => return Ok(None),
            }
        }
    };

    let team_id = team_row.team_id.clone();
    let lead_session = team_row.lead_session_id.clone();

    let mut stmt = conn.prepare(
        "SELECT s.id, s.session_id, s.first_ts, s.last_ts, \
                s.spawn_prompt, s.agent_role, s.spawned_by_session_id \
         FROM sessions s WHERE s.team_id = ? \
         ORDER BY (CASE WHEN s.agent_role = 'lead' THEN 0 ELSE 1 END), \
                  s.first_ts ASC, s.session_id ASC",
    )?;
    let members: Vec<MemberRow> = stmt
        .query_map([&team_id], |row| {
            Ok(MemberRow {
                id: row.get(0)?,
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                first_ts: row.get(2)?,
                last_ts: row.get(3)?,
                spawn_prompt: row.get(4)?,
                agent_role: row.get(5)?,
                spawned_by_session_id: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    if members.is_empty() {
        return Ok(None);
    }

    let mut lead_summary: Option<AgentSummary> = None;
    let mut agents: Vec<AgentSummary> = Vec::new();
    for row in members {
        let is_lead = row.agent_role.as_deref() == Some(ROLE_LEAD)
            || Some(&row.session_id) == lead_session.as_ref();
        // Queried for EVERY member, the lead included — Python does not gate
        // this on `is_lead`, it just discards the answer there.
        let first_raw = session_first_message_raw(conn, row.id)?;
        let agent_id = if is_lead {
            None
        } else {
            extract_agent_id(first_raw.as_deref(), Some(&row.session_id))
        };
        let agent_name = if is_lead {
            "team-lead".to_owned()
        } else {
            agent_id.clone().unwrap_or_else(|| row.session_id.clone())
        };
        let parent_sid = if is_lead {
            None
        } else {
            row.spawned_by_session_id
                .clone()
                .filter(|value| !value.is_empty())
                .or_else(|| lead_session.clone())
        };
        let agent_role = row
            .agent_role
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                if is_lead {
                    ROLE_LEAD.to_owned()
                } else {
                    ROLE_SUBAGENT.to_owned()
                }
            });
        let summary = agent_summary_for_session(
            conn,
            engine,
            row.id,
            row.session_id.clone(),
            row.first_ts,
            row.last_ts,
            is_lead,
            parent_sid,
            agent_id,
            agent_name,
            row.spawn_prompt,
            agent_role,
        )?;
        // A SECOND lead-shaped row lands in `agents`, not in `lead` — the
        // `and lead_summary is None` guard is on the FIRST branch only.
        if is_lead && lead_summary.is_none() {
            lead_summary = Some(summary);
        } else {
            agents.push(summary);
        }
    }

    let lead_summary = lead_summary.unwrap_or_else(|| AgentSummary {
        // The lead transcript is not ingested — synthesise a placeholder so the
        // sub-agents still render. `lead_session or team_id` is a truthiness
        // fallback, so an empty lead id takes the team id too.
        session_id: lead_session
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| team_id.clone()),
        agent_id: None,
        agent_name: "team-lead".to_owned(),
        is_lead: true,
        parent_session_id: None,
        message_count: 0,
        first_ts: None,
        last_ts: None,
        first_user_prompt: None,
        model: None,
        cost_usd: 0.0,
        spawn_prompt: None,
        agent_role: ROLE_LEAD.to_owned(),
    });

    Ok(Some(TeamGraph {
        session_id: lead_summary.session_id.clone(),
        team_name: Value::from(team_id),
        project_slug: team_row.slug,
        project_display_name: team_row.display_name,
        lead: lead_summary,
        agents,
        description: team_row.description,
    }))
}

/// The lead `sessions` JOIN `projects` row of the heuristic graph.
struct LeadRow {
    id: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
    project_id: i64,
    slug: String,
    display_name: String,
}

/// One sub-agent candidate of the heuristic graph.
struct CandidateRow {
    id: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

/// `_build_team_graph_scan` — the `is_sidechain` + `teamName` heuristic.
fn build_team_graph_scan(
    conn: &Connection,
    engine: &PricingEngine,
    lead_session_id: &str,
) -> rusqlite::Result<Option<TeamGraph>> {
    let lead_row: Option<LeadRow> = conn
        .query_row(
            "SELECT s.id, s.session_id, s.first_ts, s.last_ts, \
                    p.id AS project_id, p.slug, p.display_name \
             FROM sessions s JOIN projects p ON p.id = s.project_id \
             WHERE s.session_id = ?",
            [lead_session_id],
            |row| {
                Ok(LeadRow {
                    id: row.get(0)?,
                    session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    first_ts: row.get(2)?,
                    last_ts: row.get(3)?,
                    project_id: row.get(4)?,
                    slug: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    display_name: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                })
            },
        )
        .optional()?;
    let Some(lead_row) = lead_row else {
        return Ok(None);
    };

    let lead_team_name =
        extract_team_name(session_first_message_raw(conn, lead_row.id)?.as_deref());

    let lead_summary = agent_summary_for_session(
        conn,
        engine,
        lead_row.id,
        lead_row.session_id.clone(),
        lead_row.first_ts.clone(),
        lead_row.last_ts.clone(),
        true,
        None,
        None,
        "team-lead".to_owned(),
        None,
        ROLE_LEAD.to_owned(),
    )?;

    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.id, s.session_id, s.first_ts, s.last_ts \
         FROM sessions s \
         JOIN messages m ON m.session_fk = s.id \
         WHERE s.project_id = ? AND s.id != ? AND m.is_sidechain = 1 \
         ORDER BY s.first_ts ASC",
    )?;
    let candidates: Vec<CandidateRow> = stmt
        .query_map(rusqlite::params![lead_row.project_id, lead_row.id], |row| {
            Ok(CandidateRow {
                id: row.get(0)?,
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                first_ts: row.get(2)?,
                last_ts: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut agents: Vec<AgentSummary> = Vec::new();
    for candidate in candidates {
        let first_raw = session_first_message_raw(conn, candidate.id)?;
        let sub_team_name = extract_team_name(first_raw.as_deref());
        // `if lead and sub and lead != sub: continue` — a sub-agent with NO
        // team name is kept under any lead, and every sub-agent is kept when
        // the lead itself has none.
        if py_truthy(&lead_team_name)
            && py_truthy(&sub_team_name)
            && lead_team_name != sub_team_name
        {
            continue;
        }
        let agent_id = extract_agent_id(first_raw.as_deref(), Some(&candidate.session_id));
        let agent_name = agent_id
            .clone()
            .unwrap_or_else(|| candidate.session_id.clone());
        agents.push(agent_summary_for_session(
            conn,
            engine,
            candidate.id,
            candidate.session_id.clone(),
            candidate.first_ts,
            candidate.last_ts,
            false,
            Some(lead_row.session_id.clone()),
            agent_id,
            agent_name,
            None,
            ROLE_SUBAGENT.to_owned(),
        )?);
    }

    Ok(Some(TeamGraph {
        session_id: lead_row.session_id,
        team_name: lead_team_name,
        project_slug: lead_row.slug,
        project_display_name: lead_row.display_name,
        lead: lead_summary,
        agents,
        // `TeamGraph(...)` without `description=` — the dataclass default.
        description: None,
    }))
}

// ── public API: get_agent_transcript ─────────────────────────────────────────

/// `get_agent_transcript` — one agent's rows, fenced to the lead's project.
///
/// The fence is a self-join on `project_id`, so it answers `None` when *either*
/// session is missing AND when both exist in different projects. It does not
/// check team membership at all: any two sessions in one project pass, the lead
/// paired with itself included.
///
/// # Errors
/// Any SQLite error.
pub fn get_agent_transcript(
    conn: &Connection,
    lead_session_id: &str,
    agent_session_id: &str,
) -> rusqlite::Result<Option<Vec<Value>>> {
    let agent_fk: Option<i64> = conn
        .query_row(
            "SELECT s1.id AS lead_fk, s2.id AS agent_fk, s1.project_id \
             FROM sessions s1 JOIN sessions s2 \
               ON s2.project_id = s1.project_id \
             WHERE s1.session_id = ? AND s2.session_id = ?",
            [lead_session_id, agent_session_id],
            |row| row.get::<_, i64>(1),
        )
        .optional()?;
    let Some(agent_fk) = agent_fk else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT id, seq, timestamp, role, model, \
                input_tokens, output_tokens, \
                cache_create_tokens, cache_read_tokens, \
                content_text, tools_json, raw_json, \
                is_sidechain, uuid, parent_uuid, speed \
         FROM messages WHERE session_fk = ? ORDER BY seq",
    )?;
    let rows = stmt
        .query_map([agent_fk], |row| {
            let mut obj = Map::new();
            for (index, name) in TRANSCRIPT_COLUMNS.iter().enumerate() {
                // `dict(r)` — the STORAGE class, with no declared-type coercion.
                obj.insert((*name).to_owned(), sql_value(row, index)?);
            }
            // `{**dict(r), "is_sidechain": bool(...)}` — the key already exists,
            // so the override rewrites IN PLACE and does not move to the end.
            let flag = py_truthy(obj.get("is_sidechain").unwrap_or(&Value::Null));
            obj.insert("is_sidechain".to_owned(), Value::Bool(flag));
            Ok(Value::Object(obj))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EMPTY rate card. Every assertion below is about shape, ordering and
    /// the `0.0`-vs-`0` float byte, none about a price, so an engine with no
    /// manifest keeps the fixtures from depending on `models.toml`'s contents.
    /// The real engine (`crate::pricing::engine`, LAW 2) is wired in the route.
    fn engine() -> PricingEngine {
        PricingEngine::from_manifest(stax_etl::pricing::Manifest::default())
    }

    /// A store with the v013 schema and the partitioned `messages` VIEW, which
    /// is what the live store actually presents (`type='view'`, DIV-148).
    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
                 path TEXT, display_name TEXT NOT NULL,
                 first_seen REAL NOT NULL, last_modified REAL NOT NULL,
                 UNIQUE (provider, slug));
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
                 message_count INTEGER NOT NULL DEFAULT 0,
                 team_id TEXT, spawned_by_session_id TEXT,
                 spawn_prompt TEXT, agent_role TEXT,
                 UNIQUE (project_id, session_id));
             CREATE TABLE messages_202607 (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER,
                 timestamp TEXT, role TEXT, model TEXT,
                 input_tokens INTEGER, output_tokens INTEGER,
                 cache_create_tokens INTEGER, cache_read_tokens INTEGER,
                 content_text TEXT, tools_json TEXT, raw_json TEXT,
                 is_sidechain INTEGER, uuid TEXT, parent_uuid TEXT, speed TEXT);
             CREATE VIEW messages AS SELECT id, session_fk, seq, timestamp, role,
                 model, input_tokens, output_tokens, cache_create_tokens,
                 cache_read_tokens, content_text, tools_json, raw_json,
                 is_sidechain, uuid, parent_uuid, speed FROM messages_202607;
             CREATE TABLE agent_teams (
                 team_id TEXT PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 created_ts TEXT NOT NULL, description TEXT,
                 lead_session_id TEXT, config_json TEXT NOT NULL);
             INSERT INTO projects VALUES (1, 'claude', 'alpha', NULL, 'Alpha', 0.0, 0.0),
                                         (2, 'claude', 'beta',  NULL, 'Beta',  0.0, 0.0);",
        )
        .expect("schema");
        conn
    }

    fn add_session(conn: &Connection, id: i64, project: i64, sid: &str, count: i64) {
        conn.execute(
            "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) \
             VALUES (?, ?, ?, '2026-07-01T00:00:00Z', '2026-07-02T00:00:00Z', ?)",
            rusqlite::params![id, project, sid, count],
        )
        .expect("session");
    }

    fn add_message(conn: &Connection, id: i64, fk: i64, seq: i64, role: &str, raw: Option<&str>) {
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, content_text, \
                                          raw_json, is_sidechain) \
             VALUES (?, ?, ?, '2026-07-01T00:00:00Z', ?, 'hello', ?, 0)",
            rusqlite::params![id, fk, seq, role, raw],
        )
        .expect("message");
    }

    fn add_sidechain(conn: &Connection, id: i64, fk: i64, raw: &str) {
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, raw_json, \
                                          is_sidechain) \
             VALUES (?, ?, 0, 't', 'user', ?, 1)",
            rusqlite::params![id, fk, raw],
        )
        .expect("sidechain");
    }

    // ── the helpers ──────────────────────────────────────────────────────────

    #[test]
    fn an_agent_id_is_cut_at_the_first_at_sign_and_falls_back_to_the_filename() {
        assert_eq!(
            extract_agent_id(Some(r#"{"agentId":"abc@host@2"}"#), None).as_deref(),
            Some("abc")
        );
        assert_eq!(
            extract_agent_id(Some(r#"{"agentId":"plain"}"#), None).as_deref(),
            Some("plain")
        );
        // `if candidate:` is truthiness — an empty id falls to the fallback.
        assert_eq!(
            extract_agent_id(Some(r#"{"agentId":""}"#), Some("agent-9f2")).as_deref(),
            Some("9f2")
        );
        // …and a session id that is not `agent-`-prefixed yields None.
        assert_eq!(extract_agent_id(Some("{}"), Some("44b8f238")), None);
        assert_eq!(extract_agent_id(None, None), None);
        // Malformed JSON is `{}`, never a raise.
        assert_eq!(extract_agent_id(Some("{not json"), None), None);
        // `str(candidate)` for a non-string id.
        assert_eq!(
            extract_agent_id(Some(r#"{"agentId":7}"#), None).as_deref(),
            Some("7")
        );
    }

    #[test]
    fn a_team_name_is_whatever_the_blob_held_and_absent_is_null() {
        assert_eq!(
            extract_team_name(Some(r#"{"teamName":"crew"}"#)),
            Value::from("crew")
        );
        assert_eq!(extract_team_name(Some(r#"{"teamName":null}"#)), Value::Null);
        assert_eq!(extract_team_name(Some("{}")), Value::Null);
        assert_eq!(extract_team_name(Some("[1,2]")), Value::Null);
        assert_eq!(extract_team_name(None), Value::Null);
    }

    #[test]
    fn python_truthiness_not_is_none() {
        assert!(!py_truthy(&Value::Null));
        assert!(!py_truthy(&Value::from("")));
        assert!(!py_truthy(&Value::from(0)));
        assert!(!py_truthy(&Value::from(0.0)));
        assert!(!py_truthy(&Value::Bool(false)));
        assert!(!py_truthy(&Value::Array(vec![])));
        assert!(py_truthy(&Value::from("x")));
        assert!(py_truthy(&Value::from(1)));
    }

    #[test]
    fn the_first_user_prompt_is_three_hundred_code_points_not_bytes() {
        let conn = store();
        add_session(&conn, 1, 1, "s1", 0);
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, content_text, \
                                          is_sidechain) \
             VALUES (1, 1, 0, 't', 'user', ?, 0)",
            [&"é".repeat(400)],
        )
        .expect("row");
        let prompt = session_first_user_prompt(&conn, 1)
            .expect("query")
            .expect("some");
        assert_eq!(prompt.chars().count(), 300);
        assert_eq!(prompt.len(), 600, "600 BYTES — the slice is by code point");
    }

    #[test]
    fn a_zero_cost_session_still_renders_a_float() {
        let conn = store();
        add_session(&conn, 1, 1, "s1", 0);
        let cost = session_cost_usd(&conn, &engine(), 1).expect("query");
        assert!(cost.abs() < f64::EPSILON);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::from(cost)),
            "0.0",
            "round(0.0, 4) is a float and json.dumps writes 0.0, not 0"
        );
    }

    // ── list_team_sessions: the three strategies and their order ─────────────

    #[test]
    fn an_empty_store_is_an_empty_list_on_every_path() {
        let conn = store();
        assert!(!indexed_teams_available(&conn));
        assert!(
            list_team_sessions(&conn, 50, None)
                .expect("query")
                .is_empty()
        );
    }

    #[test]
    fn a_pre_v013_schema_probes_false_instead_of_raising() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch("CREATE TABLE sessions (id INTEGER PRIMARY KEY, session_id TEXT);")
            .expect("schema");
        // No `team_id` column: Python's `except sqlite3.OperationalError` and
        // this are the same answer.
        assert!(!indexed_teams_available(&conn));
    }

    #[test]
    fn the_indexed_path_wins_and_falls_back_to_the_team_id_for_a_null_lead() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 10);
        add_session(&conn, 2, 1, "sub-1", 4);
        conn.execute_batch(
            "UPDATE sessions SET team_id='crew', agent_role='lead' WHERE id=1;
             UPDATE sessions SET team_id='crew', agent_role='subagent' WHERE id=2;
             INSERT INTO agent_teams VALUES ('crew', 1, 'ts', 'the crew', 'lead-1', '{}');",
        )
        .expect("materialise");
        let rows = list_team_sessions(&conn, 50, None).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "lead-1");
        assert_eq!(rows[0].team_name, Value::from("crew"));
        assert_eq!(rows[0].agent_count, 1);
        assert_eq!(rows[0].sub_agent_message_count, 4);
        assert_eq!(rows[0].lead_message_count, 10);
        assert_eq!(rows[0].description.as_deref(), Some("the crew"));

        // A NULL lead_session_id falls back to the team id.
        conn.execute("UPDATE agent_teams SET lead_session_id = NULL", [])
            .expect("null the lead");
        let rows = list_team_sessions(&conn, 50, None).expect("query");
        assert_eq!(rows[0].session_id, "crew");
    }

    #[test]
    fn a_project_with_no_indexed_team_falls_through_rather_than_answering_empty() {
        let conn = store();
        // Project 1 is materialised; project 2 is not but has Task calls.
        add_session(&conn, 1, 1, "lead-1", 10);
        add_session(&conn, 2, 2, "other", 3);
        conn.execute_batch(
            "UPDATE sessions SET team_id='crew', agent_role='lead' WHERE id=1;
             INSERT INTO agent_teams VALUES ('crew', 1, 'ts', NULL, 'lead-1', '{}');",
        )
        .expect("materialise");
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, tools_json, \
                                          is_sidechain) \
             VALUES (1, 2, 0, 't', 'assistant', '[\"Task\"]', 0)",
            [],
        )
        .expect("task call");

        let scoped = list_team_sessions(&conn, 50, Some("beta")).expect("query");
        assert_eq!(scoped.len(), 1, "the task-tool path answered, not []");
        assert_eq!(scoped[0].session_id, "other");
        assert_eq!(scoped[0].team_name, Value::Null);
        assert_eq!(
            scoped[0].description.as_deref(),
            Some("1 Task/Agent sub-agent invocations (inline within parent session)")
        );
    }

    #[test]
    fn an_empty_project_string_is_not_none_and_skips_the_indexed_path() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 10);
        add_session(&conn, 2, 2, "other", 3);
        conn.execute_batch(
            "UPDATE sessions SET team_id='crew', agent_role='lead' WHERE id=1;
             INSERT INTO agent_teams VALUES ('crew', 1, 'ts', NULL, 'lead-1', '{}');",
        )
        .expect("materialise");
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, tools_json, \
                                          is_sidechain) \
             VALUES (1, 2, 0, 't', 'assistant', '[\"Agent\"]', 0)",
            [],
        )
        .expect("agent call");

        // `?project=` — `is None` is False, so the indexed gate runs `slug = ''`
        // and misses; the fall-through paths then read `""` as "no filter".
        let rows = list_team_sessions(&conn, 50, Some("")).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "other", "unfiltered task-tool answer");
        // …while `None` takes the indexed path and answers the OTHER row.
        let rows = list_team_sessions(&conn, 50, None).expect("query");
        assert_eq!(rows[0].session_id, "lead-1");
    }

    #[test]
    fn the_sidechain_scan_counts_distinct_agent_ids_and_honours_the_limit() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 0);
        add_session(&conn, 2, 1, "agent-aa", 0);
        add_session(&conn, 3, 1, "agent-bb", 0);
        // The lead's own non-sidechain rows.
        add_message(&conn, 1, 1, 0, "user", None);
        add_message(&conn, 2, 1, 1, "assistant", None);
        add_sidechain(&conn, 3, 2, r#"{"agentId":"aa"}"#);
        add_sidechain(&conn, 4, 2, r#"{"agentId":"aa"}"#);
        add_sidechain(&conn, 5, 3, r#"{"agentId":"bb"}"#);

        let rows = list_team_sessions(&conn, 50, None).expect("query");
        assert_eq!(rows.len(), 1, "only the lead has non-sidechain rows");
        assert_eq!(rows[0].session_id, "lead-1");
        assert_eq!(rows[0].agent_count, 2, "aa and bb, deduped");
        assert_eq!(rows[0].sub_agent_message_count, 3);
        assert_eq!(rows[0].team_name, Value::Null);

        assert!(
            list_team_sessions(&conn, 0, None)
                .expect("query")
                .is_empty()
        );
    }

    // ── build_team_graph ─────────────────────────────────────────────────────

    #[test]
    fn an_unknown_session_is_none_on_both_paths() {
        let conn = store();
        assert!(
            build_team_graph(&conn, &engine(), "nope")
                .expect("query")
                .is_none()
        );
    }

    #[test]
    fn a_subagent_id_resolves_up_to_its_teams_lead() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 0);
        add_session(&conn, 2, 1, "sub-1", 0);
        conn.execute_batch(
            "UPDATE sessions SET team_id='crew', agent_role='lead' WHERE id=1;
             UPDATE sessions SET team_id='crew', agent_role='subagent',
                                 spawned_by_session_id='lead-1' WHERE id=2;
             INSERT INTO agent_teams VALUES ('crew', 1, 'ts', 'desc', 'lead-1', '{}');",
        )
        .expect("materialise");
        let graph = build_team_graph(&conn, &engine(), "sub-1")
            .expect("query")
            .expect("resolved");
        assert_eq!(graph.session_id, "lead-1");
        assert_eq!(graph.team_name, Value::from("crew"));
        assert_eq!(graph.lead.agent_name, "team-lead");
        assert!(graph.lead.is_lead);
        assert_eq!(graph.agents.len(), 1);
        assert_eq!(graph.agents[0].session_id, "sub-1");
        assert_eq!(graph.agents[0].parent_session_id.as_deref(), Some("lead-1"));
        assert_eq!(graph.agents[0].agent_role, "subagent");

        // The dict literal puts `description` THIRD — not last, unlike asdict.
        let dict = graph.to_dict();
        let keys: Vec<&str> = dict
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "session_id",
                "team_name",
                "description",
                "project_slug",
                "project_display_name",
                "lead",
                "agents"
            ]
        );
    }

    #[test]
    fn an_unmaterialised_lead_gets_a_synthesised_placeholder() {
        let conn = store();
        add_session(&conn, 2, 1, "sub-1", 0);
        conn.execute_batch(
            "UPDATE sessions SET team_id='crew', agent_role='subagent' WHERE id=2;
             INSERT INTO agent_teams VALUES ('crew', 1, 'ts', NULL, 'ghost-lead', '{}');",
        )
        .expect("materialise");
        let graph = build_team_graph(&conn, &engine(), "ghost-lead")
            .expect("query")
            .expect("graph");
        assert_eq!(graph.lead.session_id, "ghost-lead");
        assert_eq!(graph.lead.message_count, 0);
        assert!(graph.lead.cost_usd.abs() < f64::EPSILON);
        assert_eq!(graph.agents.len(), 1);
        // The sub-agent's parent falls back to the team's lead id.
        assert_eq!(
            graph.agents[0].parent_session_id.as_deref(),
            Some("ghost-lead")
        );
    }

    #[test]
    fn the_heuristic_graph_keeps_unnamed_subagents_and_drops_disagreeing_ones() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 0);
        add_session(&conn, 2, 1, "agent-aa", 0);
        add_session(&conn, 3, 1, "agent-bb", 0);
        add_session(&conn, 4, 1, "agent-cc", 0);
        // The lead names its team.
        add_message(&conn, 1, 1, 0, "user", Some(r#"{"teamName":"crew"}"#));
        add_sidechain(&conn, 2, 2, r#"{"teamName":"crew","agentId":"aa"}"#);
        add_sidechain(&conn, 3, 3, r#"{"teamName":"other","agentId":"bb"}"#);
        add_sidechain(&conn, 4, 4, "{}");

        let graph = build_team_graph(&conn, &engine(), "lead-1")
            .expect("query")
            .expect("graph");
        assert_eq!(graph.team_name, Value::from("crew"));
        let names: Vec<&str> = graph
            .agents
            .iter()
            .map(|agent| agent.session_id.as_str())
            .collect();
        assert_eq!(
            names,
            ["agent-aa", "agent-cc"],
            "`other` disagrees and is dropped; the unnamed one is kept"
        );
        // `agent-cc` has no agentId, so the `agent-` filename convention names it.
        assert_eq!(graph.agents[1].agent_id.as_deref(), Some("cc"));
        assert_eq!(graph.agents[1].agent_name, "cc");
    }

    // ── get_agent_transcript ─────────────────────────────────────────────────

    #[test]
    fn the_transcript_fence_is_the_project_and_nothing_else() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 0);
        add_session(&conn, 2, 1, "sub-1", 0);
        add_session(&conn, 3, 2, "elsewhere", 0);
        add_message(&conn, 1, 2, 0, "user", None);

        assert!(
            get_agent_transcript(&conn, "lead-1", "sub-1")
                .expect("query")
                .is_some()
        );
        // Same project is enough — no team membership is required.
        assert!(
            get_agent_transcript(&conn, "lead-1", "lead-1")
                .expect("query")
                .is_some()
        );
        // Cross-project is None…
        assert!(
            get_agent_transcript(&conn, "lead-1", "elsewhere")
                .expect("query")
                .is_none()
        );
        // …and so is a missing lead.
        assert!(
            get_agent_transcript(&conn, "nope", "sub-1")
                .expect("query")
                .is_none()
        );
    }

    #[test]
    fn the_transcript_row_keeps_the_select_order_and_coerces_only_is_sidechain() {
        let conn = store();
        add_session(&conn, 1, 1, "lead-1", 0);
        conn.execute(
            "INSERT INTO messages_202607 (id, session_fk, seq, timestamp, role, model, \
                                          input_tokens, content_text, is_sidechain) \
             VALUES (5, 1, 0, 'ts', 'user', NULL, 3, 'hi', NULL)",
            [],
        )
        .expect("row");
        let rows = get_agent_transcript(&conn, "lead-1", "lead-1")
            .expect("query")
            .expect("rows");
        assert_eq!(rows.len(), 1);
        let keys: Vec<&str> = rows[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.as_slice(), TRANSCRIPT_COLUMNS.as_slice());
        // `bool(None)` is False — the key stays in position 13 either way.
        assert_eq!(rows[0]["is_sidechain"], Value::Bool(false));
        assert_eq!(rows[0]["model"], Value::Null);
        assert_eq!(rows[0]["input_tokens"], Value::from(3));
    }
}

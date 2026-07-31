//! `routes/projects.py` — 7 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-095` | `POST` | `/api/project` | `/api/project` | **open** — needs `infra/discovery.locate_logs` |
//! | `RS-5-096` | `GET` | `/api/project` | `/api/project` | ported |
//! | `RS-5-097` | `POST` | `/api/project-by-dir` | `/api/project-by-dir` | ported |
//! | `RS-5-098` | `GET` | `/api/recent-projects` | `/api/recent-projects` | ported |
//! | `RS-5-099` | `GET` | `/api/projects` | `/api/projects` | ported |
//! | `RS-5-100` | `GET` | `/api/providers` | `/api/providers` | ported |
//! | `RS-5-101` | `GET` | `/api/global-stats` | `/api/global-stats` | **open** — needs `queries.get_global_stats` |
//!
//! `GET /api/projects` is the wave's flagship, and not because it is the
//! prettiest: it is the endpoint the July perf campaign nearly lost. Its
//! docstring pins an eight-step ordering as load-bearing, with three measured
//! pathologies attached to getting it wrong (a mart-uncovered project that
//! never reaches the response costing +215 ms; narrowing the mart read without
//! moving the uncovered computation reading the whole store as uncovered at
//! 1,317 ms; the unscoped bulk helpers hanging the endpoint past 180 s on a
//! 382 K-message store). §6b says port the *shapes*. So the ordering below is
//! numbered against the Python comments, and the SQL is transliterated, not
//! rewritten.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::state::{AppState, CurrentProject};

/// `PROJECTS_DEFAULT_LIMIT` — the large default cap, not a page size.
const PROJECTS_DEFAULT_LIMIT: i64 = 500;
/// `PROJECTS_MAX_LIMIT` — the ceiling any single request can ask for.
const PROJECTS_MAX_LIMIT: i64 = 1000;
/// `RECENT_PROJECTS_LIMIT` — fixed, not a query param.
const RECENT_PROJECTS_LIMIT: i64 = 20;

/// `_WORKTREE_SLUG_MARKERS` from `services/worktrees.py`.
const WORKTREE_SLUG_MARKERS: [&str; 2] = ["--claude-worktrees-", "--worktrees-"];

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/project", get(get_current_project))
        .route("/api/project-by-dir", post(set_project_by_dir))
        .route("/api/recent-projects", get(get_recent_projects))
        .route("/api/projects", get(get_projects))
        .route("/api/providers", get(get_providers))
}

// ── the store row ────────────────────────────────────────────────────────────

/// `store/types.py::ProjectRow`.
#[derive(Debug, Clone)]
struct ProjectRow {
    id: i64,
    provider: String,
    slug: String,
    path: Option<String>,
    display_name: String,
    first_seen: f64,
    last_modified: f64,
}

/// The columns every `projects` read in this module selects, in order.
const PROJECT_COLUMNS: &str = "id, provider, slug, path, display_name, first_seen, last_modified";

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: row.get(0)?,
        provider: row.get(1)?,
        slug: row.get(2)?,
        path: row.get(3)?,
        display_name: row.get(4)?,
        first_seen: row.get(5)?,
        last_modified: row.get(6)?,
    })
}

/// `queries.list_projects` — every row, newest `last_modified` first.
fn list_projects(conn: &Connection, limit: Option<i64>) -> rusqlite::Result<Vec<ProjectRow>> {
    let mut sql = format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY last_modified DESC");
    if limit.is_some() {
        sql.push_str(" LIMIT ?");
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = match limit {
        Some(n) => stmt.query_map([n], project_row)?.collect::<Vec<_>>(),
        None => stmt.query_map([], project_row)?.collect::<Vec<_>>(),
    };
    rows.into_iter().collect()
}

/// `queries.get_project` — `fetchone`, so the FIRST row for the slug.
fn get_project(conn: &Connection, slug: &str) -> rusqlite::Result<Option<ProjectRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROJECT_COLUMNS} FROM projects WHERE slug = ?"
    ))?;
    let mut rows = stmt.query_map([slug], project_row)?;
    rows.next().transpose()
}

/// `queries.bulk_session_counts` — one GROUP BY over `sessions`, never `messages`.
fn bulk_session_counts(conn: &Connection) -> rusqlite::Result<HashMap<i64, i64>> {
    let mut stmt = conn.prepare("SELECT project_id, COUNT(*) FROM sessions GROUP BY project_id")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    rows.collect()
}

// ── GET /api/project ─────────────────────────────────────────────────────────

/// `get_current_project` — the mutable project state, or `no_project`.
async fn get_current_project(State(state): State<AppState>) -> JsonBody {
    let current = state.current_project();
    let Some(project_path) = current.project_path else {
        let mut obj = Map::new();
        obj.insert("status".to_owned(), Value::from("no_project"));
        obj.insert("message".to_owned(), Value::from("No project selected"));
        return JsonBody::ok(Value::Object(obj));
    };
    let mut obj = Map::new();
    obj.insert("status".to_owned(), Value::from("active"));
    obj.insert("project_path".to_owned(), Value::from(project_path));
    obj.insert(
        "log_path".to_owned(),
        current.log_path.clone().map_or(Value::Null, Value::from),
    );
    // `Path(deps.current_log_path).name if deps.current_log_path else None` —
    // note the truthiness: an EMPTY log path yields `None`, not `""`.
    obj.insert(
        "log_dir_name".to_owned(),
        match current.log_path.as_deref() {
            Some("") | None => Value::Null,
            Some(path) => Value::from(path_name(path)),
        },
    );
    JsonBody::ok(Value::Object(obj))
}

/// `pathlib.PurePath(p).name` — the last component, ignoring trailing slashes.
fn path_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

// ── POST /api/project-by-dir ─────────────────────────────────────────────────

/// `set_project_by_dir` — the state-mutating handler every project-scoped GET
/// depends on.
///
/// The body is read raw rather than through an extractor so a malformed one
/// fails the way FastAPI fails it. `data: dict[str, str]` means: a JSON object
/// is required, `dir_name` is looked up with `.get`, and a missing/empty value
/// is the handler's own `400`, not a `422`.
async fn set_project_by_dir(State(state): State<AppState>, body: Bytes) -> HandlerResult {
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| {
        // FastAPI's RequestValidationError -> 422. The `detail` list pydantic
        // builds is not reproduced byte-for-byte (DIV-053); the status is.
        HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid JSON body".to_owned(),
        )
    })?;
    let dir_name = parsed
        .get("dir_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if dir_name.is_empty() {
        return Err(HttpError::bad_request("Directory name is required"));
    }

    let worker_state = state.clone();
    let worker_dir = dir_name.clone();
    let resolved =
        tokio::task::spawn_blocking(move || resolve_project_by_dir(&worker_state, &worker_dir))
            .await
            .map_err(|err| join_failure(&err))??;

    state.set_current_project(CurrentProject {
        project_path: Some(resolved.project_path.clone()),
        log_path: Some(resolved.log_path.clone()),
    });

    let mut obj = Map::new();
    obj.insert("status".to_owned(), Value::from("success"));
    obj.insert(
        "project_path".to_owned(),
        Value::from(resolved.project_path),
    );
    obj.insert("log_path".to_owned(), Value::from(resolved.log_path));
    obj.insert("log_dir_name".to_owned(), Value::from(dir_name.clone()));
    obj.insert(
        "message".to_owned(),
        Value::from(format!("Now analyzing logs from: {dir_name}")),
    );
    Ok(JsonBody::ok(Value::Object(obj)))
}

struct ResolvedProject {
    project_path: String,
    log_path: String,
}

/// The blocking body of `set_project_by_dir`: one connection, two branches.
fn resolve_project_by_dir(state: &AppState, dir_name: &str) -> Result<ResolvedProject, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `except Exception: pass` around `get_project` — a store that cannot
    // answer is "not registered", not a 500.
    let project_row = get_project(&conn, dir_name).ok().flatten();

    if let Some(row) = project_row {
        // Registered: bypass the filesystem entirely.
        let log_path = resolve_legacy_log_dir(
            Some(&row.provider),
            row.path.as_deref(),
            dir_name,
            Some(&claude_projects_root()),
        );
        let project_path = match row.path.as_deref() {
            Some(path) if !path.is_empty() => path.to_owned(),
            _ => decode_slug(dir_name),
        };
        return Ok(ResolvedProject {
            project_path,
            log_path,
        });
    }

    // Not registered: an on-disk claude log dir, or a 404.
    let claude_base = claude_projects_root();
    let log_path = claude_base.join(dir_name);
    // Python: `(claude_base / dir_name).resolve()`, then a prefix check against
    // `claude_base.resolve() + os.sep`. `resolve()` on a non-existent path is
    // non-strict since 3.6, so it normalises without touching disk.
    let resolved = resolve_lexically(&log_path);
    let base = resolve_lexically(&claude_base);
    let mut prefix = base.as_os_str().to_string_lossy().into_owned();
    prefix.push(std::path::MAIN_SEPARATOR);
    if !resolved.to_string_lossy().starts_with(&prefix) {
        return Err(HttpError::bad_request("Invalid path"));
    }
    if !resolved.is_dir() {
        return Err(HttpError::not_found(format!(
            "Log directory not found: {dir_name}"
        )));
    }
    if !has_jsonl(&resolved) {
        return Err(HttpError::not_found(format!(
            "No log files found in directory: {dir_name}"
        )));
    }
    Ok(ResolvedProject {
        project_path: decode_slug(dir_name),
        log_path: resolved.to_string_lossy().into_owned(),
    })
}

/// `dir_name[1:].replace("-", "/") if dir_name.startswith("-") else dir_name`.
fn decode_slug(dir_name: &str) -> String {
    match dir_name.strip_prefix('-') {
        Some(rest) => rest.replace('-', "/"),
        None => dir_name.to_owned(),
    }
}

/// `Path.glob("*.jsonl")` — does the directory hold at least one transcript?
fn has_jsonl(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
    })
}

/// `Path.resolve()` without the symlink walk — lexical `.`/`..` normalisation.
///
/// Enough for the containment check above, which is about the *string* prefix
/// Python compares, and it never fails on a path that does not exist.
fn resolve_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `adapters/claude.py::default_projects_root` — `$CLAUDE_CONFIG_DIR`, else `~/.claude`, then `/projects`.
fn claude_projects_root() -> PathBuf {
    let home = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(raw) if !raw.trim().is_empty() => expand_user(raw.trim()),
        _ => home_dir().join(".claude"),
    };
    home.join("projects")
}

fn expand_user(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None if raw == "~" => home_dir(),
        None => PathBuf::from(raw),
    }
}

fn home_dir() -> PathBuf {
    #[allow(
        deprecated,
        reason = "matches stax_core::settings — the platform-correct answer on the pinned toolchain"
    )]
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `adapters/claude.py::resolve_legacy_log_dir` — the single home for the
/// slug → dir fallback, and claude-only on purpose.
fn resolve_legacy_log_dir(
    provider: Option<&str>,
    stored_path: Option<&str>,
    slug: &str,
    projects_root: Option<&Path>,
) -> String {
    if let Some(path) = stored_path
        && !path.is_empty()
    {
        return path.to_owned();
    }
    let provider = provider.filter(|p| !p.is_empty()).unwrap_or("claude");
    if provider == "claude" || provider == "anthropic" {
        let root = projects_root.map_or_else(claude_projects_root, Path::to_path_buf);
        return root.join(slug).to_string_lossy().into_owned();
    }
    String::new()
}

// ── GET /api/recent-projects ─────────────────────────────────────────────────

async fn get_recent_projects(State(state): State<AppState>) -> JsonBody {
    let payload = tokio::task::spawn_blocking(move || {
        let conn = state.connect()?;
        let rows = list_projects(&conn, Some(RECENT_PROJECTS_LIMIT))?;
        anyhow::Ok(rows)
    })
    .await;

    // Python wraps the whole body in `except Exception as e: return
    // {"projects": [], "error": str(e)}` — with a 200. Reproduced, status and
    // all: this endpoint never fails loudly.
    let rows = match payload {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => return error_projects(&err.to_string()),
        Err(err) => return error_projects(&err.to_string()),
    };

    let projects: Vec<Value> = rows
        .iter()
        .map(|p| {
            let mut obj = Map::new();
            obj.insert("dir_name".to_owned(), Value::from(p.slug.clone()));
            obj.insert(
                "log_path".to_owned(),
                Value::from(p.path.clone().unwrap_or_default()),
            );
            obj.insert("last_modified".to_owned(), Value::from(p.last_modified));
            // "not tracked in store" — a literal 0, not a count.
            obj.insert("file_count".to_owned(), Value::from(0));
            Value::Object(obj)
        })
        .collect();

    let mut obj = Map::new();
    obj.insert("projects".to_owned(), Value::Array(projects));
    JsonBody::ok(Value::Object(obj))
}

fn error_projects(message: &str) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("projects".to_owned(), Value::Array(Vec::new()));
    obj.insert("error".to_owned(), Value::from(message));
    JsonBody::ok(Value::Object(obj))
}

// ── GET /api/providers ───────────────────────────────────────────────────────

async fn get_providers(State(state): State<AppState>) -> JsonBody {
    let rows = tokio::task::spawn_blocking(move || {
        let conn = state.connect()?;
        let mut stmt = conn.prepare(
            "SELECT projects.provider AS provider, \
                    COUNT(DISTINCT projects.id) AS project_count, \
                    COUNT(DISTINCT sessions.id) AS session_count \
             FROM projects \
             LEFT JOIN sessions ON sessions.project_id = projects.id \
             GROUP BY projects.provider \
             ORDER BY project_count DESC",
        )?;
        let rows: Vec<(Option<String>, i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        anyhow::Ok(rows)
    })
    .await;

    let rows = match rows {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => return providers_error(&err.to_string()),
        Err(err) => return providers_error(&err.to_string()),
    };

    let providers: Vec<Value> = rows
        .into_iter()
        .map(|(provider, project_count, session_count)| {
            let mut obj = Map::new();
            obj.insert(
                "provider".to_owned(),
                Value::from(
                    provider
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "unknown".to_owned())
                        .to_lowercase(),
                ),
            );
            obj.insert("project_count".to_owned(), Value::from(project_count));
            obj.insert("session_count".to_owned(), Value::from(session_count));
            Value::Object(obj)
        })
        .collect();

    let mut obj = Map::new();
    obj.insert("providers".to_owned(), Value::Array(providers));
    JsonBody::ok(Value::Object(obj))
}

fn providers_error(message: &str) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("providers".to_owned(), Value::Array(Vec::new()));
    obj.insert(
        "error".to_owned(),
        Value::from(format!("Failed to list providers: {message}")),
    );
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

// ── GET /api/projects ────────────────────────────────────────────────────────

/// Query parameters, in the declared signature's order.
struct ProjectsParams {
    include_stats: bool,
    sort_by: String,
    limit: Option<i64>,
    offset: i64,
    provider_filter: Option<HashSet<String>>,
    include_worktrees: bool,
}

async fn get_projects(State(state): State<AppState>, RawQuery(raw): RawQuery) -> JsonBody {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let params = match parse_projects_params(&query) {
        Ok(params) => params,
        // A query parameter that will not coerce is a 422 in FastAPI, and the
        // handler's `try/except` never sees it — validation runs first.
        Err(err) => {
            return JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                validation_detail(&err),
            );
        }
    };

    let payload =
        tokio::task::spawn_blocking(move || compute_projects_payload(&state, &params)).await;

    match payload {
        Ok(Ok(payload)) => JsonBody::ok(payload),
        // `except Exception as e: traceback.print_exc(); return
        // JSONResponse({"error": …}, status_code=500)`.
        Ok(Err(err)) => projects_error(&err.to_string()),
        Err(err) => projects_error(&err.to_string()),
    }
}

fn projects_error(message: &str) -> JsonBody {
    let mut obj = Map::new();
    obj.insert(
        "error".to_owned(),
        Value::from(format!("Failed to get projects: {message}")),
    );
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

/// pydantic's error body, best effort — see DIV-053.
fn validation_detail(err: &crate::qs::QueryError) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from(err.kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(err.field.clone())]),
    );
    entry.insert(
        "msg".to_owned(),
        Value::from(match err.kind {
            "bool_parsing" => "Input should be a valid boolean, unable to interpret input",
            _ => "Input should be a valid integer, unable to parse string as an integer",
        }),
    );
    entry.insert("input".to_owned(), Value::from(err.input.clone()));
    let mut obj = Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    Value::Object(obj)
}

fn parse_projects_params(query: &Query) -> Result<ProjectsParams, crate::qs::QueryError> {
    Ok(ProjectsParams {
        include_stats: query.bool_or("include_stats", false)?,
        sort_by: query.str_or("sort_by", "last_modified").to_owned(),
        limit: query.opt_int("limit")?,
        offset: query.int_or("offset", 0)?,
        provider_filter: normalise_provider_filter(query.opt_list("provider").as_deref()),
        include_worktrees: query.bool_or("include_worktrees", false)?,
    })
}

/// `_normalise_provider_filter` — lowercase, drop empties, empty set → `None`.
fn normalise_provider_filter(provider: Option<&[String]>) -> Option<HashSet<String>> {
    let provider = provider?;
    if provider.is_empty() {
        return None;
    }
    let normed: HashSet<String> = provider
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    (!normed.is_empty()).then_some(normed)
}

/// `_clamp_pagination` — `(limit, offset)` bounded and non-negative.
fn clamp_pagination(limit: Option<i64>, offset: i64) -> (i64, i64) {
    let resolved = match limit {
        None => PROJECTS_DEFAULT_LIMIT,
        Some(n) => n.clamp(1, PROJECTS_MAX_LIMIT).max(1),
    };
    (resolved, offset.max(0))
}

/// One assembled project row, before it becomes JSON.
///
/// `_ids` is the provider-duplicate id set the payload pops before returning;
/// it is a field here rather than a JSON key so it *cannot* leak.
struct AssembledProject {
    dir_name: String,
    log_path: String,
    file_count: i64,
    total_size_mb: f64,
    last_modified: f64,
    first_seen: f64,
    display_name: String,
    provider: String,
    providers: Vec<String>,
    ids: Vec<i64>,
    worktree_of: Option<String>,
    worktree_rollup: Option<WorktreeRollup>,
    stats: Option<Value>,
}

struct WorktreeRollup {
    count: i64,
    sessions: i64,
    cost: f64,
}

/// `_compute_projects_payload` — the blocking body, eight numbered steps.
fn compute_projects_payload(state: &AppState, params: &ProjectsParams) -> anyhow::Result<Value> {
    let (limit, offset) = clamp_pagination(params.limit, params.offset);

    // Derive claude's projects root ONCE per request (the July hoist: 14.3ms →
    // 11.6ms on the maintainer's 334-project store). Deliberately not cached
    // process-wide — `CLAUDE_CONFIG_DIR` must stay live.
    let projects_root = claude_projects_root();

    let conn = state.connect()?;

    // ── (1) the listing universe ────────────────────────────────────────────
    let mut project_rows = list_projects(&conn, None)?;
    if let Some(filter) = &params.provider_filter {
        project_rows.retain(|p| filter.contains(&p.provider.to_lowercase()));
    }

    // ── (2) session counts ──────────────────────────────────────────────────
    let session_counts = bulk_session_counts(&conn)?;

    // Deferred to step 7; step 4 may partially populate `mart_rows`.
    let mut mart_rows: HashMap<i64, MartRow> = HashMap::new();
    let mut lite_stats: HashMap<i64, LiteStats> = HashMap::new();
    let mut cost_by_pid: HashMap<i64, f64> = HashMap::new();

    // ── (3) assemble one row per slug ───────────────────────────────────────
    // `defaultdict(list)` keyed by slug: insertion-ordered, so the group order
    // is first-appearance order in the `last_modified DESC` listing. A HashMap
    // would randomise the pre-sort order — invisible until `sort_by` ties.
    let mut slug_order: Vec<String> = Vec::new();
    let mut slug_groups: HashMap<String, Vec<ProjectRow>> = HashMap::new();
    for row in project_rows {
        let entry = slug_groups.entry(row.slug.clone()).or_insert_with(|| {
            slug_order.push(row.slug.clone());
            Vec::new()
        });
        entry.push(row);
    }

    let mut projects: Vec<AssembledProject> = Vec::with_capacity(slug_order.len());
    let mut size_cache: HashMap<(String, i64, i64), f64> = HashMap::new();
    for slug in &slug_order {
        let group = &slug_groups[slug];
        // `max(group, key=...)` returns the FIRST maximal element; Rust's
        // `max_by_key` returns the LAST. Fold explicitly.
        let primary = group
            .iter()
            .reduce(|best, row| {
                if row.last_modified > best.last_modified {
                    row
                } else {
                    best
                }
            })
            .expect("a group is never empty");
        let log_path = resolve_legacy_log_dir(
            Some(&primary.provider),
            primary.path.as_deref(),
            slug,
            Some(&projects_root),
        );
        let mut providers: Vec<String> = group
            .iter()
            .map(|p| p.provider.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        providers.sort();
        projects.push(AssembledProject {
            dir_name: slug.clone(),
            total_size_mb: dir_size_mb(&log_path, &mut size_cache),
            log_path,
            file_count: group
                .iter()
                .map(|p| session_counts.get(&p.id).copied().unwrap_or(0))
                .sum(),
            last_modified: group
                .iter()
                .map(|p| p.last_modified)
                .fold(f64::NEG_INFINITY, f64::max),
            first_seen: group
                .iter()
                .map(|p| p.first_seen)
                .fold(f64::INFINITY, f64::min),
            display_name: primary.display_name.clone(),
            provider: primary.provider.clone(),
            providers,
            ids: group.iter().map(|p| p.id).collect(),
            worktree_of: None,
            worktree_rollup: None,
            stats: None,
        });
    }

    // ── (4) worktree attribution roll-up (campaign #8) ──────────────────────
    let parent_by_slug = worktree_parents_from_store(&conn)?;
    if params.include_worktrees {
        annotate_worktree_fragments(&mut projects, &parent_by_slug);
    } else {
        let folded = fold_worktree_fragments(&mut projects, &parent_by_slug);
        if !folded.is_empty() {
            let engine = crate::pricing::engine(&conn, state.package_dir())?;
            let fragment_cost_usd =
                fragment_costs_usd(&conn, &engine, &folded, &mut mart_rows, &mut cost_by_pid)?;
            for (parent_slug, fragments) in &folded {
                let parent = projects
                    .iter_mut()
                    .find(|p| &p.dir_name == parent_slug)
                    .expect("a fold target is always in the kept list");
                parent.worktree_rollup = Some(WorktreeRollup {
                    count: i64::try_from(fragments.len()).unwrap_or(i64::MAX),
                    sessions: fragments.iter().map(|f| f.file_count).sum(),
                    cost: fragments
                        .iter()
                        .map(|f| fragment_cost_usd.get(&f.dir_name).copied().unwrap_or(0.0))
                        .sum(),
                });
            }
        }
    }

    // ── (5) sort ────────────────────────────────────────────────────────────
    // Python's `list.sort` is stable and so is `sort_by`; an unknown `sort_by`
    // value leaves the list in assembly order, which is also reproduced.
    match params.sort_by.as_str() {
        "last_modified" => projects.sort_by(|a, b| {
            b.last_modified
                .partial_cmp(&a.last_modified)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "first_seen" => projects.sort_by(|a, b| {
            a.first_seen
                .partial_cmp(&b.first_seen)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "size" => projects.sort_by(|a, b| {
            b.total_size_mb
                .partial_cmp(&a.total_size_mb)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "name" => projects.sort_by(|a, b| a.display_name.cmp(&b.display_name)),
        _ => {}
    }

    // ── (6) total_count, then the page slice ────────────────────────────────
    let total_count = i64::try_from(projects.len()).unwrap_or(i64::MAX);
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(projects.len());
    let end = offset
        .checked_add(limit)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX)
        .min(projects.len());
    let mut projects: Vec<AssembledProject> = projects.drain(start..end).collect();

    // ── (7) stats sources, scoped to the PAGE ───────────────────────────────
    if params.include_stats {
        let page_ids: HashSet<i64> = projects
            .iter()
            .flat_map(|p| p.ids.iter().copied())
            .collect();
        let mut sorted_ids: Vec<i64> = page_ids.iter().copied().collect();
        sorted_ids.sort_unstable();
        for row in list_project_mart(&conn, params.provider_filter.as_ref(), Some(&sorted_ids))? {
            if mart_row_is_placeholder(&row) {
                continue;
            }
            mart_rows.entry(row.project_id).or_insert(row);
        }
        let uncovered: Vec<i64> = {
            let mut ids: Vec<i64> = page_ids
                .iter()
                .copied()
                .filter(|id| !mart_rows.contains_key(id))
                .collect();
            ids.sort_unstable();
            ids
        };
        if !uncovered.is_empty() {
            lite_stats = bulk_project_lite_stats(&conn, &uncovered)?;
            let engine = crate::pricing::engine(&conn, state.package_dir())?;
            cost_by_pid.extend(bulk_project_cost(&conn, &engine, &uncovered)?);
        }
    }

    // ── (8) per-project stats over the page ─────────────────────────────────
    if params.include_stats {
        for project in &mut projects {
            project.stats = Some(stats_for_ids(
                &project.ids,
                &mart_rows,
                &lite_stats,
                &cost_by_pid,
            ));
        }
    }
    drop(conn);

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let rate = currency
        .get("rate_from_usd")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    if (rate - 1.0).abs() > f64::EPSILON {
        for project in &mut projects {
            if let Some(rollup) = &mut project.worktree_rollup {
                rollup.cost *= rate;
            }
            if params.include_stats
                && let Some(Value::Object(stats)) = &mut project.stats
                && let Some(total) = stats.get_mut("total_cost")
                && let Some(value) = total.as_f64()
            {
                *total = Value::from(value * rate);
            }
        }
    }

    let rendered: Vec<Value> = projects.iter().map(project_to_json).collect();
    let mut payload = Map::new();
    payload.insert("projects".to_owned(), Value::Array(rendered));
    payload.insert("total_count".to_owned(), Value::from(total_count));
    payload.insert("limit".to_owned(), Value::from(limit));
    payload.insert("offset".to_owned(), Value::from(offset));
    payload.insert(
        "has_more".to_owned(),
        Value::Bool(offset.saturating_add(limit) < total_count),
    );
    let mut cache_status = Map::new();
    cache_status.insert("cached_count".to_owned(), Value::from(0));
    cache_status.insert("total_projects".to_owned(), Value::from(total_count));
    payload.insert("cache_status".to_owned(), Value::Object(cache_status));
    payload.insert("currency".to_owned(), currency);
    Ok(Value::Object(payload))
}

/// Key order is the dict-literal order in `_compute_projects_payload` step 3,
/// with the campaign-#8 keys appended where Python assigns them.
fn project_to_json(project: &AssembledProject) -> Value {
    let mut obj = Map::new();
    obj.insert("dir_name".to_owned(), Value::from(project.dir_name.clone()));
    obj.insert("log_path".to_owned(), Value::from(project.log_path.clone()));
    obj.insert("file_count".to_owned(), Value::from(project.file_count));
    obj.insert(
        "total_size_mb".to_owned(),
        Value::from(project.total_size_mb),
    );
    obj.insert(
        "last_modified".to_owned(),
        Value::from(project.last_modified),
    );
    obj.insert("first_seen".to_owned(), Value::from(project.first_seen));
    obj.insert(
        "display_name".to_owned(),
        Value::from(project.display_name.clone()),
    );
    obj.insert("in_cache".to_owned(), Value::Bool(false));
    obj.insert("url_slug".to_owned(), Value::from(project.dir_name.clone()));
    obj.insert(
        "stats".to_owned(),
        project.stats.clone().unwrap_or(Value::Null),
    );
    obj.insert("provider".to_owned(), Value::from(project.provider.clone()));
    obj.insert(
        "providers".to_owned(),
        Value::Array(project.providers.iter().cloned().map(Value::from).collect()),
    );
    // `_ids` is popped before the response; the roll-up / badge keys are
    // assigned after the literal, so they land here.
    if let Some(rollup) = &project.worktree_rollup {
        obj.insert("worktree_count".to_owned(), Value::from(rollup.count));
        obj.insert("worktree_sessions".to_owned(), Value::from(rollup.sessions));
        obj.insert("worktree_cost".to_owned(), Value::from(rollup.cost));
    }
    if let Some(parent) = &project.worktree_of {
        obj.insert("worktree_of".to_owned(), Value::from(parent.clone()));
    }
    Value::Object(obj)
}

// ── worktrees (campaign #8) ──────────────────────────────────────────────────

/// `services/worktrees.py::is_worktree_slug` — PURE, leftmost marker wins.
fn is_worktree_slug(slug: &str) -> Option<&str> {
    if slug.is_empty() {
        return None;
    }
    let mut best: Option<usize> = None;
    for marker in WORKTREE_SLUG_MARKERS {
        if let Some(idx) = slug.find(marker)
            // `idx > 0` (a leading marker is not a parent) and a non-empty tail.
            && idx > 0
            && !slug[idx + marker.len()..].is_empty()
        {
            best = Some(best.map_or(idx, |b| b.min(idx)));
        }
    }
    best.map(|idx| &slug[..idx])
}

/// `_worktree_parents_from_store` — v027 `projects.worktree_of`, feature-detected.
fn worktree_parents_from_store(conn: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut has_column = false;
    {
        let mut stmt = conn.prepare("PRAGMA table_info(projects)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == "worktree_of" {
                has_column = true;
            }
        }
    }
    if !has_column {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare(
        "SELECT slug, worktree_of FROM projects \
         WHERE worktree_of IS NOT NULL AND worktree_of != ''",
    )?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// `_worktree_parent_of` — attribution first, slug shape second.
fn worktree_parent_of<'a>(
    slug: &'a str,
    parent_by_slug: &'a HashMap<String, String>,
) -> Option<&'a str> {
    // `parent_by_slug.get(slug) or _is_worktree_slug(slug)` — Python's `or`
    // also falls through on an EMPTY stored parent, not just a missing one.
    match parent_by_slug.get(slug) {
        Some(parent) if !parent.is_empty() => Some(parent.as_str()),
        _ => is_worktree_slug(slug),
    }
}

/// `_fold_worktree_fragments` — partition into kept rows and `{parent: fragments}`.
fn fold_worktree_fragments(
    projects: &mut Vec<AssembledProject>,
    parent_by_slug: &HashMap<String, String>,
) -> Vec<(String, Vec<AssembledProject>)> {
    let listed: HashSet<String> = projects.iter().map(|p| p.dir_name.clone()).collect();
    let mut kept: Vec<AssembledProject> = Vec::with_capacity(projects.len());
    // Insertion-ordered, because the roll-up loop below walks it and a
    // HashMap would randomise which parent is written first. (It cannot change
    // the payload — each parent is touched once — but a deterministic harness
    // is worth more than the microsecond.)
    let mut folded_order: Vec<String> = Vec::new();
    let mut folded: HashMap<String, Vec<AssembledProject>> = HashMap::new();

    for project in projects.drain(..) {
        let parent = worktree_parent_of(&project.dir_name, parent_by_slug).map(str::to_owned);
        let foldable = match &parent {
            Some(parent) => {
                parent != &project.dir_name
                    && listed.contains(parent)
                    && worktree_parent_of(parent, parent_by_slug).is_none()
            }
            None => false,
        };
        if foldable {
            let parent = parent.expect("foldable implies a parent");
            folded
                .entry(parent.clone())
                .or_insert_with(|| {
                    folded_order.push(parent.clone());
                    Vec::new()
                })
                .push(project);
        } else {
            kept.push(project);
        }
    }
    *projects = kept;
    folded_order
        .into_iter()
        .map(|slug| {
            let fragments = folded.remove(&slug).unwrap_or_default();
            (slug, fragments)
        })
        .collect()
}

/// `_annotate_worktree_fragments` — `?include_worktrees=1`: badge, never fold.
fn annotate_worktree_fragments(
    projects: &mut [AssembledProject],
    parent_by_slug: &HashMap<String, String>,
) {
    let listed: HashSet<String> = projects.iter().map(|p| p.dir_name.clone()).collect();
    for project in projects.iter_mut() {
        // NOTE the asymmetry with the fold: here a *stored* attribution is
        // honoured even when the parent is not listed, and only the
        // shape-derived match requires a listed parent.
        let parent = match parent_by_slug.get(&project.dir_name) {
            Some(parent) => Some(parent.clone()),
            None => is_worktree_slug(&project.dir_name)
                .filter(|shaped| listed.contains(*shaped))
                .map(str::to_owned),
        };
        if let Some(parent) = parent
            && !parent.is_empty()
            && parent != project.dir_name
        {
            project.worktree_of = Some(parent);
        }
    }
}

/// `_fragment_costs_usd` — mart-first, scoped bulk fallback, never a fabricated 0.0.
fn fragment_costs_usd(
    conn: &Connection,
    engine: &stax_etl::pricing::costs::PricingEngine,
    folded: &[(String, Vec<AssembledProject>)],
    mart_rows: &mut HashMap<i64, MartRow>,
    cost_by_pid: &mut HashMap<i64, f64>,
) -> anyhow::Result<HashMap<String, f64>> {
    let fragments: Vec<&AssembledProject> =
        folded.iter().flat_map(|(_, group)| group.iter()).collect();
    let need: HashSet<i64> = fragments
        .iter()
        .flat_map(|f| f.ids.iter().copied())
        .collect();

    let mut missing_mart: Vec<i64> = need
        .iter()
        .copied()
        .filter(|id| !mart_rows.contains_key(id))
        .collect();
    missing_mart.sort_unstable();
    if !missing_mart.is_empty() {
        for row in list_project_mart(conn, None, Some(&missing_mart))? {
            if mart_row_is_placeholder(&row) {
                continue;
            }
            mart_rows.entry(row.project_id).or_insert(row);
        }
    }

    let mut missing: Vec<i64> = need
        .iter()
        .copied()
        .filter(|id| !mart_rows.contains_key(id) && !cost_by_pid.contains_key(id))
        .collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        // MERGES, never rebinds — dropping the caller's entries would roll up
        // silent zeros that then get FX-converted like real numbers.
        cost_by_pid.extend(bulk_project_cost(conn, engine, &missing)?);
    }

    let mut costs = HashMap::new();
    for fragment in fragments {
        let mut total = 0.0_f64;
        for id in &fragment.ids {
            total += match mart_rows.get(id) {
                Some(row) => row.total_cost_usd,
                None => cost_by_pid.get(id).copied().unwrap_or(0.0),
            };
        }
        costs.insert(fragment.dir_name.clone(), total);
    }
    Ok(costs)
}

// ── directory sizes ──────────────────────────────────────────────────────────

/// `_dir_size_mb` — sum of `*.jsonl` sizes, rounded to 2dp, `(path, mtime)`-keyed.
///
/// The Python cache is module-global and lives for the process; this one is
/// per-request. That is a *narrowing*, and it is deliberate: the payload is
/// identical either way (the key includes the mtime, so a hit and a miss return
/// the same number), and a process-global mutable cache is exactly the shape
/// the injection law exists to remove. Cost is one extra glob per directory on
/// a repeat request.
fn dir_size_mb(log_dir: &str, cache: &mut HashMap<(String, i64, i64), f64>) -> f64 {
    if log_dir.is_empty() {
        return 0.0;
    }
    let path = Path::new(log_dir);
    let Ok(meta) = std::fs::metadata(path) else {
        return 0.0;
    };
    if !meta.is_dir() {
        return 0.0;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    let key = (
        log_dir.to_owned(),
        mtime.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
        mtime.map_or(0, |d| i64::from(d.subsec_nanos())),
    );
    if let Some(hit) = cache.get(&key) {
        return *hit;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0.0;
    };
    let total: u64 = entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
        .filter_map(|entry| entry.metadata().ok().map(|m| m.len()))
        .sum();
    let mb = round_half_even(total as f64 / (1024.0 * 1024.0), 2);
    cache.insert(key, mb);
    mb
}

/// Python's `round(x, 2)` — banker's rounding on the *decimal* value, not
/// `f64::round`'s half-away-from-zero.
///
/// CPython's `round(float, n)` goes through `_Py_dg_dtoa`, i.e. correct decimal
/// rounding with ties to even. `(x * 100.0).round() / 100.0` differs from it on
/// exact ties *and* on values whose binary representation straddles the
/// midpoint. Formatting through the shortest-repr path and re-parsing gets the
/// same answer for the 2-dp case this function needs, because `{:.*}` in Rust
/// also rounds the decimal expansion half-to-even.
fn round_half_even(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.digits$}").parse().unwrap_or(value)
}

// ── project_mart + bulk helpers ──────────────────────────────────────────────

/// The `project_mart` columns this module reads.
#[derive(Debug, Clone)]
struct MartRow {
    project_id: i64,
    first_ts: Option<String>,
    last_ts: Option<String>,
    total_messages: i64,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cache_read: i64,
    total_cache_create: i64,
    total_cost_usd: f64,
    total_commands: i64,
    total_records: i64,
}

/// `mart_queries.list_project_mart`, narrowed to the columns used here.
///
/// An **empty** `project_ids` slice means "no projects" and returns `[]`
/// without touching the DB — never silently promoted to "all". `GET
/// /api/projects` relies on exactly that for an offset past the end of the list.
fn list_project_mart(
    conn: &Connection,
    provider_filter: Option<&HashSet<String>>,
    project_ids: Option<&[i64]>,
) -> rusqlite::Result<Vec<MartRow>> {
    if !table_exists(conn, "project_mart")? {
        return Ok(Vec::new());
    }
    if project_ids.is_some_and(<[i64]>::is_empty) {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT project_id, first_ts, last_ts, total_messages, total_input_tokens, \
                          total_output_tokens, total_cache_read, total_cache_create, \
                          total_cost_usd, total_commands, total_records \
                   FROM project_mart"
        .to_owned();
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(filter) = provider_filter
        && !filter.is_empty()
    {
        // A Python `set` has no order; the bound-parameter order therefore has
        // none either and cannot affect the result. Sorted here so the emitted
        // SQL is reproducible for EXPLAIN work.
        let mut providers: Vec<String> = filter.iter().map(|p| p.to_lowercase()).collect();
        providers.sort();
        clauses.push(format!(
            "LOWER(provider) IN ({})",
            vec!["?"; providers.len()].join(",")
        ));
        for provider in providers {
            params.push(Box::new(provider));
        }
    }
    if let Some(ids) = project_ids {
        clauses.push(format!(
            "project_id IN ({})",
            vec!["?"; ids.len()].join(",")
        ));
        for id in ids {
            params.push(Box::new(*id));
        }
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(MartRow {
            project_id: row.get(0)?,
            first_ts: row.get(1)?,
            last_ts: row.get(2)?,
            total_messages: row.get(3)?,
            total_input_tokens: row.get(4)?,
            total_output_tokens: row.get(5)?,
            total_cache_read: row.get(6)?,
            total_cache_create: row.get(7)?,
            total_cost_usd: row.get(8)?,
            total_commands: row.get(9)?,
            total_records: row.get(10)?,
        })
    })?;
    rows.collect()
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

/// `_mart_row_is_placeholder` — `total_records > 0 AND total_messages == 0`.
///
/// That pair selects exactly the empty coverage seeds whose dates / command
/// count / cost the bulk fallback can still recover (54 of 334 on the
/// maintainer's store) and never a genuinely empty project.
fn mart_row_is_placeholder(row: &MartRow) -> bool {
    row.total_records > 0 && row.total_messages == 0
}

/// The `bulk_project_lite_stats` value shape.
#[derive(Debug, Clone, Default)]
struct LiteStats {
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cache_read: i64,
    total_cache_write: i64,
    first_message_date: Option<String>,
    last_message_date: Option<String>,
    total_commands: i64,
}

/// `queries._id_chunks` — de-duplicated, sorted, `_MAX_IN_PARAMS`-sized.
const MAX_IN_PARAMS: usize = 900;

fn id_chunks(ids: &[i64]) -> Vec<Vec<i64>> {
    let mut unique: Vec<i64> = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique.chunks(MAX_IN_PARAMS).map(<[i64]>::to_vec).collect()
}

/// `queries._scoped_rows`'s scope clause — **the list subquery, not a join
/// predicate**.
///
/// §6b calls this shape load-bearing and the July campaign measured it: 912 ms
/// with `s.project_id IN (…)` on the join versus 9 ms with this, on 91
/// uncovered ids over a 382 K-message store. `messages` is a UNION-ALL view over
/// 16 monthly partitions and SQLite will not push a predicate on the joined
/// `sessions` row into the arms; a `session_fk` constraint it will push, and
/// each arm then seeks its `(session_fk, seq)` index. Do not "simplify" this.
fn scope_clause(chunk_len: usize) -> String {
    format!(
        " AND m.session_fk IN (SELECT id FROM sessions WHERE project_id IN ({}))",
        vec!["?"; chunk_len].join(",")
    )
}

/// `queries.bulk_project_lite_stats`, scoped. An empty scope reads nothing.
fn bulk_project_lite_stats(
    conn: &Connection,
    project_ids: &[i64],
) -> rusqlite::Result<HashMap<i64, LiteStats>> {
    let base_where = "WHERE (m.model IS NULL OR m.model != '<synthetic>')";
    let mut out = HashMap::new();
    for chunk in id_chunks(project_ids) {
        let sql = format!(
            "SELECT s.project_id, \
                    SUM(m.input_tokens), SUM(m.output_tokens), \
                    SUM(m.cache_read_tokens), SUM(m.cache_create_tokens), \
                    MIN(m.timestamp), MAX(m.timestamp), \
                    SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END) AS user_msgs, \
                    COUNT(*) AS total_msgs \
             FROM messages m \
             JOIN sessions s ON s.id = m.session_fk \
             {base_where}{scope} \
             GROUP BY s.project_id",
            scope = scope_clause(chunk.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                LiteStats {
                    total_input_tokens: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    total_output_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    total_cache_read: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    total_cache_write: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    first_message_date: row.get(5)?,
                    last_message_date: row.get(6)?,
                    total_commands: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                },
            ))
        })?;
        for row in rows {
            let (pid, stats) = row?;
            out.insert(pid, stats);
        }
    }
    Ok(out)
}

/// `queries.bulk_project_cost`, scoped — tokens per (project, provider, model,
/// speed), priced with the same engine the reference prices with.
fn bulk_project_cost(
    conn: &Connection,
    engine: &stax_etl::pricing::costs::PricingEngine,
    project_ids: &[i64],
) -> rusqlite::Result<HashMap<i64, f64>> {
    let base_where = "WHERE m.model IS NOT NULL AND m.model != '' AND m.model != '<synthetic>'";
    let mut cost_by_pid: HashMap<i64, f64> = HashMap::new();
    for chunk in id_chunks(project_ids) {
        let sql = format!(
            "SELECT s.project_id, \
                    p.provider, \
                    COALESCE(m.model, ''), \
                    COALESCE(m.speed, 'standard'), \
                    SUM(m.input_tokens), SUM(m.output_tokens), \
                    SUM(m.cache_read_tokens), SUM(m.cache_create_tokens) \
             FROM messages m \
             JOIN sessions s ON s.id = m.session_fk \
             JOIN projects p ON p.id = s.project_id \
             {base_where}{scope} \
             GROUP BY s.project_id, p.provider, m.model, m.speed",
            scope = scope_clause(chunk.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            ))
        })?;
        for row in rows {
            let (pid, provider, model, speed, input, output, cache_read, cache_create) = row?;
            // `projects.provider` is the TOOL that wrote the transcript, not the
            // vendor whose rate card applies.
            let pricing_provider = engine.resolve_pricing_provider(provider.as_deref(), &model);
            let speed = speed
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "standard".to_owned());
            let mut tokens = stax_etl::pricing::RawTokens::default();
            tokens.set("input", input);
            tokens.set("output", output);
            tokens.set("cache_read", cache_read);
            tokens.set("cache_creation", cache_create);
            // `compute_cost(...) if model else None` — an empty model costs 0.0
            // and never reaches the pricer.
            let usd = if model.is_empty() {
                0.0
            } else {
                engine
                    .compute_cost(&tokens, &model, &pricing_provider, &speed, None)
                    .total_cost
            };
            *cost_by_pid.entry(pid).or_insert(0.0) += usd;
        }
    }
    Ok(cost_by_pid)
}

// ── stats shaping ────────────────────────────────────────────────────────────

/// One part of a merge — the intermediate `_stats_for_ids` sums over, and the
/// shape every producer of the `ProjectStats` UI dict fills in.
#[derive(Debug, Clone, Default)]
struct StatsPart {
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cache_read: i64,
    total_cache_write: i64,
    total_commands: Option<i64>,
    first_message_date: Option<String>,
    last_message_date: Option<String>,
    total_cost: f64,
}

impl StatsPart {
    /// The `ProjectStats` UI shape, in the key order every producer writes it.
    ///
    /// Written key by key rather than derived: the order IS the byte contract,
    /// and a `Serialize` derive would put it one field reorder away from a
    /// silent divergence nothing in the type system would catch.
    fn into_value(self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "total_input_tokens".to_owned(),
            Value::from(self.total_input_tokens),
        );
        obj.insert(
            "total_output_tokens".to_owned(),
            Value::from(self.total_output_tokens),
        );
        obj.insert(
            "total_cache_read".to_owned(),
            Value::from(self.total_cache_read),
        );
        obj.insert(
            "total_cache_write".to_owned(),
            Value::from(self.total_cache_write),
        );
        obj.insert(
            "total_commands".to_owned(),
            self.total_commands.map_or(Value::Null, Value::from),
        );
        // Aggregator-only fields: integer zeros, matching every producer.
        obj.insert("avg_tokens_per_command".to_owned(), Value::from(0));
        obj.insert("avg_steps_per_command".to_owned(), Value::from(0));
        obj.insert("compact_summary_count".to_owned(), Value::from(0));
        obj.insert(
            "first_message_date".to_owned(),
            self.first_message_date.map_or(Value::Null, Value::from),
        );
        obj.insert(
            "last_message_date".to_owned(),
            self.last_message_date.map_or(Value::Null, Value::from),
        );
        obj.insert("total_cost".to_owned(), Value::from(self.total_cost));
        Value::Object(obj)
    }
}

/// `_mart_row_to_stats`.
fn mart_row_to_stats(row: &MartRow) -> StatsPart {
    StatsPart {
        total_input_tokens: row.total_input_tokens,
        total_output_tokens: row.total_output_tokens,
        total_cache_read: row.total_cache_read,
        total_cache_write: row.total_cache_create,
        total_commands: Some(row.total_commands),
        first_message_date: row.first_ts.clone(),
        last_message_date: row.last_ts.clone(),
        total_cost: row.total_cost_usd,
    }
}

/// `_bulk_lite_merge`.
fn bulk_lite_merge(
    project_ids: &[i64],
    lite_stats: &HashMap<i64, LiteStats>,
    cost_by_pid: &HashMap<i64, f64>,
) -> StatsPart {
    let parts: Vec<&LiteStats> = project_ids
        .iter()
        .filter_map(|id| lite_stats.get(id))
        .collect();
    if parts.is_empty() {
        // The zero shape: note `total_commands` is 0, NOT None.
        return StatsPart {
            total_commands: Some(0),
            ..StatsPart::default()
        };
    }
    StatsPart {
        total_input_tokens: parts.iter().map(|p| p.total_input_tokens).sum(),
        total_output_tokens: parts.iter().map(|p| p.total_output_tokens).sum(),
        total_cache_read: parts.iter().map(|p| p.total_cache_read).sum(),
        total_cache_write: parts.iter().map(|p| p.total_cache_write).sum(),
        total_commands: Some(parts.iter().map(|p| p.total_commands).sum()),
        first_message_date: min_present(parts.iter().map(|p| p.first_message_date.clone())),
        last_message_date: max_present(parts.iter().map(|p| p.last_message_date.clone())),
        // `sum(cost_by_pid.get(pid, 0.0) for pid in project_ids)` — over the
        // FULL id list, not just the ids that had lite rows.
        total_cost: project_ids
            .iter()
            .map(|id| cost_by_pid.get(id).copied().unwrap_or(0.0))
            .sum(),
    }
}

/// `[x for x in xs if x]` then `min(...)` — Python's truthiness drops `""` too.
fn min_present(values: impl Iterator<Item = Option<String>>) -> Option<String> {
    values.flatten().filter(|s| !s.is_empty()).min()
}

fn max_present(values: impl Iterator<Item = Option<String>>) -> Option<String> {
    values.flatten().filter(|s| !s.is_empty()).max()
}

/// `_stats_for_ids` — mart-first, bulk-SQL fallback, merged across
/// provider-duplicates of one slug.
fn stats_for_ids(
    project_ids: &[i64],
    mart_rows: &HashMap<i64, MartRow>,
    lite_stats: &HashMap<i64, LiteStats>,
    cost_by_pid: &HashMap<i64, f64>,
) -> Value {
    let pre_mart_ids: Vec<i64> = project_ids
        .iter()
        .copied()
        .filter(|id| !mart_rows.contains_key(id))
        .collect();
    let mart_present_ids: Vec<i64> = project_ids
        .iter()
        .copied()
        .filter(|id| mart_rows.contains_key(id))
        .collect();

    if mart_present_ids.is_empty() {
        return bulk_lite_merge(&pre_mart_ids, lite_stats, cost_by_pid).into_value();
    }

    let mut parts: Vec<StatsPart> = mart_present_ids
        .iter()
        .map(|id| mart_row_to_stats(&mart_rows[id]))
        .collect();
    if !pre_mart_ids.is_empty() {
        parts.push(bulk_lite_merge(&pre_mart_ids, lite_stats, cost_by_pid));
    }
    if parts.len() == 1 {
        return parts.remove(0).into_value();
    }

    // `_opt_sum_commands`: sum the known ones; all-None merges to None.
    let known: Vec<i64> = parts.iter().filter_map(|p| p.total_commands).collect();
    let total_commands = (!known.is_empty()).then(|| known.iter().sum());

    StatsPart {
        total_input_tokens: parts.iter().map(|p| p.total_input_tokens).sum(),
        total_output_tokens: parts.iter().map(|p| p.total_output_tokens).sum(),
        total_cache_read: parts.iter().map(|p| p.total_cache_read).sum(),
        total_cache_write: parts.iter().map(|p| p.total_cache_write).sum(),
        total_commands,
        first_message_date: min_present(parts.iter().map(|p| p.first_message_date.clone())),
        last_message_date: max_present(parts.iter().map(|p| p.last_message_date.clone())),
        // Summed in part order — Python's `sum(...)` over the same sequence.
        total_cost: parts.iter().map(|p| p.total_cost).sum(),
    }
    .into_value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamping_matches_the_documented_bounds() {
        assert_eq!(clamp_pagination(None, 0), (PROJECTS_DEFAULT_LIMIT, 0));
        assert_eq!(clamp_pagination(Some(0), 0), (1, 0));
        assert_eq!(clamp_pagination(Some(-5), -7), (1, 0));
        assert_eq!(clamp_pagination(Some(999_999), 3), (PROJECTS_MAX_LIMIT, 3));
        assert_eq!(clamp_pagination(Some(20), 40), (20, 40));
    }

    #[test]
    fn worktree_slugs_take_the_leftmost_marker() {
        assert_eq!(
            is_worktree_slug("-home-u-repo--worktrees-feature"),
            Some("-home-u-repo")
        );
        assert_eq!(
            is_worktree_slug("-home-u-repo--claude-worktrees-x--worktrees-y"),
            Some("-home-u-repo")
        );
        // A single dash before `worktrees` is a genuine directory name.
        assert_eq!(is_worktree_slug("-Users-x-worktrees-app"), None);
        // A marker with an empty tail does not match.
        assert_eq!(is_worktree_slug("-repo--worktrees-"), None);
        // A leading marker has no parent.
        assert_eq!(is_worktree_slug("--worktrees-x"), None);
        assert_eq!(is_worktree_slug(""), None);
    }

    #[test]
    fn provider_filters_lowercase_and_drop_empties() {
        let filter = normalise_provider_filter(Some(&[
            "Cursor".to_owned(),
            "  ".to_owned(),
            "CLINE".to_owned(),
        ]))
        .expect("some");
        assert_eq!(filter.len(), 2);
        assert!(filter.contains("cursor") && filter.contains("cline"));
        assert!(normalise_provider_filter(Some(&["".to_owned()])).is_none());
        assert!(normalise_provider_filter(None).is_none());
    }

    #[test]
    fn slug_decoding_matches_the_inline_python() {
        assert_eq!(decode_slug("-home-u-repo"), "home/u/repo");
        assert_eq!(decode_slug("plain"), "plain");
    }

    #[test]
    fn legacy_log_dir_is_claude_only() {
        let root = PathBuf::from("/home/u/.claude/projects");
        assert_eq!(
            resolve_legacy_log_dir(Some("claude"), None, "-a-b", Some(&root)),
            "/home/u/.claude/projects/-a-b"
        );
        assert_eq!(
            resolve_legacy_log_dir(Some("anthropic"), None, "-a-b", Some(&root)),
            "/home/u/.claude/projects/-a-b"
        );
        // A stored path always wins, whatever the provider.
        assert_eq!(
            resolve_legacy_log_dir(Some("codex"), Some("/x/y"), "-a-b", Some(&root)),
            "/x/y"
        );
        // A non-claude provider with no stored path is "unknown", never cwd.
        assert_eq!(
            resolve_legacy_log_dir(Some("codex"), None, "-a-b", Some(&root)),
            ""
        );
        // `provider or "claude"` — None and "" both mean claude.
        assert_eq!(
            resolve_legacy_log_dir(None, None, "-a-b", Some(&root)),
            "/home/u/.claude/projects/-a-b"
        );
    }

    #[test]
    fn a_placeholder_mart_row_is_records_without_messages() {
        let seed = MartRow {
            project_id: 1,
            first_ts: None,
            last_ts: None,
            total_messages: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            total_cost_usd: 0.0,
            total_commands: 0,
            total_records: 12,
        };
        assert!(mart_row_is_placeholder(&seed));
        // A genuinely empty project is NOT a placeholder — the mart answers
        // for it with zeros and it must not be routed to the fallback.
        assert!(!mart_row_is_placeholder(&MartRow {
            total_records: 0,
            ..seed.clone()
        }));
        // A covered project is not a placeholder either.
        assert!(!mart_row_is_placeholder(&MartRow {
            total_messages: 5,
            ..seed
        }));
    }

    #[test]
    fn the_zero_stats_shape_reports_zero_commands_not_null() {
        let value = bulk_lite_merge(&[7], &HashMap::new(), &HashMap::new()).into_value();
        assert_eq!(
            stax_memory::pyjson::dumps_http(&value),
            r#"{"total_input_tokens":0,"total_output_tokens":0,"total_cache_read":0,"total_cache_write":0,"total_commands":0,"avg_tokens_per_command":0,"avg_steps_per_command":0,"compact_summary_count":0,"first_message_date":null,"last_message_date":null,"total_cost":0.0}"#
        );
    }

    #[test]
    fn lite_cost_sums_over_every_id_not_only_the_ones_with_rows() {
        // `sum(cost_by_pid.get(pid, 0.0) for pid in project_ids)` iterates the
        // FULL id list. Summing only the ids that produced a lite row would
        // silently drop a priced-but-message-less provider duplicate.
        let mut lite = HashMap::new();
        lite.insert(1, LiteStats::default());
        let mut cost = HashMap::new();
        cost.insert(1, 1.5);
        cost.insert(2, 2.25);
        let merged = bulk_lite_merge(&[1, 2], &lite, &cost);
        assert!((merged.total_cost - 3.75).abs() < f64::EPSILON);
    }

    #[test]
    fn the_scope_is_a_list_subquery_not_a_join_predicate() {
        // §6b: 912ms vs 9ms. If this assertion ever fails, the endpoint has
        // been "simplified" back into the July hang.
        let clause = scope_clause(3);
        assert!(clause.contains("m.session_fk IN (SELECT id FROM sessions WHERE project_id IN ("));
        assert!(!clause.contains("s.project_id IN"));
        assert_eq!(clause.matches('?').count(), 3);
    }

    #[test]
    fn id_chunks_are_deduped_sorted_and_bounded() {
        let chunks = id_chunks(&[5, 1, 5, 3]);
        assert_eq!(chunks, vec![vec![1, 3, 5]]);
        let many: Vec<i64> = (0..2000).collect();
        let chunks = id_chunks(&many);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), MAX_IN_PARAMS);
    }

    #[test]
    fn rounding_is_pythons_not_f64_round() {
        // `round(0.125, 2)` is 0.12 in CPython (ties to even on the decimal
        // expansion); `(0.125 * 100.0).round() / 100.0` is 0.13.
        assert!((round_half_even(0.125, 2) - 0.12).abs() < 1e-12);
        assert!((round_half_even(2.675, 2) - 2.67).abs() < 1e-12);
        assert!((round_half_even(1.005, 2) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn merged_provider_duplicates_take_the_first_maximum() {
        // Python's `max(group, key=...)` returns the first maximal element.
        // `Iterator::max_by_key` returns the last, which would pick a
        // different `display_name` / `provider` for a tied pair.
        let rows = [
            ProjectRow {
                id: 1,
                provider: "claude".to_owned(),
                slug: "s".to_owned(),
                path: None,
                display_name: "first".to_owned(),
                first_seen: 1.0,
                last_modified: 9.0,
            },
            ProjectRow {
                id: 2,
                provider: "codex".to_owned(),
                slug: "s".to_owned(),
                path: None,
                display_name: "second".to_owned(),
                first_seen: 2.0,
                last_modified: 9.0,
            },
        ];
        let primary = rows
            .iter()
            .reduce(|best, row| {
                if row.last_modified > best.last_modified {
                    row
                } else {
                    best
                }
            })
            .expect("non-empty");
        assert_eq!(primary.display_name, "first");
    }

    #[test]
    fn folding_refuses_chains_and_unlisted_parents() {
        let mut projects = vec![
            assembled("-repo"),
            assembled("-repo--worktrees-a"),
            assembled("-orphan--worktrees-b"),
        ];
        let folded = fold_worktree_fragments(&mut projects, &HashMap::new());
        let kept: Vec<&str> = projects.iter().map(|p| p.dir_name.as_str()).collect();
        // The orphan's parent is not listed, so it stays visible.
        assert_eq!(kept, vec!["-repo", "-orphan--worktrees-b"]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].0, "-repo");
        assert_eq!(folded[0].1.len(), 1);
    }

    fn assembled(slug: &str) -> AssembledProject {
        AssembledProject {
            dir_name: slug.to_owned(),
            log_path: String::new(),
            file_count: 1,
            total_size_mb: 0.0,
            last_modified: 0.0,
            first_seen: 0.0,
            display_name: slug.to_owned(),
            provider: "claude".to_owned(),
            providers: vec!["claude".to_owned()],
            ids: vec![1],
            worktree_of: None,
            worktree_rollup: None,
            stats: None,
        }
    }
}

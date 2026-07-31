//! `reports/optimize.py` — the waste-detection sweep behind `GET /api/optimize`.
//!
//! | Item | Python | Reached from |
//! |---|---|---|
//! | [`find_waste`] | same | `"waste"` — the legacy looped-Q&A view |
//! | [`find_patterns`] | same | `"patterns"` — the seven structural detectors |
//! | [`find_claudemd_bloat`] | same | `/api/optimize/prescriptions` |
//! | [`Finding`] | `@dataclass(frozen=True)` | every element of `"patterns"` |
//! | [`approx_tokens`] / [`tokens_to_usd`] | same | shared with `reports/prescribe.py` |
//! | [`round_half_even`] | CPython's `round(x, n)` | this module, `anomaly`, `mart_queries` |
//!
//! # The shape of the sweep
//!
//! Seven detectors run in a fixed order and each returns zero or one
//! [`Finding`]. Three are filesystem-based and ignore the time scope entirely
//! (CLAUDE.md bloat, unused MCP servers, ghost agents); four read the store.
//! Each store-backed detector has **two implementations** — a
//! `message_tool_mart` fast path and a raw-`messages` fallback — plus, in
//! between, a `tool_mart` pre-flight that can short-circuit the fallback
//! without scanning anything. All three legs are ported: the marts are
//! populated on the maintainer's store, and empty on a fresh install, and the
//! contract is that both answer the same.
//!
//! # What a careless port gets wrong
//!
//! * **The dataclass field order is not the constructor's.** Every
//!   construction site passes `estimated_waste_tokens=` before
//!   `suggested_fix=`, but `asdict()` follows the *declaration*, so the JSON is
//!   `… affected_count, suggested_fix, estimated_waste_tokens,
//!   estimated_waste_usd, details`. [`Finding::to_dict`] writes that order.
//! * **`_approx_tokens` counts CODE POINTS** (`len(text) // 4` over a `str`),
//!   while the POST endpoint's 413 guard counts BYTES. Both are reproduced —
//!   DIV-117.
//! * **`Path.home()` and `Path.cwd()` are read raw** by the MCP-registry and
//!   agent scans, so `CLAUDE_CONFIG_DIR` moves the CLAUDE.md discovery and not
//!   those two, and `ghost_agents` depends on the server's working directory.
//!   DIV-116; bug-for-bug.
//! * **`iterdir()` is readdir order and the sort that follows is stable,** so
//!   two CLAUDE.md files that tie on token count come out in filesystem order.
//!   DIV-115.
//! * **Pricing goes through the injected engine** (LAW 2). `_tokens_to_usd`
//!   calls `compute_cost` as a black box at a single representative model, so a
//!   `default_engine()` here would mis-price by the manifest-vs-price-book gap
//!   DIV-056 measured at 2%.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;

use super::mart_queries::{self, ToolCountColumn};
use super::scope::Scope;

// ── tunables ─────────────────────────────────────────────────────────────────

/// `CLAUDE_MD_TOKEN_THRESHOLD = 5_000` — ≈ 4 chars per token (rough).
pub const CLAUDE_MD_TOKEN_THRESHOLD: i64 = 5_000;
/// `JUNK_READ_REPEAT_THRESHOLD = 5` — same path Read >= N times.
const JUNK_READ_REPEAT_THRESHOLD: i64 = 5;
/// `LOW_READ_EDIT_READ_FLOOR = 20` — Reads >= N to qualify.
const LOW_READ_EDIT_READ_FLOOR: i64 = 20;
/// `CACHE_OVERHEAD_RATIO = 0.5` — `cache_create / total_input`.
const CACHE_OVERHEAD_RATIO: f64 = 0.5;
/// `int(CACHE_OVERHEAD_RATIO * 100)` — truncation, not rounding, and a
/// compile-time constant here because a float→int cast is a clippy warning for
/// a value that cannot change.
const CACHE_OVERHEAD_PERCENT: i64 = 50;
/// `BASH_OUTPUT_BYTES_THRESHOLD = 50_000` — 50 KB output.
const BASH_OUTPUT_BYTES_THRESHOLD: i64 = 50_000;
/// `UNUSED_TOOL_LOOKBACK_DAYS = 30`.
const UNUSED_TOOL_LOOKBACK_DAYS: i64 = 30;

/// `WASTE_PRICING_MODEL = "claude-sonnet-4-6"`.
///
/// The detectors estimate *tokens* wasted but do not carry the model that
/// produced them, so the dollar figure is priced at one mid-tier rate to be a
/// stable comparable lower bound rather than a per-model exact.
pub const WASTE_PRICING_MODEL: &str = "claude-sonnet-4-6";

/// `_SEVERITY_ORDER` — the sort key `find_patterns` ranks on.
fn severity_order(severity: &str) -> i64 {
    match severity {
        "high" => 0,
        "medium" => 1,
        "low" => 2,
        // `.get(f.severity, 99)` — unreachable, every detector emits one of the
        // three, and reproduced anyway because the default is the contract.
        _ => 99,
    }
}

// ── numeric helpers ──────────────────────────────────────────────────────────

/// CPython's `round(x, digits)` — banker's rounding on the DECIMAL value.
///
/// `round()` goes through `_Py_dg_dtoa`, i.e. correct decimal rounding with ties
/// to even; `(x * 1e4).round() / 1e4` differs from it both on exact ties and on
/// values whose binary representation straddles the midpoint. Formatting
/// through Rust's `{:.n}` (which also rounds the decimal expansion half-to-even)
/// and re-parsing gets the same answer.
///
/// FLAGGED FOR DEDUP: `routes/projects.rs` and `routes/pricing.rs` each carry a
/// private copy of this function (DIV-119's neighbour). Neither file belongs to
/// this batch.
#[must_use]
pub fn round_half_even(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// `format(n, ",")` — thousands separators on a non-negative integer.
fn grouped_int(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `_approx_tokens(text)` — `max(0, len(text) // 4)`, over CODE POINTS.
///
/// Python's `len()` on a `str` counts code points, so a document full of
/// em-dashes and box-drawing characters estimates lower than its byte length
/// would suggest. DIV-117.
#[must_use]
pub fn approx_tokens(text: &str) -> i64 {
    // `//` is floor division; the count is non-negative so `max(0, …)` is a
    // no-op, reproduced for the reader rather than for the arithmetic.
    i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX)
}

/// Which slot of the cost breakdown a token estimate maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasteKind {
    /// `"input"` — context/read tokens. Bloat, junk re-reads, bash output.
    Input,
    /// `"cache_creation"` — the cache-overhead detector's budget; prices higher.
    CacheCreation,
}

/// `_tokens_to_usd(tokens, kind=…)` — dollar value at [`WASTE_PRICING_MODEL`].
///
/// `None` when there is nothing to price (mirroring
/// `estimated_waste_tokens is None`). Python additionally returns `0.0` on any
/// pricing exception so a detector never raises because a rate failed to
/// resolve; the Rust engine's `compute_cost` is infallible, so that leg is
/// unreachable here and is not written.
///
/// LAW 2: the engine is INJECTED. `default_engine()` in a server path prices
/// from the in-code manifest while a running server prices from the primed
/// `price_book` table, and the two disagree by ~2% on a backfilled store
/// (DIV-056) — a gap no test on an unprimed store can see.
#[must_use]
pub fn tokens_to_usd(engine: &PricingEngine, tokens: Option<i64>, kind: WasteKind) -> Option<f64> {
    // `if not tokens or tokens <= 0: return None` — 0 and None both fall out.
    let tokens = tokens?;
    if tokens <= 0 {
        return None;
    }
    let raw = match kind {
        WasteKind::Input => RawTokens::canonical(tokens, 0, 0, 0),
        WasteKind::CacheCreation => RawTokens::canonical(0, 0, tokens, 0),
    };
    // `compute_cost(token_arg, WASTE_PRICING_MODEL)` — provider defaults to
    // "anthropic", speed to "standard", at_ts to None.
    let breakdown = engine.compute_cost(&raw, WASTE_PRICING_MODEL, "anthropic", "standard", None);
    Some(round_half_even(breakdown.total_cost, 4))
}

// ── the filesystem roots the detectors read ──────────────────────────────────

/// The four directory roots `reports/optimize.py` reaches for, injected.
///
/// Python reads them from `Path.home()` / `Path.cwd()` / the Claude adapter at
/// call time. The campaign's injection law makes them a parameter instead, so a
/// test can point the whole sweep at a fixture tree without mutating the
/// environment — which a `forbid(unsafe_code)` crate cannot do in Rust 2024
/// anyway.
///
/// The split between `claude_home` and `home` is NOT cosmetic: the CLAUDE.md
/// scan honours `CLAUDE_CONFIG_DIR` and the MCP/agent scans do not. DIV-116.
#[derive(Debug, Clone)]
pub struct FsRoots {
    /// `adapters.claude.claude_home()` — `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_home: PathBuf,
    /// `adapters.claude.default_projects_root()` — `claude_home / "projects"`.
    pub projects_root: PathBuf,
    /// `Path.home()`, raw.
    pub home: PathBuf,
    /// `Path.cwd()` — the SERVER PROCESS's working directory.
    pub cwd: PathBuf,
}

impl FsRoots {
    /// Resolve the four roots from the live environment.
    ///
    /// Mirrors `routes/projects.rs::claude_projects_root`, including the `~`
    /// expansion `Path(env).expanduser()` performs on `CLAUDE_CONFIG_DIR`.
    #[must_use]
    pub fn from_env() -> Self {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        // `os.environ.get("CLAUDE_CONFIG_DIR", "").strip()` — whitespace-only
        // is falsy after the strip and falls back to `~/.claude`.
        let claude_home = match std::env::var("CLAUDE_CONFIG_DIR") {
            Ok(raw) if !raw.trim().is_empty() => expand_user(raw.trim(), &home),
            _ => home.join(".claude"),
        };
        Self {
            projects_root: claude_home.join("projects"),
            claude_home,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            home,
        }
    }
}

/// `Path(raw).expanduser()`, for the leading-`~` forms it actually handles.
fn expand_user(raw: &str, home: &Path) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

// ── Finding ──────────────────────────────────────────────────────────────────

/// One waste-pattern hit.
///
/// `estimated_waste_usd` is `estimated_waste_tokens` priced through the cost
/// engine, and is `None` when a detector has no token estimate to price
/// (`unused_mcp_servers`, `ghost_agents` — a per-request context tax the module
/// does not quantify in dollars).
#[derive(Debug, Clone)]
pub struct Finding {
    /// `pattern_id`.
    pub pattern_id: &'static str,
    /// `"high"` / `"medium"` / `"low"`.
    pub severity: &'static str,
    /// `title`.
    pub title: String,
    /// `description`.
    pub description: String,
    /// `affected_count`.
    pub affected_count: i64,
    /// `suggested_fix`.
    pub suggested_fix: &'static str,
    /// `estimated_waste_tokens`.
    pub estimated_waste_tokens: Option<i64>,
    /// `estimated_waste_usd`.
    pub estimated_waste_usd: Option<f64>,
    /// `details`.
    pub details: Value,
}

impl Finding {
    /// `asdict(self)` — the nine keys in DECLARATION order, which is not the
    /// order the constructors pass them in.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("pattern_id".to_owned(), Value::from(self.pattern_id));
        obj.insert("severity".to_owned(), Value::from(self.severity));
        obj.insert("title".to_owned(), Value::from(self.title.clone()));
        obj.insert(
            "description".to_owned(),
            Value::from(self.description.clone()),
        );
        obj.insert(
            "affected_count".to_owned(),
            Value::from(self.affected_count),
        );
        obj.insert("suggested_fix".to_owned(), Value::from(self.suggested_fix));
        obj.insert(
            "estimated_waste_tokens".to_owned(),
            self.estimated_waste_tokens.map_or(Value::Null, Value::from),
        );
        obj.insert(
            "estimated_waste_usd".to_owned(),
            self.estimated_waste_usd.map_or(Value::Null, |usd| {
                serde_json::Number::from_f64(usd).map_or(Value::Null, Value::Number)
            }),
        );
        obj.insert("details".to_owned(), self.details.clone());
        Value::Object(obj)
    }
}

// ── legacy: find_waste ───────────────────────────────────────────────────────

/// One `"waste"` row: `{project, looped_pairs, sample_questions}`.
fn waste_row(project: &str, looped_pairs: i64, samples: Vec<String>) -> Value {
    let mut obj = Map::new();
    obj.insert("project".to_owned(), Value::from(project));
    obj.insert("looped_pairs".to_owned(), Value::from(looped_pairs));
    obj.insert(
        "sample_questions".to_owned(),
        Value::Array(samples.into_iter().map(Value::from).collect()),
    );
    Value::Object(obj)
}

/// `find_waste(conn, scope=…, include=…, exclude=…)`.
///
/// Ranks projects by the number of looped Q&A pairs, dropping projects with
/// none. The Q&A pairs live in a **separate** database (`qa_pairs.db` beside
/// `store.db`), which the port opens read-only and never creates — the same
/// DIV-077 policy `routes/qa.rs` established. A missing file makes every
/// project's total `0`, so the whole block is `[]`.
///
/// `include is not None` / `exclude is not None`, not truthiness: an EMPTY
/// include list filters everything out, where a falsy test would have widened
/// it to everything. Reproduced.
///
/// # Errors
/// Any SQLite error from the project list. Q&A failures are swallowed the way
/// `QAService.list_qa` swallows them (an unreadable pair store is `total: 0`).
pub fn find_waste(
    conn: &Connection,
    qa_db: Option<&Path>,
    scope: &Scope,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> rusqlite::Result<Vec<Value>> {
    // `queries.list_projects(conn)` — `ORDER BY last_modified DESC`.
    let mut stmt = conn.prepare("SELECT slug FROM projects ORDER BY last_modified DESC")?;
    let mut slugs: Vec<String> = stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    if let Some(include) = include {
        slugs.retain(|s| include.contains(s));
    }
    if let Some(exclude) = exclude {
        slugs.retain(|s| !exclude.contains(s));
    }

    let qa = qa_db.and_then(open_qa_readonly);
    let mut rows: Vec<(i64, Value)> = Vec::new();
    for slug in slugs {
        let (total, samples) = match qa.as_ref() {
            Some(qa) => looped_qa(qa, &slug, scope).unwrap_or((0, Vec::new())),
            None => (0, Vec::new()),
        };
        // `if result["total"] == 0: continue`.
        if total == 0 {
            continue;
        }
        rows.push((total, waste_row(&slug, total, samples)));
    }
    // `rows.sort(key=lambda r: r["looped_pairs"], reverse=True)` — stable, so
    // projects with equal counts keep `last_modified DESC` order.
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// `QAService._get_conn`, read-only — never creates the file (DIV-077).
fn open_qa_readonly(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// `QAService.list_qa(project=…, resolution_status="looped", …, per_page=100)`,
/// narrowed to the two things `find_waste` reads: the total and three samples.
///
/// The clause order is `list_qa`'s (project, date_from, date_to, status) so the
/// bound parameters line up with the reference's; no `search`, so the plain
/// `FROM qa_pairs q` branch is the only one reachable.
fn looped_qa(qa: &Connection, slug: &str, scope: &Scope) -> rusqlite::Result<(i64, Vec<String>)> {
    let mut clauses: Vec<&str> = vec!["q.project = ?"];
    let mut params: Vec<SqlValue> = vec![SqlValue::Text(slug.to_owned())];
    if let Some(date_from) = scope.since.as_deref().filter(|v| !v.is_empty()) {
        clauses.push("q.timestamp >= ?");
        params.push(SqlValue::Text(date_from.to_owned()));
    }
    if let Some(date_to) = scope.until.as_deref().filter(|v| !v.is_empty()) {
        clauses.push("q.timestamp <= ?");
        // `if len(date_to) == 10: date_to = f"{date_to}T23:59:59"` — a bare
        // date means end-of-day. Scope bounds are full stamps, so this never
        // fires from here; ported because `list_qa` is the shared contract.
        params.push(SqlValue::Text(if date_to.len() == 10 {
            format!("{date_to}T23:59:59")
        } else {
            date_to.to_owned()
        }));
    }
    clauses.push("q.resolution_status = ?");
    params.push(SqlValue::Text("looped".to_owned()));
    let where_sql = format!("WHERE {}", clauses.join(" AND "));

    let total: i64 = qa.query_row(
        &format!("SELECT COUNT(*) as total FROM qa_pairs q {where_sql}"),
        rusqlite::params_from_iter(params.iter()),
        |row| row.get(0),
    )?;
    if total == 0 {
        return Ok((0, Vec::new()));
    }

    // `per_page=100`, page 1 → `LIMIT 100 OFFSET 0`; only the first three rows
    // are read, but the ORDER BY has to be the reference's for them to be the
    // same three.
    let mut stmt = qa.prepare(&format!(
        "SELECT q.question_text FROM qa_pairs q {where_sql} \
         ORDER BY q.timestamp DESC LIMIT 100 OFFSET 0"
    ))?;
    let samples = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, Option<String>>(0)
        })?
        .take(3)
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|text| char_prefix(text.as_deref().unwrap_or_default(), 120))
        .collect();
    Ok((total, samples))
}

/// `text[:n]` — a CPython `str` slice, so CODE POINTS.
fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// ── detector 1: bloated CLAUDE.md ────────────────────────────────────────────

/// `_candidate_claude_md_paths(project_filter)`.
///
/// The Claude home's own `CLAUDE.md` first, then one per project directory
/// under the projects root — in `iterdir()` (readdir) order, NOT sorted.
/// DIV-115.
fn candidate_claude_md_paths(roots: &FsRoots, project_filter: Option<&[String]>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home_md = roots.claude_home.join("CLAUDE.md");
    if home_md.is_file() {
        out.push(home_md);
    }
    if roots.projects_root.is_dir()
        && let Ok(entries) = std::fs::read_dir(&roots.projects_root)
    {
        for entry in entries.flatten() {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            // `if project_filter is not None and child.name not in project_filter`
            // — `None` means "every project", `[]` means "none of them".
            if let Some(filter) = project_filter {
                let name = child
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !filter.contains(&name) {
                    continue;
                }
            }
            let md = child.join("CLAUDE.md");
            if md.is_file() {
                out.push(md);
            }
        }
    }
    out
}

/// `find_claudemd_bloat` / `_detect_bloated_claude_md` — Pattern 1.
///
/// Public because `/api/optimize/prescriptions` runs JUST this detector: the
/// CLAUDE.md slim preview needs the bloat finding plus its candidate file list
/// without paying for the full message-scanning sweep. Same read-only discovery,
/// no new filesystem surface.
#[must_use]
pub fn find_claudemd_bloat(
    engine: &PricingEngine,
    roots: &FsRoots,
    project_filter: Option<&[String]>,
) -> Vec<Finding> {
    let paths = candidate_claude_md_paths(roots, project_filter);
    let mut bloated: Vec<(PathBuf, i64)> = Vec::new();
    for path in paths {
        // `read_text(encoding="utf-8", errors="replace")` — invalid bytes
        // become U+FFFD rather than raising, and U+FFFD counts as one code
        // point toward the estimate.
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let approx = approx_tokens(&text);
        if approx > CLAUDE_MD_TOKEN_THRESHOLD {
            bloated.push((path, approx));
        }
    }
    if bloated.is_empty() {
        return Vec::new();
    }

    // Stable descending sort — ties keep readdir order (DIV-115).
    bloated.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let biggest = bloated[0].1;

    let sev = if biggest >= 3 * CLAUDE_MD_TOKEN_THRESHOLD {
        "high"
    } else if biggest >= 2 * CLAUDE_MD_TOKEN_THRESHOLD {
        "medium"
    } else {
        "low"
    };

    // `sum(approx - CLAUDE_MD_TOKEN_THRESHOLD for _, approx in bloated)` — an
    // int sum, so no float compensation is involved.
    let waste: i64 = bloated
        .iter()
        .map(|(_, approx)| approx - CLAUDE_MD_TOKEN_THRESHOLD)
        .sum();
    let count = i64::try_from(bloated.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "files".to_owned(),
        Value::Array(
            bloated
                .iter()
                .map(|(path, tokens)| {
                    let mut entry = Map::new();
                    entry.insert(
                        "path".to_owned(),
                        Value::from(path.to_string_lossy().into_owned()),
                    );
                    entry.insert("approx_tokens".to_owned(), Value::from(*tokens));
                    Value::Object(entry)
                })
                .collect(),
        ),
    );
    details.insert(
        "threshold_tokens".to_owned(),
        Value::from(CLAUDE_MD_TOKEN_THRESHOLD),
    );

    vec![Finding {
        pattern_id: "bloated_claude_md",
        severity: sev,
        title: format!("{count} bloated CLAUDE.md file(s)"),
        description: format!(
            "{count} CLAUDE.md file(s) exceed {} tokens and are loaded \
             into every session's context.",
            grouped_int(CLAUDE_MD_TOKEN_THRESHOLD)
        ),
        affected_count: count,
        suggested_fix: "Trim CLAUDE.md to the bare essentials — move long-form notes \
                        to project-local docs and reference them on demand.",
        estimated_waste_tokens: Some(waste),
        estimated_waste_usd: tokens_to_usd(engine, Some(waste), WasteKind::Input),
        details: Value::Object(details),
    }]
}

// ── detector 2: unused MCP servers ───────────────────────────────────────────

/// `_registered_mcp_servers()` — server names from the three config locations.
///
/// **`Path.home()`, not `claude_home()`** — DIV-116. Missing files and parse
/// failures are swallowed; this is best-effort.
fn registered_mcp_servers(roots: &FsRoots) -> Vec<String> {
    let candidates = [
        roots.home.join(".claude.json"),
        roots
            .home
            .join(".config")
            .join("claude-code")
            .join("settings.json"),
        roots.home.join(".claude").join("settings.json"),
    ];
    let mut names: HashSet<String> = HashSet::new();
    for cfg in candidates {
        let Ok(text) = std::fs::read_to_string(&cfg) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(obj) = data.as_object() else {
            continue;
        };
        if let Some(Value::Object(servers)) = obj.get("mcpServers") {
            names.extend(servers.keys().cloned());
        }
    }
    let mut out: Vec<String> = names.into_iter().collect();
    out.sort();
    out
}

/// `re.match(r"^mcp__([^_]+(?:_[^_]+)*?)__", tool_name)` — the server name.
///
/// The capture group cannot contain a double underscore (`[^_]+` separated by
/// single `_`), and the lazy `*?` plus the trailing `__` mean the group is
/// exactly the text before the FIRST `__` after the prefix. Hand-rolled rather
/// than pulling in a regex engine, and unit-tested against that reading.
fn mcp_server_name(tool_name: &str) -> Option<&str> {
    let rest = tool_name.strip_prefix("mcp__")?;
    // `[^_]+` must match at least one non-underscore at the start.
    if rest.starts_with('_') || rest.is_empty() {
        return None;
    }
    let idx = rest.find("__")?;
    if idx == 0 {
        return None;
    }
    Some(&rest[..idx])
}

/// `_recent_tool_names(conn, since_iso=…)` — the empty-`tool_mart` fallback.
///
/// Rolls `tools_json` up into a name→count map. The port only needs the KEY
/// set, so the counts are dropped; `for tool_name in counts` iterates keys.
fn recent_tool_names(conn: &Connection, since_iso: Option<&str>) -> rusqlite::Result<Vec<String>> {
    let mut sql =
        "SELECT tools_json FROM messages WHERE tools_json != '[]' AND tools_json IS NOT NULL"
            .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(since) = since_iso.filter(|v| !v.is_empty()) {
        sql.push_str(" AND timestamp >= ?");
        params.push(SqlValue::Text(since.to_owned()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    // `Counter` preserves first-seen insertion order; the caller only reads the
    // key set, so an insertion-ordered dedupe is enough.
    let mut seen: Vec<String> = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    while let Some(row) = rows.next()? {
        let raw: Option<String> = row.get(0)?;
        let Some(raw) = raw else { continue };
        let Ok(Value::Array(tools)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        for name in tools {
            // `if isinstance(name, str) and name` — non-strings and "" skip.
            if let Value::String(name) = name
                && !name.is_empty()
                && known.insert(name.clone())
            {
                seen.push(name);
            }
        }
    }
    Ok(seen)
}

/// The lookback bound the two registry detectors share.
///
/// ```python
/// if scope is not None and scope.since is not None:
///     since_iso = scope.since
/// else:
///     since_iso = (datetime.now(UTC) - timedelta(days=30)).isoformat()
/// ```
///
/// The fallback is only reachable for `period=all`, whose scope has no `since`
/// — and it reads the clock, so it is the one bound in this module the differ
/// cannot byte-compare. It is a *parameter* here rather than a call, so the
/// caller owns the clock read (see [`lookback_iso`]) and a test can pin it.
fn lookback_since(scope: Option<&Scope>, fallback_since: &str) -> String {
    match scope.and_then(|s| s.since.clone()) {
        Some(since) => since,
        None => fallback_since.to_owned(),
    }
}

/// `(datetime.now(UTC) - timedelta(days=n)).isoformat()`.
///
/// The one clock read in this module. `services/scope.rs` has the same calendar
/// arithmetic behind `Instant`, but its `minus_days` is private and that file
/// belongs to another member of this batch — so the epoch→civil conversion is
/// re-derived here rather than by editing a file this task does not own.
/// FLAGGED FOR DEDUP alongside DIV-119.
///
/// `datetime.isoformat()` prints the fractional part only when the microsecond
/// is non-zero, and a UTC-aware value always renders `+00:00`.
#[must_use]
pub fn lookback_iso(days: i64) -> String {
    let now = std::time::SystemTime::now();
    let (secs, micros) = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => (
            i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
            i64::from(delta.subsec_micros()),
        ),
        // A pre-epoch clock is a broken RTC, not an input; the epoch is a safe
        // lower bound for a "since" filter.
        Err(_) => (0, 0),
    };
    let shifted = secs - days * 86_400;
    let day_count = shifted.div_euclid(86_400);
    let secs_of_day = shifted.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(day_count);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if micros != 0 {
        out.push_str(&format!(".{micros:06}"));
    }
    out.push_str("+00:00");
    out
}

/// Howard Hinnant's `civil_from_days` — the algorithm CPython's `datetime` uses.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// `_detect_unused_mcp_servers` — Pattern 2.
fn detect_unused_mcp_servers(
    conn: &Connection,
    roots: &FsRoots,
    scope: Option<&Scope>,
    fallback_since: &str,
) -> rusqlite::Result<Vec<Finding>> {
    let registered = registered_mcp_servers(roots);
    if registered.is_empty() {
        return Ok(Vec::new());
    }
    let since_iso = lookback_since(scope, fallback_since);

    let mut used_servers: HashSet<String> = HashSet::new();
    if mart_queries::mart_has_tool_rows(conn)? {
        // Mart fast path: distinct `mcp__*` names in window.
        let names = mart_queries::tool_mart_distinct_tool_names_in_window(
            conn,
            Some(&since_iso),
            None,
            Some("mcp__"),
        )?;
        for tool_name in names {
            if let Some(server) = mcp_server_name(&tool_name) {
                used_servers.insert(server.to_owned());
            }
        }
    } else {
        for tool_name in recent_tool_names(conn, Some(&since_iso))? {
            if let Some(server) = mcp_server_name(&tool_name) {
                used_servers.insert(server.to_owned());
            }
        }
    }

    let unused: Vec<String> = registered
        .iter()
        .filter(|s| !used_servers.contains(*s))
        .cloned()
        .collect();
    if unused.is_empty() {
        return Ok(Vec::new());
    }

    let sev = if unused.len() >= 5 {
        "high"
    } else if unused.len() >= 2 {
        "medium"
    } else {
        "low"
    };
    let count = i64::try_from(unused.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "unused_servers".to_owned(),
        Value::Array(unused.into_iter().map(Value::from).collect()),
    );
    details.insert(
        "registered_total".to_owned(),
        Value::from(i64::try_from(registered.len()).unwrap_or(i64::MAX)),
    );
    details.insert(
        "lookback_days".to_owned(),
        Value::from(UNUSED_TOOL_LOOKBACK_DAYS),
    );

    Ok(vec![Finding {
        pattern_id: "unused_mcp_servers",
        severity: sev,
        title: format!("{count} unused MCP server(s)"),
        description: format!(
            "{count} MCP server(s) registered but no tool calls observed in the last \
             {UNUSED_TOOL_LOOKBACK_DAYS} days."
        ),
        affected_count: count,
        suggested_fix: "Remove unused MCP server entries from ~/.claude.json — \
                        every server adds tool definitions to each request's context.",
        // Explicitly `None` — a context tax the module does not price.
        estimated_waste_tokens: None,
        estimated_waste_usd: None,
        details: Value::Object(details),
    }])
}

// ── detector 3: ghost agents ─────────────────────────────────────────────────

/// `_registered_agents()` — `(name, path)` from the user and project agent dirs.
///
/// Both roots are walked in readdir order and appended; the dedupe is
/// `setdefault`, so the FIRST root (`~/.claude/agents`) wins a name collision —
/// note that the docstring says "a project agent shadows a user agent", which is
/// the opposite of what the code does. Bug-for-bug, comment and all.
///
/// The output is `sorted(seen.items())`, i.e. name order.
fn registered_agents(roots: &FsRoots) -> Vec<(String, PathBuf)> {
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for root in [
        roots.home.join(".claude").join("agents"),
        roots.cwd.join(".claude").join("agents"),
    ] {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if !child.is_file() {
                continue;
            }
            // `child.suffix in {".md", ".yml", ".yaml", ".json"}` — case
            // sensitive, and `Path.suffix` is the LAST extension only.
            let ok = child
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .is_some_and(|ext| matches!(ext.as_str(), "md" | "yml" | "yaml" | "json"));
            if !ok {
                continue;
            }
            let Some(stem) = child.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            seen.entry(stem).or_insert(child);
        }
    }
    seen.into_iter().collect()
}

/// `_ghost_agents_finding(agents, ghost)` — shared by all three ghost paths.
fn ghost_agents_finding(engine: &PricingEngine, ghost: &[(String, PathBuf)]) -> Vec<Finding> {
    let _ = engine; // this finding carries no dollar figure
    if ghost.is_empty() {
        return Vec::new();
    }
    // Only two rungs here — there is no "high" for ghost agents.
    let sev = if ghost.len() >= 5 { "medium" } else { "low" };
    let count = i64::try_from(ghost.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "agents".to_owned(),
        Value::Array(
            ghost
                .iter()
                .map(|(name, path)| {
                    let mut entry = Map::new();
                    entry.insert("name".to_owned(), Value::from(name.clone()));
                    entry.insert(
                        "path".to_owned(),
                        Value::from(path.to_string_lossy().into_owned()),
                    );
                    Value::Object(entry)
                })
                .collect(),
        ),
    );
    details.insert(
        "lookback_days".to_owned(),
        Value::from(UNUSED_TOOL_LOOKBACK_DAYS),
    );

    vec![Finding {
        pattern_id: "ghost_agents",
        severity: sev,
        title: format!("{count} ghost agent(s)"),
        description: format!(
            "{count} agent(s) defined under .claude/agents/ but never spawned in the last \
             {UNUSED_TOOL_LOOKBACK_DAYS} days."
        ),
        affected_count: count,
        suggested_fix: "Delete unused agent definitions — every agent adds to the \
                        tool schema each session loads.",
        estimated_waste_tokens: None,
        estimated_waste_usd: None,
        details: Value::Object(details),
    }]
}

/// `_detect_ghost_agents` — Pattern 3, three paths deep.
fn detect_ghost_agents(
    conn: &Connection,
    engine: &PricingEngine,
    roots: &FsRoots,
    scope: Option<&Scope>,
    fallback_since: &str,
) -> rusqlite::Result<Vec<Finding>> {
    let agents = registered_agents(roots);
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    let since_iso = lookback_since(scope, fallback_since);

    // Fast path: the mart carries each Task call's `subagent_type` exactly.
    if mart_queries::mart_has_message_tool_rows(conn)? {
        let invoked = mart_queries::message_tool_invoked_agents(conn, Some(&since_iso), None)?;
        let ghost: Vec<(String, PathBuf)> = agents
            .iter()
            .filter(|(name, _)| !invoked.contains(name))
            .cloned()
            .collect();
        return Ok(ghost_agents_finding(engine, &ghost));
    }

    // Wave-5 short-circuit: `tool_mart` populated and zero Task calls in window
    // means every registered agent is a ghost, with no raw_json scan.
    if mart_queries::mart_has_tool_rows(conn)? {
        let task_calls = mart_queries::tool_call_count_in_window(
            conn,
            &["Task"],
            Some(&since_iso),
            None,
            None,
            ToolCountColumn::EventCount,
        )?;
        if task_calls == 0 {
            return Ok(ghost_agents_finding(engine, &agents));
        }
    }

    // Fallback: substring-match `subagent_type` against raw_json.
    let mut sql = "SELECT raw_json FROM messages WHERE 1=1".to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if !since_iso.is_empty() {
        sql.push_str(" AND timestamp >= ?");
        params.push(SqlValue::Text(since_iso.clone()));
    }
    // The LIKE is appended AFTER the timestamp bind, so the parameter order is
    // (since,) and the LIKE carries no parameter at all.
    sql.push_str(" AND tools_json LIKE '%Task%'");

    let mut invoked: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
        for (name, _) in &agents {
            if raw.contains(&format!("\"subagent_type\":\"{name}\""))
                || raw.contains(&format!("\"subagent_type\": \"{name}\""))
                || raw.contains(&format!("\"agent\":\"{name}\""))
            {
                invoked.insert(name.clone());
            }
        }
    }
    let ghost: Vec<(String, PathBuf)> = agents
        .iter()
        .filter(|(name, _)| !invoked.contains(name))
        .cloned()
        .collect();
    Ok(ghost_agents_finding(engine, &ghost))
}

// ── the raw-messages fallback shared by detectors 4, 5 and 7 ─────────────────

/// One `messages` row as `_iter_session_messages` projects it.
#[derive(Debug, Clone, Default)]
struct MessageRow {
    session_fk: i64,
    seq: i64,
    role: Option<String>,
    tools_json: Option<String>,
    raw_json: Option<String>,
    content_text: Option<String>,
}

/// `_iter_session_messages(conn, scope=…)` — `{session_fk: [row, …]}` by seq.
///
/// Loads every in-scope message into memory. That is what the reference does,
/// and it only runs when `message_tool_mart` is empty — on a materialised store
/// no caller reaches it.
///
/// LAW 5: `messages` is a partitioned VIEW, and this is a plain scan with no
/// join, exactly as written.
fn iter_session_messages(
    conn: &Connection,
    scope: Option<&Scope>,
) -> rusqlite::Result<Vec<(i64, Vec<MessageRow>)>> {
    let mut sql = "SELECT id, session_fk, seq, timestamp, role, \
                          input_tokens, cache_create_tokens, \
                          tools_json, raw_json, content_text \
                   FROM messages WHERE 1=1"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(scope) = scope {
        if let Some(since) = scope.since.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp >= ?");
            params.push(SqlValue::Text(since.to_owned()));
        }
        if let Some(until) = scope.until.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp <= ?");
            params.push(SqlValue::Text(until.to_owned()));
        }
    }
    sql.push_str(" ORDER BY session_fk, seq");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    // `defaultdict(list)` keyed by session_fk — insertion order, which the
    // ORDER BY makes session_fk order.
    let mut order: Vec<i64> = Vec::new();
    let mut grouped: HashMap<i64, Vec<MessageRow>> = HashMap::new();
    while let Some(row) = rows.next()? {
        let entry = MessageRow {
            session_fk: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
            seq: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            role: row.get(4)?,
            tools_json: row.get(7)?,
            raw_json: row.get(8)?,
            content_text: row.get(9)?,
        };
        let key = entry.session_fk;
        grouped.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        grouped.get_mut(&key).expect("just inserted").push(entry);
    }
    Ok(order
        .into_iter()
        .map(|key| {
            let rows = grouped.remove(&key).unwrap_or_default();
            (key, rows)
        })
        .collect())
}

/// `_tool_calls_with_input(raw_json)` — `(tool_name, input)` from `tool_use`
/// blocks under `message.content`.
fn tool_calls_with_input(raw_json: &str) -> Vec<(String, Map<String, Value>)> {
    let Ok(obj) = serde_json::from_str::<Value>(raw_json) else {
        return Vec::new();
    };
    let Some(Value::Object(msg)) = obj.get("message") else {
        return Vec::new();
    };
    let Some(Value::Array(body)) = msg.get("content") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for blk in body {
        let Some(blk) = blk.as_object() else { continue };
        if blk.get("type") != Some(&Value::String("tool_use".to_owned())) {
            continue;
        }
        // `blk.get("name", "")` / `blk.get("input", {})`, then an isinstance
        // pair — a non-string name or non-object input drops the whole block.
        let name = match blk.get("name") {
            Some(Value::String(name)) => name.clone(),
            None => String::new(),
            Some(_) => continue,
        };
        let input = match blk.get("input") {
            Some(Value::Object(map)) => map.clone(),
            None => Map::new(),
            Some(_) => continue,
        };
        out.push((name, input));
    }
    out
}

// ── detector 4: low read:edit ratio ──────────────────────────────────────────

/// `_low_read_edit_finding(bad_sessions)`.
fn low_read_edit_finding(engine: &PricingEngine, bad_sessions: &[Value]) -> Vec<Finding> {
    if bad_sessions.is_empty() {
        return Vec::new();
    }
    let sev = if bad_sessions.len() >= 5 {
        "high"
    } else if bad_sessions.len() >= 2 {
        "medium"
    } else {
        "low"
    };
    // `sum(s["reads"] for s in bad_sessions) * 2_000` — an int sum.
    let est_waste: i64 = bad_sessions
        .iter()
        .map(|s| s.get("reads").and_then(Value::as_i64).unwrap_or(0))
        .sum::<i64>()
        * 2_000;
    let count = i64::try_from(bad_sessions.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "sessions".to_owned(),
        Value::Array(bad_sessions.iter().take(10).cloned().collect()),
    );
    details.insert(
        "read_threshold".to_owned(),
        Value::from(LOW_READ_EDIT_READ_FLOOR),
    );

    vec![Finding {
        pattern_id: "low_read_edit_ratio",
        severity: sev,
        title: format!("{count} exploration-only session(s)"),
        description: format!(
            "{count} session(s) Read {LOW_READ_EDIT_READ_FLOOR}+ files but never wrote or \
             edited code."
        ),
        affected_count: count,
        suggested_fix: "Use targeted search (Grep / Glob) before bulk Read; \
                        or commit a partial edit so the exploration produces output.",
        estimated_waste_tokens: Some(est_waste),
        estimated_waste_usd: tokens_to_usd(engine, Some(est_waste), WasteKind::Input),
        details: Value::Object(details),
    }]
}

/// One `details.sessions` entry: `{session_fk, reads}`.
fn session_reads_entry(session_fk: Value, reads: i64) -> Value {
    let mut obj = Map::new();
    obj.insert("session_fk".to_owned(), session_fk);
    obj.insert("reads".to_owned(), Value::from(reads));
    Value::Object(obj)
}

/// `_detect_low_read_edit_ratio` — Pattern 4.
fn detect_low_read_edit_ratio(
    conn: &Connection,
    engine: &PricingEngine,
    scope: Option<&Scope>,
    project_filter: Option<&[String]>,
) -> rusqlite::Result<Vec<Finding>> {
    if mart_queries::mart_has_message_tool_rows(conn)? {
        let rows = mart_queries::message_tool_read_edit_per_session(
            conn,
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
        )?;
        let bad: Vec<Value> = rows
            .into_iter()
            .filter(|r| r.reads >= LOW_READ_EDIT_READ_FLOOR && r.edits == 0)
            .map(|r| {
                // `session_fk` carries the mart's session_id STRING here and an
                // int on the fallback path. Same key, two types. Inherited.
                session_reads_entry(r.session_id.map_or(Value::Null, Value::from), r.reads)
            })
            .collect();
        return Ok(low_read_edit_finding(engine, &bad));
    }

    if mart_queries::mart_has_tool_rows(conn)? {
        let reads = mart_queries::tool_call_count_in_window(
            conn,
            &["Read"],
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
            ToolCountColumn::EventCount,
        )?;
        if reads < LOW_READ_EDIT_READ_FLOOR {
            return Ok(Vec::new());
        }
    }

    let grouped = iter_session_messages(conn, scope)?;
    let mut bad: Vec<Value> = Vec::new();
    for (session_fk, rows) in grouped {
        let mut reads: i64 = 0;
        let mut edits: i64 = 0;
        for row in rows {
            let Some(raw) = row.tools_json else { continue };
            let Ok(Value::Array(names)) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            for name in names {
                match name.as_str() {
                    Some("Read") => reads += 1,
                    Some("Edit" | "Write" | "MultiEdit" | "NotebookEdit") => edits += 1,
                    _ => {}
                }
            }
        }
        if reads >= LOW_READ_EDIT_READ_FLOOR && edits == 0 {
            bad.push(session_reads_entry(Value::from(session_fk), reads));
        }
    }
    Ok(low_read_edit_finding(engine, &bad))
}

// ── detector 5: junk reads ───────────────────────────────────────────────────

/// `_junk_reads_finding(hits)`.
fn junk_reads_finding(engine: &PricingEngine, hits: &[Value]) -> Vec<Finding> {
    if hits.is_empty() {
        return Vec::new();
    }
    let affected_files: i64 = hits
        .iter()
        .map(|h| i64::try_from(junk_files(h).len()).unwrap_or(0))
        .sum();
    let sev = if affected_files >= 10 {
        "high"
    } else if affected_files >= 3 {
        "medium"
    } else {
        "low"
    };
    // `sum(max(0, f["reads"] - 1) …) * 2_000` — every read after the first.
    let redundant_reads: i64 = hits
        .iter()
        .flat_map(junk_files)
        .map(|f| (f.get("reads").and_then(Value::as_i64).unwrap_or(0) - 1).max(0))
        .sum();
    let est_waste = redundant_reads * 2_000;

    let mut details = Map::new();
    details.insert(
        "sessions".to_owned(),
        Value::Array(hits.iter().take(10).cloned().collect()),
    );
    details.insert(
        "repeat_threshold".to_owned(),
        Value::from(JUNK_READ_REPEAT_THRESHOLD),
    );

    vec![Finding {
        pattern_id: "junk_reads",
        severity: sev,
        title: format!("{affected_files} file(s) re-read excessively"),
        description: format!(
            "{affected_files} file(s) Read {JUNK_READ_REPEAT_THRESHOLD}+ times in a single \
             session — assistant likely forgot prior reads."
        ),
        affected_count: affected_files,
        suggested_fix: "Cache file contents in working memory or use Grep to \
                        search within an already-loaded file.",
        estimated_waste_tokens: Some(est_waste),
        estimated_waste_usd: tokens_to_usd(engine, Some(est_waste), WasteKind::Input),
        details: Value::Object(details),
    }]
}

/// `hit["files"]` — the per-session file list, or `[]` on a malformed hit.
fn junk_files(hit: &Value) -> Vec<Value> {
    hit.get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// One `{path, reads}` file entry.
fn file_reads_entry(path: Value, reads: i64) -> Value {
    let mut obj = Map::new();
    obj.insert("path".to_owned(), path);
    obj.insert("reads".to_owned(), Value::from(reads));
    Value::Object(obj)
}

/// One `{session_fk, files}` hit.
fn junk_hit(session_fk: Value, files: Vec<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("session_fk".to_owned(), session_fk);
    obj.insert("files".to_owned(), Value::Array(files));
    Value::Object(obj)
}

/// `_detect_junk_reads` — Pattern 5.
fn detect_junk_reads(
    conn: &Connection,
    engine: &PricingEngine,
    scope: Option<&Scope>,
    project_filter: Option<&[String]>,
) -> rusqlite::Result<Vec<Finding>> {
    if mart_queries::mart_has_message_tool_rows(conn)? {
        let rows = mart_queries::message_tool_junk_reads(
            conn,
            JUNK_READ_REPEAT_THRESHOLD,
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
        )?;
        // `by_session.setdefault(...).append(...)` — insertion-ordered.
        let mut order: Vec<String> = Vec::new();
        let mut by_session: HashMap<String, Vec<Value>> = HashMap::new();
        for row in rows {
            let sid = row.session_id.clone().unwrap_or_default();
            by_session.entry(sid.clone()).or_insert_with(|| {
                order.push(sid.clone());
                Vec::new()
            });
            by_session
                .get_mut(&sid)
                .expect("just inserted")
                .push(file_reads_entry(
                    row.file_path.map_or(Value::Null, Value::from),
                    row.reads,
                ));
        }
        let hits: Vec<Value> = order
            .into_iter()
            .map(|sid| {
                let mut files = by_session.remove(&sid).unwrap_or_default();
                // `sorted(files, key=lambda f: f["reads"], reverse=True)` — stable.
                files.sort_by(|a, b| {
                    let ra = a.get("reads").and_then(Value::as_i64).unwrap_or(0);
                    let rb = b.get("reads").and_then(Value::as_i64).unwrap_or(0);
                    rb.cmp(&ra)
                });
                junk_hit(Value::from(sid), files)
            })
            .collect();
        return Ok(junk_reads_finding(engine, &hits));
    }

    if mart_queries::mart_has_tool_rows(conn)? {
        // `count_column="calls_total"` here, `event_count` in detector 4 — the
        // non-distinct measure, matching the legacy aggregator's `calls`.
        let reads = mart_queries::tool_call_count_in_window(
            conn,
            &["Read"],
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
            ToolCountColumn::CallsTotal,
        )?;
        if reads == 0 {
            return Ok(Vec::new());
        }
    }

    let grouped = iter_session_messages(conn, scope)?;
    let mut hits: Vec<Value> = Vec::new();
    for (session_fk, rows) in grouped {
        // `Counter` — first-seen insertion order, which the repeats dict and
        // then the stable sort both inherit.
        let mut order: Vec<String> = Vec::new();
        let mut per_path: HashMap<String, i64> = HashMap::new();
        for row in rows {
            for (name, input) in tool_calls_with_input(row.raw_json.as_deref().unwrap_or("")) {
                if name != "Read" {
                    continue;
                }
                // `inp.get("file_path") or inp.get("path") or ""` — a falsy
                // `file_path` falls through to `path`.
                let fp = input
                    .get("file_path")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        input
                            .get("path")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("");
                if fp.is_empty() {
                    continue;
                }
                per_path
                    .entry(fp.to_owned())
                    .and_modify(|n| *n += 1)
                    .or_insert_with(|| {
                        order.push(fp.to_owned());
                        1
                    });
            }
        }
        let mut files: Vec<Value> = order
            .iter()
            .filter_map(|path| {
                let n = per_path.get(path).copied().unwrap_or(0);
                (n >= JUNK_READ_REPEAT_THRESHOLD)
                    .then(|| file_reads_entry(Value::from(path.clone()), n))
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        files.sort_by(|a, b| {
            let ra = a.get("reads").and_then(Value::as_i64).unwrap_or(0);
            let rb = b.get("reads").and_then(Value::as_i64).unwrap_or(0);
            rb.cmp(&ra)
        });
        hits.push(junk_hit(Value::from(session_fk), files));
    }
    Ok(junk_reads_finding(engine, &hits))
}

// ── detector 6: cache overhead ───────────────────────────────────────────────

/// `_cache_overhead_finding(bad)`.
fn cache_overhead_finding(engine: &PricingEngine, bad: &[Value]) -> Vec<Finding> {
    if bad.is_empty() {
        return Vec::new();
    }
    let sev = if bad.len() >= 10 {
        "high"
    } else if bad.len() >= 3 {
        "medium"
    } else {
        "low"
    };
    // `sum(b["cache_create_tokens"] for b in bad) // 2` — FLOOR division.
    let est_waste: i64 = bad
        .iter()
        .map(|b| {
            b.get("cache_create_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        })
        .sum::<i64>()
        .div_euclid(2);
    let count = i64::try_from(bad.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "sessions".to_owned(),
        Value::Array(bad.iter().take(10).cloned().collect()),
    );
    details.insert(
        "ratio_threshold".to_owned(),
        serde_json::Number::from_f64(CACHE_OVERHEAD_RATIO).map_or(Value::Null, Value::Number),
    );

    vec![Finding {
        pattern_id: "cache_overhead",
        severity: sev,
        title: format!("{count} session(s) with cache thrash"),
        description: format!(
            "{count} session(s) where cache_create_tokens exceed \
             {CACHE_OVERHEAD_PERCENT}% of total input — \
             cache is being written but not amortised."
        ),
        affected_count: count,
        suggested_fix: "Bundle related questions into one session so cache writes \
                        amortise; avoid spawning fresh sessions for one-shot tasks.",
        estimated_waste_tokens: Some(est_waste),
        // The ONLY detector that prices as cache-creation rather than input.
        estimated_waste_usd: tokens_to_usd(engine, Some(est_waste), WasteKind::CacheCreation),
        details: Value::Object(details),
    }]
}

/// One `details.sessions` entry for the cache detector.
fn cache_session_entry(session_fk: Value, cache: i64, inp: i64, ratio: f64) -> Value {
    let mut obj = Map::new();
    obj.insert("session_fk".to_owned(), session_fk);
    obj.insert("cache_create_tokens".to_owned(), Value::from(cache));
    obj.insert("input_tokens".to_owned(), Value::from(inp));
    obj.insert(
        "ratio".to_owned(),
        serde_json::Number::from_f64(ratio).map_or(Value::Null, Value::Number),
    );
    Value::Object(obj)
}

/// `_detect_cache_overhead` — Pattern 6.
fn detect_cache_overhead(
    conn: &Connection,
    engine: &PricingEngine,
    scope: Option<&Scope>,
) -> rusqlite::Result<Vec<Finding>> {
    if mart_queries::mart_has_session_rows(conn)? {
        let bad = mart_queries::session_mart_cache_overhead(
            conn,
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            CACHE_OVERHEAD_RATIO,
        )?;
        let entries: Vec<Value> = bad
            .into_iter()
            .map(|row| {
                cache_session_entry(
                    row.session_id.map_or(Value::Null, Value::from),
                    row.cache_create_tokens,
                    row.input_tokens,
                    row.ratio,
                )
            })
            .collect();
        return Ok(cache_overhead_finding(engine, &entries));
    }

    // Aggregator fallback — the GROUP BY over `messages`.
    let mut sql = "SELECT session_fk, \
                          SUM(input_tokens) AS inp, \
                          SUM(cache_create_tokens) AS cache_create \
                   FROM messages WHERE 1=1"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(scope) = scope {
        if let Some(since) = scope.since.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp >= ?");
            params.push(SqlValue::Text(since.to_owned()));
        }
        if let Some(until) = scope.until.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp <= ?");
            params.push(SqlValue::Text(until.to_owned()));
        }
    }
    sql.push_str(" GROUP BY session_fk");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    let mut bad: Vec<Value> = Vec::new();
    while let Some(row) = rows.next()? {
        let session_fk: Option<i64> = row.get(0)?;
        let inp = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
        let cache = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        if inp == 0 || cache == 0 {
            continue;
        }
        let total_input = inp + cache;
        if total_input == 0 {
            continue;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "token counts are far below 2^53; Python does the same float divide"
        )]
        let ratio = cache as f64 / total_input as f64;
        if ratio > CACHE_OVERHEAD_RATIO {
            bad.push(cache_session_entry(
                session_fk.map_or(Value::Null, Value::from),
                cache,
                inp,
                round_half_even(ratio, 3),
            ));
        }
    }
    Ok(cache_overhead_finding(engine, &bad))
}

// ── detector 7: oversized bash output ────────────────────────────────────────

/// `_bash_output_finding(big)`.
fn bash_output_finding(engine: &PricingEngine, big: &[Value]) -> Vec<Finding> {
    if big.is_empty() {
        return Vec::new();
    }
    let sev = if big.len() >= 10 {
        "high"
    } else if big.len() >= 3 {
        "medium"
    } else {
        "low"
    };
    // `sum(b["bytes"] for b in big) // 4` — ~4 chars/token, FLOOR division.
    let est_waste: i64 = big
        .iter()
        .map(|b| b.get("bytes").and_then(Value::as_i64).unwrap_or(0))
        .sum::<i64>()
        .div_euclid(4);
    let count = i64::try_from(big.len()).unwrap_or(i64::MAX);

    let mut details = Map::new();
    details.insert(
        "samples".to_owned(),
        Value::Array(big.iter().take(10).cloned().collect()),
    );
    details.insert(
        "threshold_bytes".to_owned(),
        Value::from(BASH_OUTPUT_BYTES_THRESHOLD),
    );

    vec![Finding {
        pattern_id: "bash_output_limits",
        severity: sev,
        title: format!("{count} oversized bash output(s)"),
        description: format!(
            "{count} Bash tool result(s) exceeded {} KB of output — \
             wasted tokens that head/tail/grep would have avoided.",
            BASH_OUTPUT_BYTES_THRESHOLD.div_euclid(1000)
        ),
        affected_count: count,
        suggested_fix: "Pipe bash output through head/tail/grep/awk; cap with \
                        --limit/--max flags or write to a file and read selectively.",
        estimated_waste_tokens: Some(est_waste),
        estimated_waste_usd: tokens_to_usd(engine, Some(est_waste), WasteKind::Input),
        details: Value::Object(details),
    }]
}

/// One `details.samples` entry: `{session_fk, seq, bytes}`.
fn bash_sample(session_fk: Value, seq: Value, bytes: i64) -> Value {
    let mut obj = Map::new();
    obj.insert("session_fk".to_owned(), session_fk);
    obj.insert("seq".to_owned(), seq);
    obj.insert("bytes".to_owned(), Value::from(bytes));
    Value::Object(obj)
}

/// `_detect_bash_output_limits` — Pattern 7.
fn detect_bash_output_limits(
    conn: &Connection,
    engine: &PricingEngine,
    scope: Option<&Scope>,
    project_filter: Option<&[String]>,
) -> rusqlite::Result<Vec<Finding>> {
    if mart_queries::mart_has_message_tool_rows(conn)? {
        let rows = mart_queries::message_tool_oversized(
            conn,
            "Bash",
            BASH_OUTPUT_BYTES_THRESHOLD,
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
        )?;
        let big: Vec<Value> = rows
            .into_iter()
            .map(|r| {
                // The mart's `message_id` goes out under the key `seq`, because
                // that is the fallback's field name and the contract is stable
                // across sources. It is NOT a sequence number.
                bash_sample(
                    r.session_id.map_or(Value::Null, Value::from),
                    Value::from(r.message_id),
                    r.byte_count,
                )
            })
            .collect();
        return Ok(bash_output_finding(engine, &big));
    }

    if mart_queries::mart_has_tool_rows(conn)? {
        let bash_calls = mart_queries::tool_call_count_in_window(
            conn,
            &["Bash"],
            scope.and_then(|s| s.since.as_deref()),
            scope.and_then(|s| s.until.as_deref()),
            project_filter,
            ToolCountColumn::EventCount,
        )?;
        if bash_calls == 0 {
            return Ok(Vec::new());
        }
    }

    // The two-pass raw scan. Note it does NOT go through
    // `_iter_session_messages`: it keeps one flat, seq-ordered list.
    let mut sql = "SELECT id, session_fk, seq, role, raw_json, content_text \
                   FROM messages WHERE 1=1"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(scope) = scope {
        if let Some(since) = scope.since.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp >= ?");
            params.push(SqlValue::Text(since.to_owned()));
        }
        if let Some(until) = scope.until.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(" AND timestamp <= ?");
            params.push(SqlValue::Text(until.to_owned()));
        }
    }
    sql.push_str(" ORDER BY session_fk, seq");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(MessageRow {
                session_fk: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                seq: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                role: row.get(3)?,
                tools_json: None,
                raw_json: row.get(4)?,
                content_text: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Pass 1: every assistant Bash call's (session_fk, seq).
    let mut bash_call_seqs: HashSet<(i64, i64)> = HashSet::new();
    for row in &rows {
        if row.role.as_deref() != Some("assistant") {
            continue;
        }
        for (name, _input) in tool_calls_with_input(row.raw_json.as_deref().unwrap_or("")) {
            if name == "Bash" {
                bash_call_seqs.insert((row.session_fk, row.seq));
            }
        }
    }

    // Pass 2: user rows over the threshold that follow a Bash call.
    let mut big: Vec<Value> = Vec::new();
    for row in &rows {
        if row.role.as_deref() != Some("user") {
            continue;
        }
        // `len((r["content_text"] or "").encode("utf-8"))` — BYTES here, unlike
        // `_approx_tokens`'s code points.
        let size =
            i64::try_from(row.content_text.as_deref().unwrap_or("").len()).unwrap_or(i64::MAX);
        // `if size < THRESHOLD: continue` — so exactly 50 000 IS counted here
        // and is NOT on the mart path, which uses `byte_count > ?`.
        if size < BASH_OUTPUT_BYTES_THRESHOLD {
            continue;
        }
        let has_prior_bash = bash_call_seqs
            .iter()
            .any(|(sfk, bseq)| *sfk == row.session_fk && *bseq < row.seq);
        if !has_prior_bash {
            continue;
        }
        big.push(bash_sample(
            Value::from(row.session_fk),
            Value::from(row.seq),
            size,
        ));
    }
    Ok(bash_output_finding(engine, &big))
}

// ── orchestrator ─────────────────────────────────────────────────────────────

/// `find_patterns(conn, scope=…, project_filter=…)` — every detector, ranked.
///
/// The order the detectors RUN in is the order they were appended, and the
/// final sort is `(severity_rank, -tokens)` with Python's stable sort, so
/// findings that tie on both keys keep their run order. `project_filter`
/// narrows the CLAUDE.md scan and the three store-backed slug filters; the MCP
/// and agent scans are project-blind.
///
/// # Errors
/// Any SQLite error a detector surfaces. Filesystem errors are swallowed by the
/// detectors themselves — patterns are advisory, never load-bearing.
pub fn find_patterns(
    conn: &Connection,
    engine: &PricingEngine,
    roots: &FsRoots,
    scope: Option<&Scope>,
    project_filter: Option<&[String]>,
    fallback_since: &str,
) -> rusqlite::Result<Vec<Finding>> {
    let mut findings: Vec<Finding> = Vec::new();

    // Filesystem-based detectors — scope-independent except for the lookback.
    findings.extend(find_claudemd_bloat(engine, roots, project_filter));
    findings.extend(detect_unused_mcp_servers(
        conn,
        roots,
        scope,
        fallback_since,
    )?);
    findings.extend(detect_ghost_agents(
        conn,
        engine,
        roots,
        scope,
        fallback_since,
    )?);

    // Message-based detectors.
    findings.extend(detect_low_read_edit_ratio(
        conn,
        engine,
        scope,
        project_filter,
    )?);
    findings.extend(detect_junk_reads(conn, engine, scope, project_filter)?);
    findings.extend(detect_cache_overhead(conn, engine, scope)?);
    findings.extend(detect_bash_output_limits(
        conn,
        engine,
        scope,
        project_filter,
    )?);

    findings.sort_by(|a, b| {
        let ka = (
            severity_order(a.severity),
            -a.estimated_waste_tokens.unwrap_or(0),
        );
        let kb = (
            severity_order(b.severity),
            -b.estimated_waste_tokens.unwrap_or(0),
        );
        ka.cmp(&kb)
    });
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> PricingEngine {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        PricingEngine::from_manifest_path(&crate::pricing::manifest_path(&package))
            .expect("the shipped manifest")
    }

    fn empty_roots(dir: &Path) -> FsRoots {
        FsRoots {
            claude_home: dir.join("claude-home"),
            projects_root: dir.join("claude-home").join("projects"),
            home: dir.join("home"),
            cwd: dir.join("cwd"),
        }
    }

    #[test]
    fn approx_tokens_counts_code_points_not_bytes() {
        // Eight code points, sixteen bytes: Python's len() says 8 → 2 tokens.
        assert_eq!(approx_tokens("————————"), 2);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abc"), 0);
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn tokens_to_usd_is_none_for_nothing_and_a_four_place_float_otherwise() {
        let engine = engine();
        assert_eq!(tokens_to_usd(&engine, None, WasteKind::Input), None);
        assert_eq!(tokens_to_usd(&engine, Some(0), WasteKind::Input), None);
        assert_eq!(tokens_to_usd(&engine, Some(-5), WasteKind::Input), None);
        let input = tokens_to_usd(&engine, Some(1_000_000), WasteKind::Input).expect("priced");
        let cache =
            tokens_to_usd(&engine, Some(1_000_000), WasteKind::CacheCreation).expect("priced");
        // Cache CREATION prices strictly higher than fresh input on every
        // Anthropic rate card — that is why the cache detector uses it.
        assert!(cache > input, "{cache} !> {input}");
        // Four decimal places, exactly.
        assert!((input - round_half_even(input, 4)).abs() < f64::EPSILON);
    }

    #[test]
    fn the_mcp_server_regex_takes_the_text_before_the_first_double_underscore() {
        assert_eq!(mcp_server_name("mcp__slack__send"), Some("slack"));
        assert_eq!(
            mcp_server_name("mcp__claude_ai_Gmail__get"),
            Some("claude_ai_Gmail")
        );
        // A leading underscore after the prefix cannot match `[^_]+`.
        assert_eq!(mcp_server_name("mcp___x__y"), None);
        // No trailing `__` at all.
        assert_eq!(mcp_server_name("mcp__slack"), None);
        assert_eq!(mcp_server_name("Read"), None);
        assert_eq!(mcp_server_name("mcp__"), None);
    }

    #[test]
    fn the_finding_renders_nine_keys_in_declaration_not_construction_order() {
        let finding = Finding {
            pattern_id: "ghost_agents",
            severity: "low",
            title: "1 ghost agent(s)".to_owned(),
            description: "d".to_owned(),
            affected_count: 1,
            suggested_fix: "f",
            estimated_waste_tokens: None,
            estimated_waste_usd: None,
            details: Value::Object(Map::new()),
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&finding.to_dict()),
            r#"{"pattern_id":"ghost_agents","severity":"low","title":"1 ghost agent(s)","description":"d","affected_count":1,"suggested_fix":"f","estimated_waste_tokens":null,"estimated_waste_usd":null,"details":{}}"#
        );
    }

    #[test]
    fn the_bloat_threshold_renders_with_a_thousands_separator() {
        assert_eq!(grouped_int(5_000), "5,000");
        assert_eq!(grouped_int(999), "999");
        assert_eq!(grouped_int(1_234_567), "1,234,567");
        assert_eq!(grouped_int(0), "0");
    }

    #[test]
    fn a_claude_md_under_the_threshold_produces_no_finding_at_all() {
        let dir = tempdir();
        let roots = empty_roots(&dir);
        std::fs::create_dir_all(&roots.claude_home).expect("mkdir");
        std::fs::write(roots.claude_home.join("CLAUDE.md"), "x".repeat(400)).expect("write");
        assert!(find_claudemd_bloat(&engine(), &roots, None).is_empty());
    }

    #[test]
    fn the_bloat_severity_ladder_keys_on_the_biggest_file_not_the_total() {
        let dir = tempdir();
        let roots = empty_roots(&dir);
        std::fs::create_dir_all(roots.projects_root.join("-a")).expect("mkdir");
        std::fs::create_dir_all(roots.projects_root.join("-b")).expect("mkdir");
        // Two files just over the threshold: 24_000 chars → 6_000 tokens each.
        // The TOTAL is 12_000 (over 2×) but the biggest is 6_000 (under 2×),
        // so the severity is "low", not "medium".
        for slug in ["-a", "-b"] {
            std::fs::write(
                roots.projects_root.join(slug).join("CLAUDE.md"),
                "x".repeat(24_000),
            )
            .expect("write");
        }
        let findings = find_claudemd_bloat(&engine(), &roots, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "low");
        assert_eq!(findings[0].affected_count, 2);
        // waste = Σ(approx - threshold) = 2 × 1_000
        assert_eq!(findings[0].estimated_waste_tokens, Some(2_000));
    }

    #[test]
    fn an_empty_project_filter_scans_the_home_file_but_no_project_at_all() {
        let dir = tempdir();
        let roots = empty_roots(&dir);
        std::fs::create_dir_all(roots.projects_root.join("-a")).expect("mkdir");
        std::fs::write(
            roots.projects_root.join("-a").join("CLAUDE.md"),
            "x".repeat(80_000),
        )
        .expect("write");
        // `project_filter is not None and name not in filter` → an EMPTY list
        // excludes every project. The home file is not filtered by it at all,
        // and there is none here, so the sweep finds nothing.
        assert!(find_claudemd_bloat(&engine(), &roots, Some(&[])).is_empty());
        assert_eq!(
            find_claudemd_bloat(&engine(), &roots, Some(&["-a".to_owned()])).len(),
            1
        );
        assert_eq!(find_claudemd_bloat(&engine(), &roots, None).len(), 1);
    }

    #[test]
    fn find_patterns_ranks_high_first_then_by_descending_tokens() {
        let a = Finding {
            pattern_id: "junk_reads",
            severity: "medium",
            title: String::new(),
            description: String::new(),
            affected_count: 0,
            suggested_fix: "",
            estimated_waste_tokens: Some(10),
            estimated_waste_usd: None,
            details: Value::Null,
        };
        let mut findings = [
            Finding {
                severity: "low",
                estimated_waste_tokens: Some(9_999),
                ..a.clone()
            },
            Finding {
                severity: "medium",
                estimated_waste_tokens: Some(10),
                ..a.clone()
            },
            Finding {
                severity: "high",
                estimated_waste_tokens: None,
                ..a.clone()
            },
            Finding {
                severity: "medium",
                estimated_waste_tokens: Some(500),
                ..a.clone()
            },
        ];
        findings.sort_by(|x, y| {
            let kx = (
                severity_order(x.severity),
                -x.estimated_waste_tokens.unwrap_or(0),
            );
            let ky = (
                severity_order(y.severity),
                -y.estimated_waste_tokens.unwrap_or(0),
            );
            kx.cmp(&ky)
        });
        let got: Vec<(&str, Option<i64>)> = findings
            .iter()
            .map(|f| (f.severity, f.estimated_waste_tokens))
            .collect();
        assert_eq!(
            got,
            vec![
                ("high", None),
                ("medium", Some(500)),
                ("medium", Some(10)),
                ("low", Some(9_999)),
            ]
        );
    }

    #[test]
    fn tool_use_blocks_are_read_out_of_message_content_and_nowhere_else() {
        let raw = r#"{"message":{"content":[
            {"type":"text","text":"hi"},
            {"type":"tool_use","name":"Read","input":{"file_path":"/a.rs"}},
            {"type":"tool_use","name":"Bash","input":{"command":"ls"}},
            {"type":"tool_use","name":42,"input":{}}
        ]}}"#;
        let calls = tool_calls_with_input(raw);
        assert_eq!(calls.len(), 2, "a non-string name drops the block");
        assert_eq!(calls[0].0, "Read");
        assert_eq!(calls[0].1["file_path"], Value::from("/a.rs"));
        // A top-level list, a missing `message`, and junk all yield nothing.
        assert!(tool_calls_with_input("[]").is_empty());
        assert!(tool_calls_with_input("{\"content\":[]}").is_empty());
        assert!(tool_calls_with_input("not json").is_empty());
    }

    #[test]
    fn the_cache_detector_prices_as_cache_creation_and_floor_divides_by_two() {
        let engine = engine();
        let bad = vec![
            cache_session_entry(Value::from("s1"), 101, 10, 0.909),
            cache_session_entry(Value::from("s2"), 100, 10, 0.909),
        ];
        let findings = cache_overhead_finding(&engine, &bad);
        assert_eq!(findings.len(), 1);
        // (101 + 100) // 2 == 100, not 100.5
        assert_eq!(findings[0].estimated_waste_tokens, Some(100));
        assert_eq!(findings[0].severity, "low", "2 < 3");
        assert!(
            findings[0]
                .description
                .contains("exceed 50% of total input"),
            "{}",
            findings[0].description
        );
        // Priced on the cache-creation slot, which is dearer than input.
        let as_input = tokens_to_usd(&engine, Some(100), WasteKind::Input);
        assert!(findings[0].estimated_waste_usd > as_input);
    }

    #[test]
    fn the_bash_finding_reports_the_threshold_in_whole_kilobytes() {
        let engine = engine();
        let big: Vec<Value> = (0..3)
            .map(|i| bash_sample(Value::from(1), Value::from(i), 60_000))
            .collect();
        let findings = bash_output_finding(&engine, &big);
        assert_eq!(findings[0].severity, "medium", "3 <= n < 10");
        assert_eq!(findings[0].estimated_waste_tokens, Some(45_000));
        assert!(
            findings[0].description.contains("exceeded 50 KB of output"),
            "{}",
            findings[0].description
        );
    }

    #[test]
    fn find_waste_without_a_pair_store_is_an_empty_list_not_an_error() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, last_modified TEXT);
             INSERT INTO projects VALUES (1, '-a', '2026-07-01'), (2, '-b', '2026-07-02');",
        )
        .expect("schema");
        let scope = super::super::scope::parse_period(
            "all",
            super::super::scope::Instant::from_parts(2026, 7, 31, 0, 0, 0, 0),
        )
        .expect("known spec");
        let waste = find_waste(&conn, None, &scope, None, None).expect("guarded");
        assert!(waste.is_empty());
    }

    #[test]
    fn find_waste_ranks_by_looped_pairs_and_truncates_the_samples() {
        let dir = tempdir();
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, last_modified TEXT);
             INSERT INTO projects VALUES (1, '-a', '2026-07-01'), (2, '-b', '2026-07-02');",
        )
        .expect("schema");

        let qa_path = dir.join("qa_pairs.db");
        {
            let qa = Connection::open(&qa_path).expect("create");
            qa.execute_batch(
                "CREATE TABLE qa_pairs (id TEXT, project TEXT, question_text TEXT,
                                        timestamp TEXT, resolution_status TEXT);",
            )
            .expect("schema");
            let long = "q".repeat(200);
            for i in 0..3 {
                qa.execute(
                    "INSERT INTO qa_pairs VALUES (?, '-a', ?, ?, 'looped')",
                    rusqlite::params![i.to_string(), long, format!("2026-07-0{}", i + 1)],
                )
                .expect("row");
            }
            qa.execute(
                "INSERT INTO qa_pairs VALUES ('x', '-b', 'short', '2026-07-01', 'looped')",
                [],
            )
            .expect("row");
            // A resolved pair is not waste.
            qa.execute(
                "INSERT INTO qa_pairs VALUES ('y', '-b', 'done', '2026-07-01', 'resolved')",
                [],
            )
            .expect("row");
        }

        let scope = super::super::scope::parse_period(
            "all",
            super::super::scope::Instant::from_parts(2026, 7, 31, 0, 0, 0, 0),
        )
        .expect("known spec");
        let waste = find_waste(&conn, Some(&qa_path), &scope, None, None).expect("guarded");
        assert_eq!(waste.len(), 2);
        // -a has 3 looped pairs, -b has 1 → -a first regardless of the
        // `last_modified DESC` project order that put -b first.
        assert_eq!(waste[0]["project"], Value::from("-a"));
        assert_eq!(waste[0]["looped_pairs"], Value::from(3));
        let samples = waste[0]["sample_questions"].as_array().expect("array");
        assert_eq!(samples.len(), 3);
        assert_eq!(
            samples[0].as_str().expect("str").chars().count(),
            120,
            "question_text[:120]"
        );
        assert_eq!(waste[1]["looped_pairs"], Value::from(1));
    }

    #[test]
    fn an_empty_include_list_filters_every_project_out() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, last_modified TEXT);
             INSERT INTO projects VALUES (1, '-a', '2026-07-01');",
        )
        .expect("schema");
        let scope = super::super::scope::parse_period(
            "all",
            super::super::scope::Instant::from_parts(2026, 7, 31, 0, 0, 0, 0),
        )
        .expect("known spec");
        // `if include is not None` — a truthiness test would have widened this
        // to "every project". It does not.
        assert!(
            find_waste(&conn, None, &scope, Some(&[]), None)
                .expect("guarded")
                .is_empty()
        );
    }

    /// A throwaway directory under the crate's target dir — no `tempfile` dep.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("stax-optimize-test-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }
}

//! `reports/forks.py` — fork / sidechain economics over the conversation DAG.
//!
//! | Item | Python | Reached from |
//! |---|---|---|
//! | [`analyze_forks`] | same | `routes/forks.rs` → `GET /api/forks` |
//! | [`TOP_N`] / [`MIN_BRANCH_COST_USD`] | same | the two tunables |
//!
//! # The shape of the sweep
//!
//! One SQL statement loads **every** scoped `messages` row (383 580 on the
//! harness store), prices each assistant row that names a model, and then walks
//! the conversation DAG twice per session: once to count fork points, once to
//! find the branches that were started and dropped. There is no mart — a DAG is
//! not aggregate grain — so the cost is inherent, and the route pays for it with
//! a process-wide memo (`routes/forks.rs`), not with a shortcut here.
//!
//! # What a careless port gets wrong
//!
//! * **`abandoned_cost_usd` is an `int` when no branch qualified.**
//!   `round(sum(b.cost_usd for b in abandoned), 4)` — `sum([])` is the `int` `0`
//!   and `round(0, 4)` is the `int` `0`, so starlette writes `0`. With one or
//!   more branches every term is a `float`. The *empty-store* report is a third
//!   case: it comes from `ForkReport()`'s declared default `0.0`, a float. LAW
//!   3, and [`PyNum`] carries it.
//! * **Three different accumulation shapes in one file.** The totals are `+=`
//!   chains (NOT compensated), `_subtree_stats` is a `+=` chain, and only the
//!   abandoned-cost roll-up is `sum()` (Neumaier-compensated). Matching the
//!   *operation* is the rule; "more accurate" is a divergence.
//! * **The DFS order is load-bearing twice.** `stack.pop()` is LIFO, so the
//!   children of a node are visited last-first, and that order reaches the
//!   answer through (a) the `+=` cost chain, which is not associative in `f64`,
//!   and (b) `ep > last_epoch`, which is strict — so the first-visited of two
//!   equal epochs keeps `last_ts`.
//! * **`session_last` and `session_last_ts` are two different maxima.**
//!   `session_last` is `max` over parsed *epochs*; `session_last_ts` is `max`
//!   over the raw *strings*. Lexicographic order over ISO-8601 agrees with
//!   chronological order only while every stamp shares a SHAPE, and the store's
//!   do not: 373 112 rows are `…SS.fffZ`, 10 468 are `…SS.ffffff+00:00` or
//!   `…SS+00:00`. At the byte where two shapes diverge, `'Z'` (0x5A) beats
//!   `'.'` (0x2E) and `'+'` (0x2B), so a fraction-less `Z` stamp can sort above
//!   one that is genuinely later. The string max can therefore name a message
//!   that is not the latest instant, while `gap_seconds` beside it is measured
//!   against the one that is. Inherited, not fixed.
//! * **`_table_exists` here is the WIDE guard** (`type IN ('table','view')`) —
//!   the store's `messages` is a partitioned routing VIEW, so a `type='table'`
//!   check would read a full store as empty. LAW 7 / DIV-148.
//! * **Pricing goes through the injected engine** (LAW 2). A `default_engine()`
//!   prices from the in-code manifest while the running server prices from the
//!   primed `price_book`, and the two disagree by ~2% (DIV-056) — on the one
//!   module whose whole job is attributing dollars to sidechains.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::stats::aggregator::{Neumaier, PyNum, jf, ji, round_py};

use crate::scope::Scope;

// ── tunables ─────────────────────────────────────────────────────────────────

/// `TOP_N = 10` — the panel shows the worst few, not a wall of every retry.
pub const TOP_N: i64 = 10;

/// `MIN_BRANCH_COST_USD = 0.01` — below this a "dropped branch" is pennies.
pub const MIN_BRANCH_COST_USD: f64 = 0.01;

// ── internal row shape ───────────────────────────────────────────────────────

/// `@dataclass(frozen=True, slots=True) class _Msg`, narrowed to the fields the
/// DAG walk reads.
///
/// `provider`, `role`, `model` and `speed` are consumed during the load — they
/// decide the per-row price and nothing else — so they are not carried.
///
/// `uuid` / `parent_uuid` are `Option`, and an EMPTY string is normalised to
/// `None` at load: every use in Python is `if m.uuid:` / `if pu:` truthiness,
/// which treats `""` and `None` identically, and no falsy value is ever used as
/// a key.
#[derive(Debug, Clone)]
struct Msg {
    session_id: String,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    is_sidechain: bool,
    timestamp: String,
    cost_usd: f64,
    token_total: i64,
}

/// `AbandonedBranch` — one fork branch that was started then dropped.
#[derive(Debug, Clone)]
struct AbandonedBranch {
    session_id: String,
    fork_uuid: String,
    branch_head_uuid: String,
    message_count: i64,
    cost_usd: f64,
    token_total: i64,
    sidechain: bool,
    last_ts: Option<String>,
    session_last_ts: Option<String>,
    gap_seconds: Option<f64>,
    reason: String,
}

impl AbandonedBranch {
    /// `asdict(self)` — the eleven keys in DECLARATION order.
    fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "session_id".to_owned(),
            Value::from(self.session_id.clone()),
        );
        obj.insert("fork_uuid".to_owned(), Value::from(self.fork_uuid.clone()));
        obj.insert(
            "branch_head_uuid".to_owned(),
            Value::from(self.branch_head_uuid.clone()),
        );
        obj.insert("message_count".to_owned(), ji(self.message_count));
        obj.insert("cost_usd".to_owned(), jf(self.cost_usd));
        obj.insert("token_total".to_owned(), ji(self.token_total));
        obj.insert("sidechain".to_owned(), Value::Bool(self.sidechain));
        obj.insert(
            "last_ts".to_owned(),
            self.last_ts.clone().map_or(Value::Null, Value::from),
        );
        obj.insert(
            "session_last_ts".to_owned(),
            self.session_last_ts
                .clone()
                .map_or(Value::Null, Value::from),
        );
        obj.insert(
            "gap_seconds".to_owned(),
            self.gap_seconds.map_or(Value::Null, jf),
        );
        obj.insert("reason".to_owned(), Value::from(self.reason.clone()));
        Value::Object(obj)
    }
}

// ── data sourcing ────────────────────────────────────────────────────────────

/// `_table_exists` — `type IN ('table', 'view')`, the WIDE guard.
///
/// NOT `services::mart_queries::table_exists`, which is `type='table'` on
/// purpose. `store/mart_queries.py::_table_exists` and
/// `reports/forks.py::_table_exists` are genuinely different guards and must not
/// be deduped into one — the same split `DIV-c-optimize.md` recorded for
/// `reports/prescribe.py`. FLAGGED FOR THE INTEGRATOR'S DEDUP LIST: this is now
/// the *third* file-local copy of the wide guard (`routes/projects.rs`,
/// `services/prescribe.rs`, here).
///
/// `except sqlite3.Error: return False` — an unreadable catalogue reads empty.
fn table_or_view_exists(conn: &Connection, name: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
    ) else {
        return false;
    };
    let Ok(mut rows) = stmt.query([name]) else {
        return false;
    };
    matches!(rows.next(), Ok(Some(_)))
}

/// `_load_messages` — scoped rows with a per-row cost, or `[]` when unavailable.
///
/// Cost is charged only for assistant messages that name a model, exactly the
/// rule the by-provider rollup uses, so user turns and tool results contribute
/// tokens and structure but never dollars they did not incur.
fn load_messages(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
    engine: &PricingEngine,
) -> Vec<Msg> {
    if !(table_or_view_exists(conn, "messages") && table_or_view_exists(conn, "sessions")) {
        return Vec::new();
    }
    // `project_ids=None` means "no project filter" (whole store); an *empty*
    // list means "a filter was requested but matched no project" — that must
    // scope to nothing, not silently widen to the whole store.
    if project_ids.is_some_and(<[i64]>::is_empty) {
        return Vec::new();
    }
    // Without `projects` there is no provider column, so everything prices as
    // anthropic (compute_cost's default) rather than returning nothing.
    let have_projects = table_or_view_exists(conn, "projects");
    let provider_select = if have_projects {
        "projects.provider AS provider"
    } else {
        "'anthropic' AS provider"
    };
    let provider_join = if have_projects {
        "JOIN projects ON projects.id = sessions.project_id"
    } else {
        ""
    };

    let mut sql = format!(
        "SELECT sessions.session_id AS session_id, \
                {provider_select}, \
                messages.uuid AS uuid, \
                messages.parent_uuid AS parent_uuid, \
                messages.role AS role, \
                COALESCE(messages.model, '') AS model, \
                COALESCE(messages.speed, 'standard') AS speed, \
                COALESCE(messages.is_sidechain, 0) AS is_sidechain, \
                messages.timestamp AS timestamp, \
                COALESCE(messages.input_tokens, 0) AS input_tokens, \
                COALESCE(messages.output_tokens, 0) AS output_tokens, \
                COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, \
                COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         {provider_join} \
         WHERE 1=1 "
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // `if project_ids:` — truthiness, so an empty list adds no clause (and is
    // already unreachable: the guard above returned).
    if let Some(ids) = project_ids.filter(|ids| !ids.is_empty()) {
        sql.push_str(&format!(
            "AND sessions.project_id IN ({}) ",
            vec!["?"; ids.len()].join(",")
        ));
        for id in ids {
            params.push(Box::new(*id));
        }
    }
    if let Some(since) = scope.and_then(|scope| scope.since.as_ref()) {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(Box::new(since.clone()));
    }
    if let Some(until) = scope.and_then(|scope| scope.until.as_ref()) {
        sql.push_str("AND messages.timestamp <= ? ");
        params.push(Box::new(until.clone()));
    }
    // Deterministic order so subtree "last activity" and fork child ordering are
    // stable; (session, seq) matches the ingest order the DAG was built in.
    sql.push_str("ORDER BY sessions.session_id, messages.seq ");

    // `except sqlite3.Error: return []` — the whole statement, prepare included.
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let Ok(mut rows) = stmt.query(refs.as_slice()) else {
        return Vec::new();
    };

    let mut out: Vec<Msg> = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => match row_to_msg(row, engine) {
                Ok(msg) => out.push(msg),
                Err(_) => return Vec::new(),
            },
            Ok(None) => break,
            Err(_) => return Vec::new(),
        }
    }
    out
}

/// One `_load_messages` row → a priced [`Msg`].
fn row_to_msg(row: &rusqlite::Row<'_>, engine: &PricingEngine) -> rusqlite::Result<Msg> {
    let session_id: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
    let provider: Option<String> = row.get(1)?;
    let uuid: Option<String> = row.get(2)?;
    let parent_uuid: Option<String> = row.get(3)?;
    let role: Option<String> = row.get(4)?;
    let model: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
    let speed: Option<String> = row.get(6)?;
    let is_sidechain: Option<i64> = row.get(7)?;
    let timestamp: Option<String> = row.get(8)?;
    let input_t: i64 = row.get::<_, Option<i64>>(9)?.unwrap_or(0);
    let output_t: i64 = row.get::<_, Option<i64>>(10)?.unwrap_or(0);
    let cc: i64 = row.get::<_, Option<i64>>(11)?.unwrap_or(0);
    let cr: i64 = row.get::<_, Option<i64>>(12)?.unwrap_or(0);

    let token_total = input_t + output_t + cc + cr;
    // `provider = r["provider"] or "anthropic"` — truthiness, so `""` too.
    let provider = provider
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "anthropic".to_owned());
    let speed_value = speed
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "standard".to_owned());

    let mut cost = 0.0_f64;
    if role.as_deref() == Some("assistant") && !model.is_empty() {
        // Python wraps this in `except Exception: cost = 0.0` so a pricing
        // failure never sinks the report; the Rust engine's `compute_cost` is
        // infallible, so that leg is unreachable here and is not written.
        cost = engine
            .compute_cost(
                &RawTokens::canonical(input_t, output_t, cc, cr),
                &model,
                &provider,
                &speed_value,
                None,
            )
            .total_cost;
    }

    Ok(Msg {
        session_id,
        uuid: uuid.filter(|value| !value.is_empty()),
        parent_uuid: parent_uuid.filter(|value| !value.is_empty()),
        is_sidechain: is_sidechain.is_some_and(|value| value != 0),
        timestamp: timestamp.unwrap_or_default(),
        cost_usd: cost,
        token_total,
    })
}

// ── DAG / branch analysis ────────────────────────────────────────────────────

/// `_ts_to_epoch` — best-effort ISO-8601 → epoch seconds, `None` on anything
/// unparseable (and on the empty string, which is falsy in Python).
///
/// `datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()`. The
/// `.replace` is textual and GLOBAL in Python, so a `Z` anywhere becomes an
/// offset — reproduced by replacing before parsing.
///
/// `datetime.timestamp()` on an aware value is `(dt - EPOCH).total_seconds()`,
/// which CPython evaluates as one exact integer microsecond count divided by
/// `10**6`. That order is reproduced literally: accumulating whole seconds and a
/// fraction separately would round differently in the last ULP, and this value
/// is compared with `>` and subtracted to form `gap_seconds`.
///
/// A NAIVE stamp (no offset) is treated as UTC here; CPython's `.timestamp()`
/// would interpret it in the server's LOCAL zone. See `DIV-e-forks.md` — no row
/// on the harness store is naive.
fn ts_to_epoch(ts: &str) -> Option<f64> {
    if ts.is_empty() {
        return None;
    }
    let text = ts.replace('Z', "+00:00");
    let parsed = parse_isoformat(&text)?;
    let seconds = days_from_civil(parsed.year, parsed.month, parsed.day) * 86_400
        + i64::from(parsed.hour) * 3_600
        + i64::from(parsed.minute) * 60
        + i64::from(parsed.second)
        - i64::from(parsed.offset_seconds.unwrap_or(0));
    let micros = seconds
        .checked_mul(1_000_000)?
        .checked_add(i64::from(parsed.micro))?;
    #[allow(
        clippy::cast_precision_loss,
        reason = "CPython's total_seconds() performs exactly this int -> float division"
    )]
    Some(micros as f64 / 1e6)
}

/// A decomposed `datetime.fromisoformat` result.
struct Stamp {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micro: u32,
    /// `None` is a NAIVE datetime — see [`ts_to_epoch`].
    offset_seconds: Option<i32>,
}

/// `datetime.fromisoformat`, over the calendar-date grammar the store writes.
///
/// The accepted forms are exactly `services::scope::parse_isoformat`'s — which
/// returns `Option<()>`, because it only needs "did this raise", and so cannot
/// be reused for the value: `YYYY-MM-DD` / `YYYYMMDD`, an optional
/// single-character separator, `HH[:MM[:SS[.f…]]]` extended or basic, and a
/// `±HH[:MM[:SS]]` offset. The week-date and ordinal-date forms CPython 3.11
/// added are rejected and appear in no session log.
fn parse_isoformat(text: &str) -> Option<Stamp> {
    let bytes = text.as_bytes();
    let (year, month, day, rest) = if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        (
            digits(&text[0..4])?,
            digits(&text[5..7])?,
            digits(&text[8..10])?,
            &text[10..],
        )
    } else if bytes.len() >= 8 && text[..8].bytes().all(|byte| byte.is_ascii_digit()) {
        (
            digits(&text[0..4])?,
            digits(&text[4..6])?,
            digits(&text[6..8])?,
            &text[8..],
        )
    } else {
        return None;
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(i64::from(year), month) {
        return None;
    }
    let mut stamp = Stamp {
        year: i64::from(year),
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        micro: 0,
        offset_seconds: None,
    };
    if rest.is_empty() {
        return Some(stamp);
    }
    // CPython accepts any single character as the date/time separator.
    let rest = &rest[rest.chars().next()?.len_utf8()..];
    let (clock, offset) = match rest.rfind(['+', '-']) {
        Some(index) => (&rest[..index], Some(&rest[index..])),
        None => (rest, None),
    };
    if let Some(offset) = offset {
        stamp.offset_seconds = Some(parse_offset(offset)?);
    }
    parse_clock(clock, &mut stamp)?;
    Some(stamp)
}

/// `HH[:MM[:SS[.ffffff]]]`, extended or basic, written into `stamp`.
fn parse_clock(clock: &str, stamp: &mut Stamp) -> Option<()> {
    if clock.is_empty() {
        return Some(());
    }
    let (head, fraction) = match clock.split_once(['.', ',']) {
        Some((head, fraction)) => (head, Some(fraction)),
        None => (clock, None),
    };
    let parts: Vec<&str> = if head.contains(':') {
        head.split(':').collect()
    } else {
        match head.len() {
            2 => vec![head],
            4 => vec![&head[0..2], &head[2..4]],
            6 => vec![&head[0..2], &head[2..4], &head[4..6]],
            _ => return None,
        }
    };
    if parts.len() > 3 {
        return None;
    }
    let hour = digits(parts[0])?;
    let minute = parts.get(1).map_or(Some(0), |part| digits(part))?;
    let second = parts.get(2).map_or(Some(0), |part| digits(part))?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    stamp.hour = hour;
    stamp.minute = minute;
    stamp.second = second;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // CPython pads to six digits and truncates beyond them.
        let mut padded: String = fraction.chars().take(6).collect();
        while padded.len() < 6 {
            padded.push('0');
        }
        stamp.micro = padded.parse().ok()?;
    }
    Some(())
}

/// `±HH[:MM[:SS]]` → signed seconds. A fractional part is discarded, matching
/// `services::scope::parse_offset`; no store stamp carries one.
fn parse_offset(text: &str) -> Option<i32> {
    let negative = match text.as_bytes().first()? {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let body = &text[1..];
    let body = body.split(['.', ',']).next()?;
    let parts: Vec<&str> = if body.contains(':') {
        body.split(':').collect()
    } else {
        match body.len() {
            2 => vec![body],
            4 => vec![&body[0..2], &body[2..4]],
            6 => vec![&body[0..2], &body[2..4], &body[4..6]],
            _ => return None,
        }
    };
    if parts.len() > 3 {
        return None;
    }
    let hours = digits(parts[0])?;
    let minutes = parts.get(1).map_or(Some(0), |part| digits(part))?;
    let seconds = parts.get(2).map_or(Some(0), |part| digits(part))?;
    if minutes > 59 || seconds > 59 || hours * 3_600 + minutes * 60 + seconds >= 24 * 3_600 {
        return None;
    }
    let total = i32::try_from(hours * 3_600 + minutes * 60 + seconds).ok()?;
    Some(if negative { -total } else { total })
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The inverse of Howard Hinnant's `civil_from_days`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `calendar.monthrange(year, month)[1]`.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// The per-session index `_abandoned_branches_for_session` builds.
struct Dag<'a> {
    msgs: &'a [Msg],
    /// `by_uuid[m.uuid] = m` — the LAST row with a given uuid wins.
    by_uuid: HashMap<&'a str, usize>,
    /// `children.setdefault(pu, []).append(m)` key order (first appearance).
    parent_order: Vec<&'a str>,
    /// The child rows per parent, in `msgs` order.
    children: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Dag<'a> {
    fn build(msgs: &'a [Msg]) -> Self {
        let mut by_uuid: HashMap<&'a str, usize> = HashMap::new();
        let mut parent_order: Vec<&'a str> = Vec::new();
        let mut children: HashMap<&'a str, Vec<usize>> = HashMap::new();
        for (idx, m) in msgs.iter().enumerate() {
            if let Some(uuid) = m.uuid.as_deref() {
                by_uuid.insert(uuid, idx);
            }
            if let Some(parent) = m.parent_uuid.as_deref() {
                children
                    .entry(parent)
                    .or_insert_with(|| {
                        parent_order.push(parent);
                        Vec::new()
                    })
                    .push(idx);
            }
        }
        Self {
            msgs,
            by_uuid,
            parent_order,
            children,
        }
    }

    /// `children.get(uid, ())`.
    fn child_rows(&self, uuid: &str) -> &[usize] {
        self.children.get(uuid).map_or(&[][..], Vec::as_slice)
    }
}

/// `(message_count, cost_usd, token_total, last_epoch, last_ts)`.
type SubtreeStats = (i64, f64, i64, f64, Option<String>);

/// `_subtree_stats` — aggregate the subtree rooted at `head_uuid`, inclusive.
///
/// Iterative DFS keeps deep chains from blowing the recursion limit, and the
/// `seen` set makes a malformed cyclic link terminate. `stack.pop()` takes the
/// LAST-pushed child first, and that order reaches the answer twice: the `+=`
/// cost chain is order-sensitive in `f64`, and `ep > last_epoch` is strict, so
/// the first-visited of two equal epochs keeps `last_ts`.
fn subtree_stats(head_uuid: &str, dag: &Dag<'_>) -> SubtreeStats {
    let mut count: i64 = 0;
    let mut cost: f64 = 0.0;
    let mut tokens: i64 = 0;
    let mut last_epoch: f64 = 0.0;
    let mut last_ts: Option<String> = None;
    let mut stack: Vec<&str> = vec![head_uuid];
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(uid) = stack.pop() {
        // `if uid in seen: continue` then `seen.add(uid)`.
        if !seen.insert(uid) {
            continue;
        }
        if let Some(&idx) = dag.by_uuid.get(uid) {
            let node = &dag.msgs[idx];
            count += 1;
            // A `+=` chain, NOT `sum()` — do not compensate.
            cost += node.cost_usd;
            tokens += node.token_total;
            if let Some(epoch) = ts_to_epoch(&node.timestamp)
                && epoch > last_epoch
            {
                last_epoch = epoch;
                last_ts = Some(node.timestamp.clone());
            }
        }
        for &child_idx in dag.child_rows(uid) {
            if let Some(child_uuid) = dag.msgs[child_idx].uuid.as_deref()
                && !seen.contains(child_uuid)
            {
                stack.push(child_uuid);
            }
        }
    }
    (count, cost, tokens, last_epoch, last_ts)
}

/// `_branch_reason` — the human-readable one-liner.
fn branch_reason(cost: f64, count: i64, sidechain: bool, gap: Option<f64>) -> String {
    let kind = if sidechain {
        "sidechain branch"
    } else {
        "branch"
    };
    let turns = if count == 1 { "turn" } else { "turns" };
    let when = match gap {
        Some(gap) if gap >= 86_400.0 => {
            format!(" — dropped {:.1}d before the session ended", gap / 86_400.0)
        }
        Some(gap) if gap >= 3_600.0 => {
            format!(" — dropped {:.1}h before the session ended", gap / 3_600.0)
        }
        Some(gap) if gap >= 60.0 => {
            format!(" — dropped {:.0}m before the session ended", gap / 60.0)
        }
        Some(_) => " — dropped shortly before the session ended".to_owned(),
        None => String::new(),
    };
    // `${cost:,.2f}` — the UNROUNDED cost, thousands-separated.
    format!(
        "This {kind} cost ${} over {count} {turns} and was then abandoned{when}.",
        grouped_2f(cost)
    )
}

/// `format(value, ",.2f")` — two decimals, then thousands separators on the
/// integer part. Rust's `{:.2}` is the same correctly-rounded, ties-to-even
/// decimal conversion CPython's `format` performs.
fn grouped_2f(value: f64) -> String {
    let rendered = format!("{value:.2}");
    let (sign, body) = match rendered.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", rendered.as_str()),
    };
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3 + 4);
    for (index, ch) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}{grouped}.{fraction}")
}

/// `_abandoned_branches_for_session` — dropped branches within ONE session.
///
/// A **fork point** is a uuid with >= 2 distinct children. For each, the branch
/// whose subtree reaches the latest is "live"; every other child heads an
/// abandoned branch, reported only when its subtree stops strictly before the
/// session's own last activity and it sank at least [`MIN_BRANCH_COST_USD`].
fn abandoned_branches_for_session(msgs: &[Msg]) -> Vec<AbandonedBranch> {
    if msgs.is_empty() {
        return Vec::new();
    }
    let dag = Dag::build(msgs);

    // `max((_ts_to_epoch(m.timestamp) or 0.0) for m in msgs)` — CPython's `max`
    // keeps the current best unless the next value is strictly greater.
    let mut epochs = msgs
        .iter()
        .map(|m| ts_to_epoch(&m.timestamp).unwrap_or(0.0));
    let mut session_last = epochs.next().unwrap_or(0.0);
    for value in epochs {
        if value > session_last {
            session_last = value;
        }
    }
    // `max((m.timestamp for m in msgs if m.timestamp), default=None)` — a
    // STRING max, which is not the same message as the epoch max whenever the
    // session mixes `…Z` and `…+00:00` spellings.
    let session_last_ts: Option<&str> = msgs
        .iter()
        .map(|m| m.timestamp.as_str())
        .filter(|ts| !ts.is_empty())
        .fold(None, |best, ts| match best {
            Some(current) if ts <= current => Some(current),
            _ => Some(ts),
        });

    let mut out: Vec<AbandonedBranch> = Vec::new();
    for parent_uuid in &dag.parent_order {
        // `{k.uuid: k for k in kids if k.uuid}` — insertion order is first
        // appearance, the VALUE is the last row carrying that uuid.
        let mut distinct_order: Vec<&str> = Vec::new();
        let mut distinct: HashMap<&str, usize> = HashMap::new();
        for &kid_idx in dag.child_rows(parent_uuid) {
            if let Some(uuid) = msgs[kid_idx].uuid.as_deref()
                && distinct.insert(uuid, kid_idx).is_none()
            {
                distinct_order.push(uuid);
            }
        }
        if distinct_order.len() < 2 {
            continue;
        }
        // Rank children by how late their subtree reaches; the latest is "live".
        let mut scored: Vec<(f64, usize, SubtreeStats)> = distinct_order
            .iter()
            .map(|uuid| {
                let head_idx = distinct[uuid];
                let stats = subtree_stats(uuid, &dag);
                (stats.3, head_idx, stats)
            })
            .collect();
        // `sort(key=…, reverse=True)` — STABLE, so ties keep insertion order.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // `scored[0]` is the pursued branch; the rest are candidate-abandoned.
        for (_last_epoch, head_idx, stats) in scored.into_iter().skip(1) {
            let (count, cost, tokens, branch_last, branch_last_ts) = stats;
            // "Went cold": strictly before the session's overall last activity.
            // Equal timestamps => not cold.
            if !(branch_last > 0.0 && session_last > 0.0 && branch_last < session_last) {
                continue;
            }
            if cost < MIN_BRANCH_COST_USD {
                continue;
            }
            // `if (branch_last and session_last)` — float truthiness. Both are
            // strictly positive by the test above, so this is always `Some`.
            let gap = if branch_last == 0.0 || session_last == 0.0 {
                None
            } else {
                Some(session_last - branch_last)
            };
            let head = &msgs[head_idx];
            out.push(AbandonedBranch {
                session_id: head.session_id.clone(),
                fork_uuid: (*parent_uuid).to_owned(),
                // `head.uuid or ""` — truthy by construction here.
                branch_head_uuid: head.uuid.clone().unwrap_or_default(),
                message_count: count,
                cost_usd: round_py(cost, 4),
                token_total: tokens,
                sidechain: head.is_sidechain,
                last_ts: branch_last_ts,
                session_last_ts: session_last_ts.map(str::to_owned),
                gap_seconds: gap.map(|gap| round_py(gap, 1)),
                // NOTE the UNROUNDED cost and the UNROUNDED gap.
                reason: branch_reason(cost, count, head.is_sidechain, gap),
            });
        }
    }
    out
}

/// `_count_fork_points` — messages that are the parent of >= 2 distinct
/// children.
fn count_fork_points(msgs: &[Msg]) -> i64 {
    let mut children: HashMap<&str, HashSet<&str>> = HashMap::new();
    for m in msgs {
        if let (Some(parent), Some(uuid)) = (m.parent_uuid.as_deref(), m.uuid.as_deref()) {
            children.entry(parent).or_default().insert(uuid);
        }
    }
    i64::try_from(children.values().filter(|kids| kids.len() >= 2).count()).unwrap_or(i64::MAX)
}

// ── public entry point ───────────────────────────────────────────────────────

/// `ForkReport().to_dict()` — the well-formed empty result.
///
/// Every dollar field here is the dataclass's declared `0.0`, i.e. a **float**.
/// That is NOT the same as the no-abandoned-branches case, where
/// `abandoned_cost_usd` comes out of `sum([])` as an `int`. See
/// [`analyze_forks`].
fn empty_report() -> Value {
    report_value(
        0,
        0.0,
        0,
        0.0,
        0,
        0,
        0.0,
        0.0,
        0,
        0,
        PyNum::Float(0.0),
        Vec::new(),
    )
}

/// `asdict(ForkReport(...))` — the twelve keys in DECLARATION order.
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per dataclass field, in declaration order — a struct \
    here would only move the key-order risk one level out"
)]
fn report_value(
    sidechain_message_count: i64,
    sidechain_cost_usd: f64,
    sidechain_token_total: i64,
    total_cost_usd: f64,
    total_token_total: i64,
    total_message_count: i64,
    sidechain_cost_share: f64,
    sidechain_token_share: f64,
    fork_point_count: i64,
    abandoned_branch_count: i64,
    abandoned_cost_usd: PyNum,
    abandoned_branches: Vec<Value>,
) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "sidechain_message_count".to_owned(),
        ji(sidechain_message_count),
    );
    obj.insert("sidechain_cost_usd".to_owned(), jf(sidechain_cost_usd));
    obj.insert(
        "sidechain_token_total".to_owned(),
        ji(sidechain_token_total),
    );
    obj.insert("total_cost_usd".to_owned(), jf(total_cost_usd));
    obj.insert("total_token_total".to_owned(), ji(total_token_total));
    obj.insert("total_message_count".to_owned(), ji(total_message_count));
    obj.insert("sidechain_cost_share".to_owned(), jf(sidechain_cost_share));
    obj.insert(
        "sidechain_token_share".to_owned(),
        jf(sidechain_token_share),
    );
    obj.insert("fork_point_count".to_owned(), ji(fork_point_count));
    obj.insert(
        "abandoned_branch_count".to_owned(),
        ji(abandoned_branch_count),
    );
    obj.insert(
        "abandoned_cost_usd".to_owned(),
        abandoned_cost_usd.to_json(),
    );
    obj.insert(
        "abandoned_branches".to_owned(),
        Value::Array(abandoned_branches),
    );
    Value::Object(obj)
}

/// `analyze_forks(conn, scope=…, project_ids=…, top_n=…, compute_cost=…)`.
///
/// Advisory and total: a missing `messages` relation, a store with no DAG
/// links, or any arithmetic edge returns an empty-but-well-formed report.
///
/// `project_ids = None` spans every project in scope; an EMPTY slice is "a
/// filter was requested and matched nothing" and scopes to nothing.
#[must_use]
pub fn analyze_forks(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
    top_n: i64,
    engine: &PricingEngine,
) -> Value {
    let msgs = load_messages(conn, scope, project_ids, engine);
    if msgs.is_empty() {
        return empty_report();
    }

    // ── sidechain economics ─────────────────────────────────────────────────
    // Five `+=` chains. NOT `sum()`, so NOT compensated.
    let mut total_cost = 0.0_f64;
    let mut total_tokens: i64 = 0;
    let mut side_cost = 0.0_f64;
    let mut side_tokens: i64 = 0;
    let mut side_count: i64 = 0;
    for m in &msgs {
        total_cost += m.cost_usd;
        total_tokens += m.token_total;
        if m.is_sidechain {
            side_count += 1;
            side_cost += m.cost_usd;
            side_tokens += m.token_total;
        }
    }
    let cost_share = if total_cost > 0.0 {
        side_cost / total_cost
    } else {
        0.0
    };
    #[allow(
        clippy::cast_precision_loss,
        reason = "Python's `int / int` is true division and produces this float"
    )]
    let token_share = if total_tokens > 0 {
        side_tokens as f64 / total_tokens as f64
    } else {
        0.0
    };

    // ── branch / abandonment economics (per session) ────────────────────────
    // `by_session.setdefault(...)` — insertion-ordered by first appearance,
    // which `ORDER BY sessions.session_id` makes session-id order. That order
    // decides the tie-break of the stable sort below.
    let mut session_order: Vec<&str> = Vec::new();
    let mut by_session: HashMap<&str, Vec<Msg>> = HashMap::new();
    for m in &msgs {
        by_session
            .entry(m.session_id.as_str())
            .or_insert_with(|| {
                session_order.push(m.session_id.as_str());
                Vec::new()
            })
            .push(m.clone());
    }

    let mut fork_point_count: i64 = 0;
    for session_id in &session_order {
        fork_point_count += count_fork_points(&by_session[session_id]);
    }

    let mut abandoned: Vec<AbandonedBranch> = Vec::new();
    for session_id in &session_order {
        // Python wraps each session in `except Exception: continue` so one bad
        // session cannot sink the report. Nothing in the Rust walk can fail, so
        // the handler is recorded rather than written.
        abandoned.extend(abandoned_branches_for_session(&by_session[session_id]));
    }

    // `sort(key=lambda b: b.cost_usd, reverse=True)` — on the ROUNDED cost, and
    // stable, so ties keep session order.
    abandoned.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // `round(sum(b.cost_usd for b in abandoned), 4)` — this ONE roll-up is
    // `sum()`, so it IS Neumaier-compensated, and `sum([])` is the `int` 0,
    // which `round(0, 4)` leaves an `int`. LAW 3, both halves.
    let mut acc = Neumaier::default();
    for branch in &abandoned {
        acc.add(branch.cost_usd);
    }
    let abandoned_cost = match acc.finish_pynum() {
        PyNum::Int(value) => PyNum::Int(value),
        PyNum::Float(value) => PyNum::Float(round_py(value, 4)),
    };
    let abandoned_count = i64::try_from(abandoned.len()).unwrap_or(i64::MAX);
    // `abandoned[: max(0, top_n)]` — the COUNT above is the FULL list's.
    let top: Vec<Value> = abandoned
        .iter()
        .take(usize::try_from(top_n.max(0)).unwrap_or(0))
        .map(AbandonedBranch::to_value)
        .collect();

    report_value(
        side_count,
        round_py(side_cost, 4),
        side_tokens,
        round_py(total_cost, 4),
        total_tokens,
        i64::try_from(msgs.len()).unwrap_or(i64::MAX),
        round_py(cost_share, 4),
        round_py(token_share, 4),
        fork_point_count,
        abandoned_count,
        abandoned_cost,
        top,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn engine() -> PricingEngine {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        PricingEngine::from_manifest_path(&crate::pricing::manifest_path(&package))
            .expect("the shipped manifest")
    }

    /// A free message — the DAG shape is what most of these pin.
    fn msg(uuid: &str, parent: Option<&str>, ts: &str) -> Msg {
        Msg {
            session_id: "s1".to_owned(),
            uuid: (!uuid.is_empty()).then(|| uuid.to_owned()),
            parent_uuid: parent.map(str::to_owned),
            is_sidechain: false,
            timestamp: ts.to_owned(),
            cost_usd: 0.0,
            token_total: 0,
        }
    }

    fn priced(uuid: &str, parent: Option<&str>, ts: &str, cost: f64, tokens: i64) -> Msg {
        Msg {
            cost_usd: cost,
            token_total: tokens,
            ..msg(uuid, parent, ts)
        }
    }

    fn iso(day: u32, hour: u32) -> String {
        format!("2026-07-{day:02}T{hour:02}:00:00+00:00")
    }

    // ── the timestamp primitive ─────────────────────────────────────────────

    #[test]
    fn ts_to_epoch_matches_cpython_on_the_shapes_the_store_writes() {
        // `2026-07-31T00:00:00+00:00` is 1785456000. Both spellings on disk,
        // plus the fractional form and an offset.
        assert_eq!(
            ts_to_epoch("2026-07-31T00:00:00+00:00"),
            Some(1_785_456_000.0)
        );
        assert_eq!(ts_to_epoch("2026-07-31T00:00:00Z"), Some(1_785_456_000.0));
        assert_eq!(
            ts_to_epoch("2026-07-31T00:00:00.500000+00:00"),
            Some(1_785_456_000.5)
        );
        // `-07:00` is seven hours LATER in UTC.
        assert_eq!(
            ts_to_epoch("2026-07-31T00:00:00-07:00"),
            Some(1_785_456_000.0 + 7.0 * 3_600.0)
        );
    }

    #[test]
    fn ts_to_epoch_is_none_for_the_empty_and_the_malformed() {
        assert_eq!(ts_to_epoch(""), None);
        assert_eq!(ts_to_epoch("not a timestamp"), None);
        assert_eq!(ts_to_epoch("2026-13-01T00:00:00+00:00"), None);
        assert_eq!(ts_to_epoch("2026-02-30T00:00:00+00:00"), None);
        // A naive stamp parses — and is read as UTC here. DIV ledger entry 3.
        assert_eq!(ts_to_epoch("2026-07-31T00:00:00"), Some(1_785_456_000.0));
    }

    // ── the DAG walk, branch by branch ──────────────────────────────────────

    #[test]
    fn a_linear_chain_has_no_fork_point_and_no_abandoned_branch() {
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            msg("b", Some("a"), &iso(1, 1)),
            msg("c", Some("b"), &iso(1, 2)),
        ];
        assert_eq!(count_fork_points(&msgs), 0);
        assert!(abandoned_branches_for_session(&msgs).is_empty());
    }

    #[test]
    fn a_fork_reports_the_cold_branch_and_never_the_live_one() {
        // a -> {b (dies at 01:00), c (lives on to 05:00 through d)}
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), 2.5, 400),
            priced("c", Some("a"), &iso(1, 2), 1.0, 100),
            priced("d", Some("c"), &iso(1, 5), 3.0, 300),
        ];
        assert_eq!(count_fork_points(&msgs), 1);
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1, "only the loser is reported");
        let branch = &out[0];
        assert_eq!(branch.fork_uuid, "a");
        assert_eq!(branch.branch_head_uuid, "b");
        assert_eq!(branch.message_count, 1);
        assert!((branch.cost_usd - 2.5).abs() < 1e-12);
        assert_eq!(branch.token_total, 400);
        assert_eq!(branch.last_ts.as_deref(), Some(iso(1, 1)).as_deref());
        assert_eq!(
            branch.session_last_ts.as_deref(),
            Some(iso(1, 5)).as_deref()
        );
        // 05:00 − 01:00 = 4 h.
        assert_eq!(branch.gap_seconds, Some(14_400.0));
        assert_eq!(
            branch.reason,
            "This branch cost $2.50 over 1 turn and was then abandoned \
             — dropped 4.0h before the session ended."
        );
    }

    #[test]
    fn a_branch_under_the_penny_floor_is_dropped_but_still_counts_as_a_fork() {
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), 0.009_9, 10),
            priced("c", Some("a"), &iso(1, 5), 1.0, 10),
        ];
        assert_eq!(count_fork_points(&msgs), 1);
        assert!(
            abandoned_branches_for_session(&msgs).is_empty(),
            "0.0099 < MIN_BRANCH_COST_USD"
        );
        // …and exactly at the floor it IS reported (`cost < MIN` is strict).
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), MIN_BRANCH_COST_USD, 10),
            priced("c", Some("a"), &iso(1, 5), 1.0, 10),
        ];
        assert_eq!(abandoned_branches_for_session(&msgs).len(), 1);
    }

    #[test]
    fn a_branch_that_ties_the_session_end_is_not_cold() {
        // Both children reach 05:00, so the loser's subtree is NOT strictly
        // before the session's last activity.
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 5), 5.0, 10),
            priced("c", Some("a"), &iso(1, 5), 5.0, 10),
        ];
        assert_eq!(count_fork_points(&msgs), 1);
        assert!(abandoned_branches_for_session(&msgs).is_empty());
    }

    #[test]
    fn a_subtree_is_summed_whole_and_the_walk_terminates_on_a_cycle() {
        // b <-> e is a cycle inside the abandoned branch; the `seen` set must
        // stop it, and each node must be counted exactly once.
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), 1.0, 10),
            priced("e", Some("b"), &iso(1, 2), 2.0, 20),
            priced("c", Some("a"), &iso(1, 9), 0.5, 5),
            // The malformed edge: e is also the parent of b.
            priced("b", Some("e"), &iso(1, 1), 1.0, 10),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        // {b, e} — two distinct uuids, each visited once, despite the loop.
        assert_eq!(out[0].message_count, 2);
        assert!((out[0].cost_usd - 3.0).abs() < 1e-12);
        assert_eq!(out[0].token_total, 30);
    }

    #[test]
    fn a_self_parent_is_one_node_and_does_not_hang() {
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), 1.0, 10),
            // c is its own parent AND a child of a: two edges, one node.
            priced("c", Some("a"), &iso(1, 2), 0.5, 5),
            priced("c", Some("c"), &iso(1, 2), 0.5, 5),
            priced("d", Some("a"), &iso(1, 9), 0.5, 5),
        ];
        let out = abandoned_branches_for_session(&msgs);
        // Two losers (b and c) against the live branch d.
        assert_eq!(out.len(), 2);
        for branch in &out {
            assert_eq!(branch.message_count, 1, "the self-edge adds no node");
        }
    }

    #[test]
    fn a_missing_parent_still_forks_and_the_orphan_head_is_priced() {
        // "ghost" is nobody's uuid — it is a fork point all the same, because
        // `children` is keyed on parent_uuid whether or not the parent exists.
        let msgs = vec![
            priced("b", Some("ghost"), &iso(1, 1), 4.0, 40),
            priced("c", Some("ghost"), &iso(1, 9), 1.0, 10),
        ];
        assert_eq!(count_fork_points(&msgs), 1);
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fork_uuid, "ghost");
        assert_eq!(out[0].branch_head_uuid, "b");
        assert_eq!(out[0].message_count, 1);
    }

    #[test]
    fn two_roots_are_two_subtrees_that_share_one_session_last() {
        // Root r1 forks and dies early; root r2 carries the session to 20:00.
        // The abandoned branch under r1 is measured against the WHOLE session.
        let msgs = vec![
            msg("r1", None, &iso(1, 0)),
            priced("x", Some("r1"), &iso(1, 1), 3.0, 30),
            priced("y", Some("r1"), &iso(1, 2), 1.0, 10),
            msg("r2", None, &iso(1, 10)),
            msg("z", Some("r2"), &iso(1, 20)),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].branch_head_uuid, "x");
        assert_eq!(
            out[0].session_last_ts.as_deref(),
            Some(iso(1, 20)).as_deref(),
            "session_last_ts spans the other root"
        );
        // 20:00 − 01:00 = 19 h.
        assert_eq!(out[0].gap_seconds, Some(68_400.0));
    }

    #[test]
    fn an_orphan_sidechain_branch_is_labelled_a_sidechain_branch() {
        let mut side = priced("b", Some("a"), &iso(1, 1), 1.5, 15);
        side.is_sidechain = true;
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            side,
            priced("c", Some("a"), &iso(3, 0), 1.0, 10),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert!(out[0].sidechain);
        // 2026-07-03T00:00 − 2026-07-01T01:00 = 47 h = 1.958…d, `:.1f` → 2.0.
        assert_eq!(
            out[0].reason,
            "This sidechain branch cost $1.50 over 1 turn and was then abandoned \
             — dropped 2.0d before the session ended."
        );
    }

    #[test]
    fn a_row_with_no_uuid_never_becomes_a_branch_head() {
        // Three children of `a`, but one has no uuid: `distinct` has two.
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("", Some("a"), &iso(1, 1), 9.0, 90),
            priced("b", Some("a"), &iso(1, 2), 2.0, 20),
            priced("c", Some("a"), &iso(1, 9), 1.0, 10),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].branch_head_uuid, "b");
        assert!(
            (out[0].cost_usd - 2.0).abs() < 1e-12,
            "the uuid-less sibling is in nobody's subtree"
        );
    }

    #[test]
    fn a_duplicate_child_uuid_is_one_branch_and_the_last_row_is_its_head() {
        // Two rows share uuid "b"; `distinct` keeps ONE entry whose value is
        // the LAST row — so `sidechain` comes from the second one.
        let mut second = priced("b", Some("a"), &iso(1, 1), 1.0, 10);
        second.is_sidechain = true;
        let msgs = vec![
            msg("a", None, &iso(1, 0)),
            priced("b", Some("a"), &iso(1, 1), 1.0, 10),
            second,
            priced("c", Some("a"), &iso(1, 9), 1.0, 10),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert!(out[0].sidechain, "the LAST duplicate wins the dict value");
        assert_eq!(count_fork_points(&msgs), 1, "{{b, c}}, not {{b, b, c}}");
    }

    #[test]
    fn the_reason_line_walks_all_four_gap_rungs_and_the_turn_plural() {
        assert!(
            branch_reason(1.0, 1, false, Some(90_000.0))
                .ends_with("1.0d before the session ended.")
        );
        assert!(
            branch_reason(1.0, 2, false, Some(7_200.0)).ends_with("2.0h before the session ended.")
        );
        assert!(
            // 150 / 60 = 2.5, and `:.0f` is ties-to-EVEN on the decimal
            // expansion in both languages — so "2m", not the "3m" Rust's
            // `f64::round` (half away from zero) would have produced.
            branch_reason(1.0, 2, false, Some(150.0)).ends_with("2m before the session ended.")
        );
        assert!(
            branch_reason(1.0, 2, false, Some(210.0)).ends_with("4m before the session ended."),
            "3.5 rounds UP to 4 under the same ties-to-even rule"
        );
        assert!(
            branch_reason(1.0, 2, false, Some(59.0))
                .ends_with("abandoned — dropped shortly before the session ended.")
        );
        // `gap is None` drops the whole clause.
        assert_eq!(
            branch_reason(1.0, 1, false, None),
            "This branch cost $1.00 over 1 turn and was then abandoned."
        );
        // Thousands separators, and `turn` only at exactly 1.
        assert!(
            branch_reason(12_345.678, 3, true, None)
                .starts_with("This sidechain branch cost $12,345.68 over 3 turns")
        );
    }

    // ── the report envelope ─────────────────────────────────────────────────

    #[test]
    fn an_empty_store_is_the_dataclass_defaults_with_float_zeros() {
        let conn = Connection::open_in_memory().expect("in-memory");
        let report = analyze_forks(&conn, None, None, TOP_N, &engine());
        assert_eq!(
            stax_memory::pyjson::dumps_http(&report),
            r#"{"sidechain_message_count":0,"sidechain_cost_usd":0.0,"sidechain_token_total":0,"total_cost_usd":0.0,"total_token_total":0,"total_message_count":0,"sidechain_cost_share":0.0,"sidechain_token_share":0.0,"fork_point_count":0,"abandoned_branch_count":0,"abandoned_cost_usd":0.0,"abandoned_branches":[]}"#
        );
    }

    /// A store shaped like the real one: `messages` is a VIEW.
    fn fixture_store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, provider TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                    session_id TEXT, last_ts TEXT, message_count INTEGER);
             CREATE TABLE messages_raw (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER, timestamp TEXT,
                 role TEXT, model TEXT, input_tokens INTEGER, output_tokens INTEGER,
                 cache_create_tokens INTEGER, cache_read_tokens INTEGER,
                 is_sidechain INTEGER, uuid TEXT, parent_uuid TEXT, speed TEXT);
             CREATE VIEW messages AS SELECT * FROM messages_raw;
             INSERT INTO projects VALUES (1, 'proj', 'anthropic');
             INSERT INTO sessions VALUES (7, 1, 'sess-a', '2026-07-01T09:00:00+00:00', 3);
             INSERT INTO messages_raw VALUES
                 (1, 7, 1, '2026-07-01T00:00:00+00:00', 'user', NULL, 0,0,0,0, 0, 'a', NULL, NULL),
                 (2, 7, 2, '2026-07-01T01:00:00+00:00', 'user', NULL, 0,0,0,0, 1, 'b', 'a',  NULL),
                 (3, 7, 3, '2026-07-01T09:00:00+00:00', 'user', NULL, 0,0,0,0, 0, 'c', 'a',  NULL);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn the_messages_view_is_found_by_the_wide_guard() {
        let conn = fixture_store();
        // LAW 7 / DIV-148: `messages` here is a VIEW, and the narrow
        // `type='table'` guard would read this populated store as empty.
        assert!(table_or_view_exists(&conn, "messages"));
        assert!(!crate::mart_queries::table_exists(&conn, "messages").expect("query"));
    }

    #[test]
    fn no_abandoned_branch_makes_abandoned_cost_usd_an_int_zero() {
        // Every message here is free, so the one cold branch is under the penny
        // floor and `abandoned` is empty — `sum([])` is `int` 0 and the wire
        // shows `0`, while the neighbouring cost fields stay floats.
        let conn = fixture_store();
        let report = analyze_forks(&conn, None, None, TOP_N, &engine());
        let rendered = stax_memory::pyjson::dumps_http(&report);
        assert!(
            rendered.contains(r#""abandoned_cost_usd":0,"#),
            "int zero, got {rendered}"
        );
        assert!(rendered.contains(r#""total_cost_usd":0.0,"#), "{rendered}");
        assert!(rendered.contains(r#""fork_point_count":1,"#), "{rendered}");
        assert!(
            rendered.contains(r#""sidechain_message_count":1,"#),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_project_id_slice_scopes_to_nothing_and_none_scopes_to_all() {
        let conn = fixture_store();
        let engine = engine();
        let none = analyze_forks(&conn, None, None, TOP_N, &engine);
        assert_eq!(none["total_message_count"], Value::from(3));
        let empty = analyze_forks(&conn, None, Some(&[]), TOP_N, &engine);
        assert_eq!(empty["total_message_count"], Value::from(0));
        // …and the empty-slice answer is the DEFAULTS report, floats and all.
        assert_eq!(empty["abandoned_cost_usd"], Value::from(0.0));
        let matched = analyze_forks(&conn, None, Some(&[1]), TOP_N, &engine);
        assert_eq!(matched["total_message_count"], Value::from(3));
        let unmatched = analyze_forks(&conn, None, Some(&[99]), TOP_N, &engine);
        assert_eq!(unmatched["total_message_count"], Value::from(0));
    }

    #[test]
    fn the_scope_bounds_are_bound_as_string_comparisons_on_the_timestamp() {
        let conn = fixture_store();
        let scope = Scope::new(
            Some("2026-07-01T00:30:00+00:00".to_owned()),
            Some("2026-07-01T02:00:00+00:00".to_owned()),
            "window",
        );
        let report = analyze_forks(&conn, Some(&scope), None, TOP_N, &engine());
        assert_eq!(report["total_message_count"], Value::from(1));
        assert_eq!(report["sidechain_message_count"], Value::from(1));
        // One message in window means no fork point survives the filter.
        assert_eq!(report["fork_point_count"], Value::from(0));
    }

    #[test]
    fn twelve_cold_branches_are_counted_whole_and_the_list_is_capped_at_top_n() {
        let mut msgs = vec![msg("root", None, &iso(1, 0))];
        for i in 0..12 {
            msgs.push(priced(
                &format!("k{i}"),
                Some("root"),
                &iso(1, 1),
                1.0 + f64::from(i),
                10,
            ));
        }
        msgs.push(priced("live", Some("root"), &iso(2, 0), 0.5, 5));
        let branches = abandoned_branches_for_session(&msgs);
        assert_eq!(branches.len(), 12, "the count is of the FULL list");
        // The cap itself is `analyze_forks`'s slice; check the arithmetic here.
        let capped = usize::try_from(TOP_N.max(0)).expect("positive");
        assert_eq!(branches.iter().take(capped).count(), 10);
    }

    #[test]
    fn the_string_max_and_the_epoch_max_can_name_different_stamps() {
        // Both stamp SHAPES the store writes appear in one session. At the
        // point they differ, `'Z'` (0x5A) beats `'.'` (0x2E), so the
        // fraction-less `Z` spelling sorts ABOVE a stamp half a second LATER —
        // the string max and the epoch max name different messages.
        let msgs = vec![
            msg("a", None, "2026-07-01T00:00:00+00:00"),
            priced("b", Some("a"), "2026-07-01T01:00:00+00:00", 1.0, 10),
            // The session's latest INSTANT, with microseconds and an offset…
            priced("c", Some("a"), "2026-07-01T09:00:00.500000+00:00", 1.0, 10),
            // …and half a second EARLIER, spelled `Z`, which wins the STRING max.
            msg("d", Some("c"), "2026-07-01T09:00:00Z"),
        ];
        let out = abandoned_branches_for_session(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].session_last_ts.as_deref(),
            Some("2026-07-01T09:00:00Z"),
            "the string max, not the latest instant"
        );
        // The gap is measured against the EPOCH max (09:00:00.5), not that
        // string: 8 h and the extra half second.
        assert_eq!(out[0].gap_seconds, Some(28_800.5));
    }
}

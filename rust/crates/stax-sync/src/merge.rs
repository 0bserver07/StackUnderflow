//! `sync/merge.py` — the cross-device union overlay (Phase 2, §5 / §7).
//!
//! Because sync moves *derived aggregates* and each session is aggregated
//! exactly once on its origin device, cross-device merge is an **additive union
//! at the stable grain**, not conflict resolution. Every `unioned_*` here is
//!
//! ```text
//! local mart  (JOIN projects for slug where the mart keys on project_id)
//! UNION ALL   <mart>_remote
//! GROUP BY    (provider, slug, …)   SUM(measures)
//! ```
//!
//! `session_mart` is the one non-additive case: the same `session_id` can appear
//! on two devices only if the user hand-copied raw logs. It is deduped by
//! `session_id` with a deterministic tiebreak — **local wins, then lowest
//! device_uuid** — and every dropped duplicate counts into `merge_warnings`.
//!
//! # Why this lives here and not only in `routes/sync.rs`
//!
//! Batch D ported `merged_overview` inline into the route because `stax-server`
//! could not take a dependency it was not allowed to add. This crate is that
//! dependency. The SQL below is the same text, and
//! [`crate::merge::merged_overview`] produces the same payload — the route now
//! calls it, so there is one owner (the wave-5 dedup law) and the endpoint
//! matrix keeps proving it.
//!
//! # The two arithmetic traps, preserved
//!
//! * `totals["cost_usd"]` is `sum(generator)` — **Neumaier-compensated**
//!   (DIV-057) — while `by_day`'s costs accumulate with `+=` four lines later.
//!   They are not interchangeable and "more accurate" would be a divergence.
//! * `sum([])` is the **`int` 0**, not `0.0`. An empty union renders
//!   `"cost_usd":0`, while `by_day`'s buckets start at a literal `0.0` and stay
//!   floats however empty they are.

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::pyvalue::PyValue;

/// `_UNIONED_DAILY`.
pub const UNIONED_DAILY: &str = "
SELECT day, provider, slug, model, speed,
       SUM(input_tokens)  AS input_tokens,
       SUM(output_tokens) AS output_tokens,
       SUM(cache_read)    AS cache_read,
       SUM(cache_create)  AS cache_create,
       SUM(message_count) AS message_count,
       SUM(session_count) AS session_count,
       SUM(cost_usd)      AS cost_usd
FROM (
    SELECT d.day, d.provider, p.slug, d.model, d.speed,
           d.input_tokens, d.output_tokens, d.cache_read, d.cache_create,
           d.message_count, d.session_count, d.cost_usd
    FROM daily_mart d JOIN projects p ON p.id = d.project_id
    UNION ALL
    SELECT day, provider, slug, model, speed,
           input_tokens, output_tokens, cache_read, cache_create,
           message_count, session_count, cost_usd
    FROM daily_mart_remote
)
GROUP BY day, provider, slug, model, speed
ORDER BY day, provider, slug, model, speed
";

/// `_UNIONED_PROVIDER_DAY`.
///
/// `project_count` is SUMmed at the stable grain like the spec's mechanical rule
/// says; it can *overcount* a project active on two devices the same day (a
/// distinct-count that is not additive across devices — the documented §5.1
/// family of limitations). The additive measures are exact.
pub const UNIONED_PROVIDER_DAY: &str = "
SELECT day, provider,
       SUM(cost_usd)       AS cost_usd,
       SUM(message_count)  AS message_count,
       SUM(session_count)  AS session_count,
       SUM(project_count)  AS project_count
FROM (
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart
    UNION ALL
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart_remote
)
GROUP BY day, provider
ORDER BY day, provider
";

/// `_UNIONED_MODEL_DAY`.
pub const UNIONED_MODEL_DAY: &str = "
SELECT day, model, speed,
       SUM(cost_usd)       AS cost_usd,
       SUM(input_tokens)   AS input_tokens,
       SUM(output_tokens)  AS output_tokens,
       SUM(cache_read)     AS cache_read,
       SUM(cache_create)   AS cache_create,
       SUM(message_count)  AS message_count,
       SUM(session_count)  AS session_count
FROM (
    SELECT day, model, speed, cost_usd, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count
    FROM model_day_mart
    UNION ALL
    SELECT day, model, speed, cost_usd, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count
    FROM model_day_mart_remote
)
GROUP BY day, model, speed
ORDER BY day, model, speed
";

/// `_UNIONED_PROJECTS`.
///
/// `first_ts` / `last_ts` take the widest window across devices; `display_name`
/// is the MAX (deterministic) of the contributing names.
pub const UNIONED_PROJECTS: &str = "
SELECT provider, slug,
       MAX(display_name)         AS display_name,
       MIN(first_ts)             AS first_ts,
       MAX(last_ts)              AS last_ts,
       SUM(total_messages)       AS total_messages,
       SUM(total_sessions)       AS total_sessions,
       SUM(total_input_tokens)   AS total_input_tokens,
       SUM(total_output_tokens)  AS total_output_tokens,
       SUM(total_cache_read)     AS total_cache_read,
       SUM(total_cache_create)   AS total_cache_create,
       SUM(total_cost_usd)       AS total_cost_usd
FROM (
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart
    UNION ALL
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart_remote
)
GROUP BY provider, slug
ORDER BY provider, slug
";

/// `_UNIONED_SESSIONS` — the one non-additive family.
///
/// Local rows carry device `''` (empty string) which sorts before any hex UUID,
/// so `ORDER BY session_id, device_uuid` makes **local win** the tiebreak, then
/// the lowest remote `device_uuid` — a deterministic "earliest-device" rule with
/// no wall-clock dependence.
pub const UNIONED_SESSIONS: &str = "
SELECT '' AS device_uuid, s.session_id, s.provider, p.slug, s.primary_model,
       s.first_ts, s.last_ts, s.message_count, s.user_message_count,
       s.assistant_message_count, s.input_tokens, s.output_tokens,
       s.cache_read, s.cache_create, s.cost_usd, s.is_one_shot
FROM session_mart s JOIN projects p ON p.id = s.project_id
UNION ALL
SELECT device_uuid, session_id, provider, slug, primary_model,
       first_ts, last_ts, message_count, user_message_count,
       assistant_message_count, input_tokens, output_tokens,
       cache_read, cache_create, cost_usd, is_one_shot
FROM session_mart_remote
ORDER BY session_id, device_uuid
";

/// `[dict(r) for r in conn.execute(sql).fetchall()]`, storage classes intact.
///
/// # Errors
/// Any SQLite failure.
pub fn query_rows(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Map<String, Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (index, name) in names.iter().enumerate() {
            obj.insert(
                name.clone(),
                PyValue::from_sqlite(row.get_ref(index)?).to_json(),
            );
        }
        out.push(obj);
    }
    Ok(out)
}

/// `unioned_daily(conn)`.
///
/// # Errors
/// Any SQLite failure.
pub fn unioned_daily(conn: &Connection) -> rusqlite::Result<Vec<Map<String, Value>>> {
    query_rows(conn, UNIONED_DAILY)
}

/// `unioned_provider_day(conn)`.
///
/// # Errors
/// Any SQLite failure.
pub fn unioned_provider_day(conn: &Connection) -> rusqlite::Result<Vec<Map<String, Value>>> {
    query_rows(conn, UNIONED_PROVIDER_DAY)
}

/// `unioned_model_day(conn)`.
///
/// Not read by `merged_overview` — the route never surfaces it — but part of
/// the module's public surface and therefore ported.
///
/// # Errors
/// Any SQLite failure.
pub fn unioned_model_day(conn: &Connection) -> rusqlite::Result<Vec<Map<String, Value>>> {
    query_rows(conn, UNIONED_MODEL_DAY)
}

/// `unioned_projects(conn)`.
///
/// # Errors
/// Any SQLite failure.
pub fn unioned_projects(conn: &Connection) -> rusqlite::Result<Vec<Map<String, Value>>> {
    query_rows(conn, UNIONED_PROJECTS)
}

/// `unioned_sessions(conn)` — deduped rows plus the dropped-duplicate count.
///
/// # Errors
/// Any SQLite failure.
pub fn unioned_sessions(conn: &Connection) -> rusqlite::Result<(Vec<Map<String, Value>>, i64)> {
    let rows = query_rows(conn, UNIONED_SESSIONS)?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut merge_warnings = 0_i64;
    for row in rows {
        // `r["session_id"]` — a `sqlite3.Row` subscript, which raises on a NULL
        // column name but happily yields `None` for a NULL value. `None` is
        // hashable, so a NULL session id dedups against other NULLs. Ported by
        // rendering the cell rather than requiring a string.
        let sid = row
            .get("session_id")
            .map_or_else(|| "None".to_owned(), ToString::to_string);
        if !seen.insert(sid) {
            merge_warnings += 1;
            continue;
        }
        out.push(row);
    }
    Ok((out, merge_warnings))
}

/// `device_breakdown(conn)` — the local row, then one per pulled peer.
///
/// # Errors
/// Any SQLite failure.
pub fn device_breakdown(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut out = Vec::new();
    let (projects, cost) = conn.query_row(
        "SELECT COUNT(*) AS projects, COALESCE(SUM(total_cost_usd), 0.0) AS cost_usd \
         FROM project_mart",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
    )?;
    let mut local = Map::new();
    local.insert("device_uuid".into(), Value::from("(local)"));
    local.insert("alias".into(), Value::Null);
    local.insert("is_local".into(), Value::Bool(true));
    local.insert("projects".into(), Value::from(projects));
    // `float(local["cost_usd"])` — a float even when the sum is 0.
    local.insert("cost_usd".into(), PyValue::Float(cost).to_json());
    out.push(Value::Object(local));

    let mut stmt = conn.prepare(
        "SELECT r.device_uuid AS device_uuid, d.alias AS alias, \
                COUNT(*) AS projects, COALESCE(SUM(r.total_cost_usd), 0.0) AS cost_usd \
         FROM project_mart_remote r \
         LEFT JOIN sync_remote_devices d ON d.remote_device_uuid = r.device_uuid \
         GROUP BY r.device_uuid, d.alias \
         ORDER BY r.device_uuid",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut peer = Map::new();
        peer.insert("device_uuid".into(), Value::from(row.get::<_, String>(0)?));
        peer.insert(
            "alias".into(),
            row.get::<_, Option<String>>(1)?
                .map_or(Value::Null, Value::from),
        );
        peer.insert("is_local".into(), Value::Bool(false));
        peer.insert("projects".into(), Value::from(row.get::<_, i64>(2)?));
        peer.insert(
            "cost_usd".into(),
            PyValue::Float(row.get::<_, f64>(3)?).to_json(),
        );
        out.push(Value::Object(peer));
    }
    Ok(out)
}

/// `remote_row_count(conn)` — total rows across every `<mart>_remote` table.
///
/// # Errors
/// Any SQLite failure. The family list is hard-coded in the reference (it does
/// NOT read `MART_FAMILIES`), and that literal is reproduced — the two lists
/// agree today and a port that unified them would hide it if they stopped.
pub fn remote_row_count(conn: &Connection) -> rusqlite::Result<i64> {
    let mut total = 0_i64;
    for family in [
        "daily_mart",
        "provider_day_mart",
        "model_day_mart",
        "project_mart",
        "session_mart",
    ] {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) AS n FROM {family}_remote"),
            [],
            |row| row.get(0),
        )?;
        total += count;
    }
    Ok(total)
}

/// Neumaier compensated summation — CPython's `builtins.sum` float fast path.
///
/// Copied in shape from `stax_etl::stats::aggregator::Neumaier` rather than
/// depended on, because taking `stax-etl` into the sync crate for eleven lines
/// of accumulator would drag the whole ETL surface behind it. The behaviour is
/// pinned against that crate's by the differ, which sums the same rows on both
/// sides.
#[derive(Debug, Default, Clone, Copy)]
pub struct Neumaier {
    sum: f64,
    compensation: f64,
    /// Whether anything has been added — `sum([])` is the `int` 0.
    seen: bool,
}

impl Neumaier {
    /// Add one term.
    pub fn add(&mut self, value: f64) {
        self.seen = true;
        let total = self.sum + value;
        if self.sum.abs() >= value.abs() {
            self.compensation += (self.sum - total) + value;
        } else {
            self.compensation += (value - total) + self.sum;
        }
        self.sum = total;
    }

    /// The compensated total.
    #[must_use]
    pub fn finish(self) -> f64 {
        self.sum + self.compensation
    }

    /// The value CPython's `sum` returns — `int` 0 for an empty iterable.
    #[must_use]
    pub fn to_json(self) -> Value {
        if self.seen {
            PyValue::Float(self.finish()).to_json()
        } else {
            Value::from(0)
        }
    }
}

/// `merged_overview(conn)` — the compact cross-device payload (USD).
///
/// Key order: `totals`, `by_day`, `by_project`, `by_provider_day`, `devices`,
/// `merge_warnings`.
///
/// # Errors
/// Any SQLite failure.
pub fn merged_overview(conn: &Connection) -> rusqlite::Result<Map<String, Value>> {
    let daily = unioned_daily(conn)?;
    let projects = unioned_projects(conn)?;
    let provider_day = unioned_provider_day(conn)?;
    let (sessions, merge_warnings) = unioned_sessions(conn)?;
    let devices = device_breakdown(conn)?;

    let mut cost = Neumaier::default();
    for row in &daily {
        cost.add(number_at(row, "cost_usd"));
    }
    let mut totals = Map::new();
    totals.insert("cost_usd".into(), cost.to_json());
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read",
        "cache_create",
        "message_count",
    ] {
        // Sums over `int`s: CPython's `sum` is exact there and an empty
        // iterable is likewise the `int` 0, so a plain integer accumulator IS
        // the faithful port.
        let total: i64 = daily.iter().map(|row| integer_at(row, key)).sum();
        totals.insert(key.to_owned(), Value::from(total));
    }
    // NOT from `daily`: the deduped unique session count across devices.
    totals.insert("session_count".into(), Value::from(sessions.len()));

    // `by_day` accumulates with `+=` from a literal `0.0` / `0`. Plain, on
    // purpose — the counter-example sitting four lines from the `sum()`.
    let mut by_day: BTreeMap<String, (f64, i64, i64, i64)> = BTreeMap::new();
    for row in &daily {
        let day = string_at(row, "day");
        let bucket = by_day.entry(day).or_insert((0.0, 0, 0, 0));
        bucket.0 += number_at(row, "cost_usd");
        bucket.1 += integer_at(row, "input_tokens");
        bucket.2 += integer_at(row, "output_tokens");
        bucket.3 += integer_at(row, "message_count");
    }
    let by_day_rows: Vec<Value> = by_day
        .into_iter()
        .map(|(day, (cost_usd, input, output, messages))| {
            let mut obj = Map::new();
            obj.insert("day".into(), Value::from(day));
            obj.insert("cost_usd".into(), PyValue::Float(cost_usd).to_json());
            obj.insert("input_tokens".into(), Value::from(input));
            obj.insert("output_tokens".into(), Value::from(output));
            obj.insert("message_count".into(), Value::from(messages));
            Value::Object(obj)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("totals".into(), Value::Object(totals));
    payload.insert("by_day".into(), Value::Array(by_day_rows));
    payload.insert(
        "by_project".into(),
        Value::Array(projects.into_iter().map(Value::Object).collect()),
    );
    payload.insert(
        "by_provider_day".into(),
        Value::Array(provider_day.into_iter().map(Value::Object).collect()),
    );
    payload.insert("devices".into(), Value::Array(devices));
    payload.insert("merge_warnings".into(), Value::from(merge_warnings));
    Ok(payload)
}

fn number_at(row: &Map<String, Value>, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn integer_at(row: &Map<String, Value>, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn string_at(row: &Map<String, Value>, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(include_str!("../tests/fixture-schema.sql"))
            .expect("schema");
        conn
    }

    #[test]
    fn an_empty_union_renders_the_integer_zero_for_the_summed_cost() {
        // DIV-057 in the shape this module meets it.
        assert_eq!(Neumaier::default().to_json(), Value::from(0));
        assert_eq!(
            stax_memory::pyjson::dumps_compact(&Neumaier::default().to_json()),
            "0"
        );
        // …while a `by_day` bucket starts at a literal `0.0` and stays a float.
        assert_eq!(
            stax_memory::pyjson::dumps_compact(&PyValue::Float(0.0).to_json()),
            "0.0"
        );
    }

    #[test]
    fn the_totals_cost_is_compensated_and_the_by_day_cost_is_not() {
        let values = [1e16, 1.0, -1e16, 1.0];
        let mut acc = Neumaier::default();
        let mut plain = 0.0_f64;
        for value in values {
            acc.add(value);
            plain += value;
        }
        assert!((acc.finish() - 2.0).abs() < f64::EPSILON, "sum() is exact");
        assert!(
            (plain - 2.0).abs() > f64::EPSILON,
            "+= drifts, and the port must drift with it"
        );
    }

    #[test]
    fn merged_overview_on_an_empty_store_is_the_documented_empty_shape() {
        let conn = fixture();
        let payload = merged_overview(&conn).expect("merge");
        assert_eq!(
            stax_memory::pyjson::dumps_compact(&Value::Object(payload)),
            concat!(
                r#"{"totals":{"cost_usd":0,"input_tokens":0,"output_tokens":0,"cache_read":0,"#,
                r#""cache_create":0,"message_count":0,"session_count":0},"by_day":[],"#,
                r#""by_project":[],"by_provider_day":[],"devices":[{"device_uuid":"(local)","#,
                r#""alias":null,"is_local":true,"projects":0,"cost_usd":0.0}],"merge_warnings":0}"#
            )
        );
    }

    #[test]
    fn a_session_on_two_devices_is_counted_once_and_warned_about() {
        let conn = fixture();
        conn.execute_batch(
            "
            INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified)
            VALUES (1, 'claude', 'proj-a', 'A', 0.0, 0.0);
            INSERT INTO session_mart
              (session_id, project_id, provider, primary_model, first_ts, last_ts, cwd,
               message_count, user_message_count, assistant_message_count, input_tokens,
               output_tokens, cache_read, cache_create, cost_usd, is_one_shot)
            VALUES ('shared', 1, 'claude', 'opus', '2026-07-01', '2026-07-01', '/x',
                    2, 1, 1, 10, 20, 0, 0, 1.5, 0);
            INSERT INTO session_mart_remote
              (device_uuid, session_id, provider, slug, primary_model, first_ts, last_ts,
               message_count, user_message_count, assistant_message_count, input_tokens,
               output_tokens, cache_read, cache_create, cost_usd, is_one_shot)
            VALUES ('bbbb', 'shared', 'claude', 'proj-a', 'opus', '2026-07-01', '2026-07-01',
                    2, 1, 1, 10, 20, 0, 0, 1.5, 0),
                   ('aaaa', 'other', 'claude', 'proj-a', 'opus', '2026-07-01', '2026-07-01',
                    1, 1, 0, 5, 5, 0, 0, 0.5, 1);
            ",
        )
        .expect("seed");
        let (rows, warnings) = unioned_sessions(&conn).expect("sessions");
        assert_eq!(rows.len(), 2, "two distinct session ids");
        assert_eq!(warnings, 1, "one duplicate dropped");
        // Local wins the tiebreak: the surviving `shared` row carries `''`.
        let shared = rows
            .iter()
            .find(|row| row.get("session_id") == Some(&Value::from("shared")))
            .expect("shared row");
        assert_eq!(shared.get("device_uuid"), Some(&Value::from("")));
    }

    #[test]
    fn the_device_breakdown_leads_with_the_local_row() {
        let conn = fixture();
        conn.execute_batch(
            "
            INSERT INTO project_mart (provider, slug, display_name, total_cost_usd)
            VALUES ('claude', 'a', 'A', 2.5);
            INSERT INTO project_mart_remote (device_uuid, provider, slug, display_name, total_cost_usd)
            VALUES ('bbbb', 'claude', 'b', 'B', 1.0), ('aaaa', 'claude', 'c', 'C', 3.0);
            INSERT INTO sync_remote_devices
              (remote_device_uuid, alias, key_fingerprint, first_seen, last_seen, last_generation)
            VALUES ('aaaa', 'work-mac', 'fp', 't', 't', 1);
            ",
        )
        .expect("seed");
        let devices = device_breakdown(&conn).expect("devices");
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0]["device_uuid"], Value::from("(local)"));
        assert_eq!(devices[0]["is_local"], Value::Bool(true));
        // Peers sort by uuid, and the alias comes from `sync_remote_devices`.
        assert_eq!(devices[1]["device_uuid"], Value::from("aaaa"));
        assert_eq!(devices[1]["alias"], Value::from("work-mac"));
        assert_eq!(devices[2]["device_uuid"], Value::from("bbbb"));
        assert_eq!(devices[2]["alias"], Value::Null);
    }

    #[test]
    fn two_devices_disjoint_contributions_sum_at_the_stable_grain() {
        let conn = fixture();
        conn.execute_batch(
            "
            INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified)
            VALUES (7, 'claude', 'proj-a', 'A', 0.0, 0.0);
            INSERT INTO daily_mart
              (project_id, day, provider, model, speed, input_tokens, output_tokens,
               cache_read, cache_create, message_count, session_count, cost_usd)
            VALUES (7, '2026-07-01', 'claude', 'opus', 'standard', 10, 20, 0, 0, 2, 1, 1.5);
            INSERT INTO daily_mart_remote
              (device_uuid, day, provider, slug, model, speed, input_tokens, output_tokens,
               cache_read, cache_create, message_count, session_count, cost_usd)
            VALUES ('bbbb', '2026-07-01', 'claude', 'proj-a', 'opus', 'standard',
                    5, 5, 0, 0, 1, 1, 0.5);
            ",
        )
        .expect("seed");
        let rows = unioned_daily(&conn).expect("daily");
        assert_eq!(
            rows.len(),
            1,
            "the local id 7 and the remote slug re-key to one grain"
        );
        assert_eq!(rows[0]["input_tokens"], Value::from(15));
        assert_eq!(rows[0]["session_count"], Value::from(2));
        assert_eq!(
            stax_memory::pyjson::dumps_compact(&rows[0]["cost_usd"]),
            "2.0"
        );
    }

    #[test]
    fn remote_row_count_reads_every_landing_table() {
        let conn = fixture();
        assert_eq!(remote_row_count(&conn).expect("count"), 0);
        conn.execute_batch(
            "INSERT INTO project_mart_remote (device_uuid, provider, slug) VALUES ('a', 'c', 's');",
        )
        .expect("seed");
        assert_eq!(remote_row_count(&conn).expect("count"), 1);
    }
}

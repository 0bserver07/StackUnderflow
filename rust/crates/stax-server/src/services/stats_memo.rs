//! `routes/cost.py::_project_stats_cached` — DIV-055, ported.
//!
//! This is the **only place in the campaign where the reference is faster than
//! the port**, and it was filed as such: cold, python 140 ms against the port's
//! 68 ms; warm, python 5–12 ms against the port's 43–46 ms, because every
//! request re-ran the whole collector sweep. The maintainer ruled *port the
//! memo*, so here it is — the same key, the same validator, the same bound, the
//! same eviction order.
//!
//! ```python
//! _STATS_CACHE: OrderedDict[
//!     tuple[str, str, int, tuple[int, ...]],
//!     tuple[tuple[str | None, int], dict],
//! ] = OrderedDict()
//! _STATS_CACHE_LOCK = threading.Lock()
//! _STATS_CACHE_MAX = 8
//! ```
//!
//! # Why this is safe to port at all, and why DIV-091's hazard is not here
//!
//! DIV-091 records the *other* memo — `/api/plan`'s `_SPEND_CACHE` — as **not
//! purely a latency device**: it is keyed on `(store, period_start, period_end)`
//! and validated against the store's mtime, while `_spend_daily_window` reads
//! `date.today()` and the clock is *not* in the key. A server that crosses local
//! midnight with no intervening ingest serves yesterday's window. That memo is
//! deliberately still unported, and the port is more correct on that boundary.
//!
//! The mission asked whether this memo shares the defect. **It does not, and the
//! check is mechanical rather than a reading**: the cached value is
//! `queries.get_project_stats(conn, project_id=…, tz_offset=…)`, whose whole
//! body is `build_enriched_dataset` → `formatter.to_dicts` → `aggregator.summarise`.
//! `grep -n 'now(\|today()\|utcnow' python-legacy: stats/aggregator.py` is
//! **empty**: the aggregation is a pure function of (the stored rows, the
//! tz offset), both of which *are* in the key or the signature. Every rolling
//! window in the response — `days`, the `?range=` presets, the daily-stats cap —
//! is applied by the handler *after* the memo returns, so no clock is ever
//! inside the cached value. That is the property DIV-091 lacks, stated as the
//! thing that was checked rather than as a conclusion.
//!
//! # The signature is what makes a hit safe
//!
//! Every hit is re-validated against `_stats_signature` — `(MAX(last_ts),
//! SUM(message_count))` over the project's sessions — recomputed with one SQL
//! statement on *every* call. Ingest moves it the moment it writes, so a stale
//! entry cannot outlive an ingest and a warm hit can never serve a different
//! answer than a cold miss. That is why the endpoint matrix cannot see this
//! change at all: it is invisible by construction, and the perf row is the only
//! evidence it is doing anything.
//!
//! # The four things a "just add a cache" would have got wrong
//!
//! 1. **The tz clamp is inside, not outside.** `_clamp_tz_offset` runs *before*
//!    the key is built AND before `get_project_stats` is called, so the clamped
//!    value lands in both. An unclamped key would let a client mint unbounded
//!    multi-megabyte entries by incrementing the query parameter — the reason
//!    the clamp exists (COST-5b).
//! 2. **The id tuple is `sorted()` and part of the key.** A provider-narrowed
//!    subset of a multi-provider slug must not collide with the slug's
//!    all-provider entry.
//! 3. **Eviction is LRU, not FIFO.** A hit does `move_to_end`; the eviction is
//!    `popitem(last=False)`. Entries are 5.5–19 MB, so which one goes is not an
//!    academic question.
//! 4. **The caller gets a copy.** `/api/cost-data` rebinds `tool_costs`,
//!    `_strip_heavy_blocks` mutates nested structures in place, and the currency
//!    walk rewrites every cost leaf. Handing back the shared entry would poison
//!    it for every later reader. `Value::clone` is a deep copy, and `keys`
//!    narrows *what is copied* while the entry always holds the whole dict.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde_json::{Map, Value};

/// `_STATS_CACHE_MAX`.
const STATS_CACHE_MAX: usize = 8;

/// `_TZ_OFFSET_MIN` / `_TZ_OFFSET_MAX` — minutes east of UTC, UTC-12:00…UTC+14:00.
pub const TZ_OFFSET_MIN: i64 = -720;
/// See [`TZ_OFFSET_MIN`].
pub const TZ_OFFSET_MAX: i64 = 840;

/// `(str(deps.store_path), slug, tz_offset, tuple(sorted(project_ids)))`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    store_path: String,
    slug: String,
    tz_offset: i64,
    project_ids: Vec<i64>,
}

/// `(row["max_ts"], int(row["n"] or 0))`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    max_last_ts: Option<String>,
    message_count: i64,
}

#[derive(Debug)]
struct Entry {
    key: Key,
    signature: Signature,
    stats: Value,
}

/// The memo. One per [`crate::AppState`], never a process global.
///
/// Python's is a module-level `OrderedDict` behind a `threading.Lock`; the
/// injection law (`ARCHITECT-STATE` finding 5) makes it state instead, which is
/// also what lets the parity harness run two servers in one process without
/// them sharing a cache.
#[derive(Debug, Default)]
pub struct StatsMemo {
    entries: Mutex<VecDeque<Entry>>,
}

impl StatsMemo {
    /// A fresh, empty memo.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `_invalidate_stats_cache(slug)` — `None` clears everything.
    ///
    /// Wired at the same sites Python wires it: the two `cfg` model-alias
    /// writers and the two `/api/refresh` paths. Alias edits change how rows are
    /// *aggregated* without moving the sessions signature, so this is the only
    /// thing that can drop them; before Python wired it up, editing an alias
    /// kept serving pre-alias aggregation until the next ingest.
    pub fn invalidate(&self, slug: Option<&str>) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        match slug {
            None => entries.clear(),
            Some(slug) => entries.retain(|entry| entry.key.slug != slug),
        }
    }

    /// Number of live entries — for the tests, and for nothing else.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    /// Is the memo empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A validated hit, with the LRU recency bump. `None` on a miss or a
    /// signature mismatch.
    fn get(&self, key: &Key, signature: &Signature) -> Option<Value> {
        let mut entries = self.entries.lock().ok()?;
        let index = entries.iter().position(|entry| &entry.key == key)?;
        if &entries[index].signature != signature {
            return None;
        }
        // `move_to_end` — the back of the deque is the most recently used.
        let entry = entries.remove(index)?;
        let stats = entry.stats.clone();
        entries.push_back(entry);
        Some(stats)
    }

    /// `_STATS_CACHE[key] = (sig, stats)` + `move_to_end` + the eviction loop.
    fn put(&self, key: Key, signature: Signature, stats: &Value) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.retain(|entry| entry.key != key);
        entries.push_back(Entry {
            key,
            signature,
            stats: stats.clone(),
        });
        while entries.len() > STATS_CACHE_MAX {
            // `popitem(last=False)` — least recently used.
            entries.pop_front();
        }
    }
}

/// `_clamp_tz_offset` — minutes east of UTC, `[-720, 840]`.
#[must_use]
pub fn clamp_tz_offset(tz_offset: i64) -> i64 {
    tz_offset.clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX)
}

/// `_stats_signature(conn, project_ids)`.
///
/// # Errors
/// Any sqlite failure; the caller turns it into the `500` it already would have.
pub fn stats_signature(conn: &Connection, project_ids: &[i64]) -> rusqlite::Result<Signature> {
    if project_ids.is_empty() {
        // `if not project_ids: return (None, 0)` — no query at all.
        return Ok(Signature {
            max_last_ts: None,
            message_count: 0,
        });
    }
    let placeholders = vec!["?"; project_ids.len()].join(",");
    let sql = format!(
        "SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n \
         FROM sessions WHERE project_id IN ({placeholders})"
    );
    let params = rusqlite::params_from_iter(project_ids.iter());
    conn.query_row(&sql, params, |row| {
        Ok(Signature {
            max_last_ts: row.get::<_, Option<String>>(0)?,
            message_count: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        })
    })
}

/// `_copy_stats_subset(stats, keys)`.
///
/// `None` is the whole dict; a key list keeps only those top-level keys, in the
/// **list's** order, and silently skips ones that are missing. A non-object
/// value is copied whole, which is what `if not isinstance(stats, dict)` does.
#[must_use]
pub fn copy_stats_subset(stats: &Value, keys: Option<&[&str]>) -> Value {
    let (Some(keys), Value::Object(map)) = (keys, stats) else {
        return stats.clone();
    };
    let mut out = Map::new();
    for key in keys {
        if let Some(value) = map.get(*key) {
            out.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(out)
}

/// The four inputs that make a cache key, plus the copy narrowing.
///
/// A struct rather than five parameters because the five are the KEY, and a
/// caller that transposed `slug` and `store_path` would compile and then quietly
/// serve one project's stats under another's name. Named fields make that
/// unwriteable.
#[derive(Debug, Clone, Copy)]
pub struct StatsRequest<'a> {
    /// `str(deps.store_path)`.
    pub store_path: &'a Path,
    /// `Path(log_path).name`.
    pub slug: &'a str,
    /// The project rows this slug resolves to — one per provider.
    pub project_ids: &'a [i64],
    /// Raw, un-clamped: `_clamp_tz_offset` runs inside, as it does in Python.
    pub tz_offset: i64,
    /// `keys=` — `None` copies the whole dict.
    pub keys: Option<&'a [&'a str]>,
}

/// `_project_stats_cached` — the whole function.
///
/// `compute` is the cold path: it is handed the **clamped** offset and must be
/// `get_project_stats_with(conn, project_ids, tz_offset, engine)`. Passing it in
/// rather than calling it here keeps the pricing engine — and therefore
/// DIV-056's price-book seam — in the route module that already owns it.
///
/// `map_sql` exists because the three callers spell their `500` three different
/// ways (`any_500`, `sql_500`, an inline `HttpError::new`) and this module is
/// not the place to pick one; a blanket `E: From<rusqlite::Error>` would have
/// forced an impl on `HttpError` that nothing else wants.
///
/// # Errors
/// The signature query's, or whatever `compute` returns.
pub fn project_stats_cached<E, F, M>(
    memo: &StatsMemo,
    conn: &Connection,
    request: &StatsRequest<'_>,
    map_sql: M,
    compute: F,
) -> Result<Value, E>
where
    F: FnOnce(i64) -> Result<Value, E>,
    M: FnOnce(rusqlite::Error) -> E,
{
    let tz_offset = clamp_tz_offset(request.tz_offset);
    let signature = stats_signature(conn, request.project_ids).map_err(map_sql)?;
    let mut sorted_ids = request.project_ids.to_vec();
    sorted_ids.sort_unstable();
    let key = Key {
        store_path: request.store_path.display().to_string(),
        slug: request.slug.to_owned(),
        tz_offset,
        project_ids: sorted_ids,
    };

    if let Some(hit) = memo.get(&key, &signature) {
        // The copy happens OUTSIDE the lock in Python for the same reason it is
        // outside `StatsMemo::get`'s guard here: it is the expensive part, no
        // writer ever mutates a cached entry, and holding the lock through it
        // would serialise every reader behind one deep copy.
        return Ok(copy_stats_subset(&hit, request.keys));
    }

    let stats = compute(tz_offset)?;
    memo.put(key, signature, &stats);
    Ok(copy_stats_subset(&stats, request.keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sig(n: i64) -> Signature {
        Signature {
            max_last_ts: Some("2026-08-04".to_owned()),
            message_count: n,
        }
    }

    fn key(slug: &str, tz: i64, ids: &[i64]) -> Key {
        Key {
            store_path: "/store.db".to_owned(),
            slug: slug.to_owned(),
            tz_offset: tz,
            project_ids: ids.to_vec(),
        }
    }

    #[test]
    fn the_clamp_is_the_reference_range() {
        assert_eq!(clamp_tz_offset(0), 0);
        assert_eq!(clamp_tz_offset(-480), -480);
        assert_eq!(clamp_tz_offset(480), 480);
        assert_eq!(clamp_tz_offset(-99999), -720);
        assert_eq!(clamp_tz_offset(99999), 840);
        assert_eq!(clamp_tz_offset(i64::MAX), 840);
    }

    #[test]
    fn a_hit_needs_the_signature_to_match() {
        let memo = StatsMemo::new();
        memo.put(key("a", 0, &[1]), sig(10), &json!({"n": 1}));
        assert_eq!(
            memo.get(&key("a", 0, &[1]), &sig(10)),
            Some(json!({"n": 1}))
        );
        // Ingest wrote: same key, moved signature, no hit.
        assert_eq!(memo.get(&key("a", 0, &[1]), &sig(11)), None);
    }

    #[test]
    fn every_component_of_the_key_is_load_bearing() {
        let memo = StatsMemo::new();
        memo.put(key("a", 0, &[1]), sig(10), &json!({"n": 1}));
        assert!(memo.get(&key("b", 0, &[1]), &sig(10)).is_none(), "slug");
        assert!(memo.get(&key("a", 60, &[1]), &sig(10)).is_none(), "tz");
        // The id tuple: a provider-narrowed subset must not collide with the
        // slug's all-provider entry.
        assert!(memo.get(&key("a", 0, &[1, 2]), &sig(10)).is_none(), "ids");
    }

    #[test]
    fn eviction_is_least_recently_used_and_not_least_recently_inserted() {
        let memo = StatsMemo::new();
        for id in 0..STATS_CACHE_MAX as i64 {
            memo.put(key("s", 0, &[id]), sig(1), &json!({ "id": id }));
        }
        assert_eq!(memo.len(), STATS_CACHE_MAX);
        // Touch the OLDEST entry, then overflow by one. FIFO would drop the one
        // just touched; LRU drops the second-oldest.
        assert!(memo.get(&key("s", 0, &[0]), &sig(1)).is_some());
        memo.put(key("s", 0, &[99]), sig(1), &json!({"id": 99}));
        assert_eq!(memo.len(), STATS_CACHE_MAX);
        assert!(
            memo.get(&key("s", 0, &[0]), &sig(1)).is_some(),
            "the touched entry survives"
        );
        assert!(
            memo.get(&key("s", 0, &[1]), &sig(1)).is_none(),
            "the least recently USED entry is the one evicted"
        );
    }

    #[test]
    fn invalidation_is_scoped_by_slug_or_total() {
        let memo = StatsMemo::new();
        memo.put(key("a", 0, &[1]), sig(1), &json!({}));
        memo.put(key("b", 0, &[2]), sig(1), &json!({}));
        memo.invalidate(Some("a"));
        assert!(memo.get(&key("a", 0, &[1]), &sig(1)).is_none());
        assert!(memo.get(&key("b", 0, &[2]), &sig(1)).is_some());
        memo.invalidate(None);
        assert!(memo.is_empty());
    }

    #[test]
    fn the_subset_copy_omits_rather_than_defaults() {
        let stats = json!({"a": 1, "b": {"deep": [1, 2]}, "c": 3});
        assert_eq!(copy_stats_subset(&stats, None), stats);
        assert_eq!(
            copy_stats_subset(&stats, Some(&["b", "a"])),
            json!({"b": {"deep": [1, 2]}, "a": 1}),
            "the requested order, and `c` OMITTED rather than nulled"
        );
        // A key that is not there is skipped, not defaulted.
        assert_eq!(copy_stats_subset(&stats, Some(&["nope"])), json!({}));
        // A non-object entry copies whole.
        assert_eq!(
            copy_stats_subset(&json!([1, 2]), Some(&["a"])),
            json!([1, 2])
        );
    }

    #[test]
    fn the_caller_cannot_poison_the_entry() {
        // `/api/cost-data` rebinds `tool_costs` and `_strip_heavy_blocks`
        // mutates nested structures; the copy is what keeps the shared entry
        // intact for the next reader.
        let memo = StatsMemo::new();
        memo.put(key("a", 0, &[1]), sig(1), &json!({"tool_costs": {"x": 1}}));
        let mut mine = memo.get(&key("a", 0, &[1]), &sig(1)).expect("hit");
        mine["tool_costs"]["x"] = json!(999);
        assert_eq!(
            memo.get(&key("a", 0, &[1]), &sig(1)).expect("hit"),
            json!({"tool_costs": {"x": 1}})
        );
    }

    #[test]
    fn an_empty_id_list_signs_as_none_zero_without_touching_sqlite() {
        // `if not project_ids: return (None, 0)`. The connection is deliberately
        // one with no `sessions` table at all: reaching sqlite here would be an
        // error, and that is the assertion.
        let conn = Connection::open_in_memory().expect("memory db");
        let signature = stats_signature(&conn, &[]).expect("no query is run");
        assert_eq!(signature.max_last_ts, None);
        assert_eq!(signature.message_count, 0);
    }

    #[test]
    fn the_signature_is_max_last_ts_and_summed_message_count() {
        let conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            "CREATE TABLE sessions (project_id INTEGER, last_ts TEXT, message_count INTEGER);
             INSERT INTO sessions VALUES (1, '2026-01-01', 5), (1, '2026-03-02', 7),
                                         (2, '2026-09-09', 100);",
        )
        .expect("fixture");
        let signature = stats_signature(&conn, &[1]).expect("signature");
        assert_eq!(signature.max_last_ts.as_deref(), Some("2026-03-02"));
        assert_eq!(signature.message_count, 12);
        // The other project's rows are not in this project's signature.
        let both = stats_signature(&conn, &[1, 2]).expect("signature");
        assert_eq!(both.max_last_ts.as_deref(), Some("2026-09-09"));
        assert_eq!(both.message_count, 112);
    }

    #[test]
    fn the_cold_path_runs_once_and_the_warm_path_never_recomputes() {
        let conn = Connection::open_in_memory().expect("memory db");
        let memo = StatsMemo::new();
        let mut runs = 0_u32;
        let store = Path::new("/store.db");

        for _ in 0..3 {
            let stats = project_stats_cached::<rusqlite::Error, _, _>(
                &memo,
                &conn,
                &StatsRequest {
                    store_path: store,
                    slug: "slug",
                    project_ids: &[],
                    tz_offset: -480,
                    keys: None,
                },
                |err| err,
                |tz| {
                    runs += 1;
                    // The clamp reaches the COMPUTE, not just the key.
                    assert_eq!(tz, -480);
                    Ok(json!({"total": 1}))
                },
            )
            .expect("stats");
            assert_eq!(stats, json!({"total": 1}));
        }
        assert_eq!(runs, 1, "two warm reads recomputed nothing");
    }

    #[test]
    fn an_absurd_offset_is_clamped_before_both_the_key_and_the_call() {
        let conn = Connection::open_in_memory().expect("memory db");
        let memo = StatsMemo::new();
        let store = Path::new("/store.db");
        let mut seen = Vec::new();
        for offset in [99_999, 840, 1_000_000] {
            let _ = project_stats_cached::<rusqlite::Error, _, _>(
                &memo,
                &conn,
                &StatsRequest {
                    store_path: store,
                    slug: "slug",
                    project_ids: &[],
                    tz_offset: offset,
                    keys: None,
                },
                |err| err,
                |tz| {
                    seen.push(tz);
                    Ok(json!({}))
                },
            )
            .expect("stats");
        }
        // All three clamp to 840, so all three are ONE key and one computation.
        assert_eq!(seen, vec![840]);
        assert_eq!(memo.len(), 1);
    }
}

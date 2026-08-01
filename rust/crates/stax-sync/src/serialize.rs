//! `sync/serialize.py` — canonical shard bytes, content hashes, and the re-key.
//!
//! A **shard** is one `(mart family, month)`. Its canonical byte form drives the
//! SHA-256 that push idempotency and pull cursors both key off, so this module
//! is the one place in the crate where "shape-identical" is not good enough:
//! one different escape, one different float repr, one different key order and
//! the two implementations compute different hashes for the same data, push
//! never converges, and pull re-downloads forever.
//!
//! # Three exact things
//!
//! 1. **The writer is `ensure_ascii=False` + `(",", ":")`** — `json.dumps(
//!    payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`. That
//!    is [`stax_memory::pyjson::dumps_http`]'s flag combination, not
//!    `dumps_compact`'s (wave 5's finding 11 — the two differ in exactly one
//!    flag and the maintainer's store already carries a non-ASCII
//!    `display_name`). A `project_mart` shard carries `display_name`, so this
//!    is live on the real data, not a hypothetical.
//! 2. **`sort_keys=True` is applied recursively by CPython**, so the payload's
//!    five keys ship as `columns, family, month, rows, v`. Built in that order
//!    here rather than sorted afterwards, because `preserve_order` means
//!    insertion order *is* the output order.
//! 3. **Storage classes survive.** `sqlite3.Row` hands Python whatever SQLite
//!    stored, so an `INTEGER` renders `8` and a `REAL` renders `8.0`. The rows
//!    go into the hash, so flattening either way changes every hash.
//!
//! # The re-key (§4.5)
//!
//! `projects.id` is a machine-local autoincrement, so every mart that carries
//! one is JOINed to `projects` and re-grouped at the stable `(provider, slug)`.
//! Two machines that assign different local ids to the same project therefore
//! produce identical shard bytes — the property the cross-device union relies
//! on. `session_mart.cwd` is a filesystem path and is dropped at this boundary;
//! `usage_events` and `price_book` are never read here at all.

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use sha2::{Digest as _, Sha256};

use crate::pyvalue::PyValue;

/// A mart family's export query, canonical columns, and month grouping.
#[derive(Debug, Clone, Copy)]
pub struct MartSpec {
    /// `daily_mart`, `project_mart`, …
    pub family: &'static str,
    /// The canonical shard columns, in `SELECT` order.
    pub columns: &'static [&'static str],
    /// The export query. `SELECT` column order MUST match `columns`.
    pub sql: &'static str,
    /// Column whose `YYYY-MM` prefix buckets rows into monthly shards.
    /// `None` means the whole mart is a single `"all"` shard.
    pub month_column: Option<&'static str>,
}

/// `_SPECS` — the five Overview/Cost-core marts, in the reference's order.
///
/// The SQL is transcribed character for character including the whitespace,
/// because these strings are also what a `EXPLAIN QUERY PLAN` assertion would
/// key off and §6b makes SQL *shape* load-bearing for this campaign.
pub const SPECS: &[MartSpec] = &[
    MartSpec {
        family: "daily_mart",
        columns: &[
            "day",
            "provider",
            "slug",
            "model",
            "speed",
            "input_tokens",
            "output_tokens",
            "cache_read",
            "cache_create",
            "message_count",
            "session_count",
            "cost_usd",
        ],
        sql: "SELECT d.day, d.provider, p.slug, d.model, d.speed, \
              SUM(d.input_tokens), SUM(d.output_tokens), \
              SUM(d.cache_read), SUM(d.cache_create), \
              SUM(d.message_count), SUM(d.session_count), SUM(d.cost_usd) \
              FROM daily_mart d JOIN projects p ON p.id = d.project_id \
              GROUP BY d.day, d.provider, p.slug, d.model, d.speed \
              ORDER BY d.day, d.provider, p.slug, d.model, d.speed",
        month_column: Some("day"),
    },
    MartSpec {
        family: "provider_day_mart",
        columns: &[
            "day",
            "provider",
            "cost_usd",
            "message_count",
            "session_count",
            "project_count",
        ],
        sql: "SELECT day, provider, cost_usd, message_count, session_count, project_count \
              FROM provider_day_mart \
              ORDER BY day, provider",
        month_column: Some("day"),
    },
    MartSpec {
        family: "model_day_mart",
        columns: &[
            "day",
            "model",
            "speed",
            "cost_usd",
            "input_tokens",
            "output_tokens",
            "cache_read",
            "cache_create",
            "message_count",
            "session_count",
        ],
        sql: "SELECT day, model, speed, cost_usd, input_tokens, output_tokens, \
              cache_read, cache_create, message_count, session_count \
              FROM model_day_mart \
              ORDER BY day, model, speed",
        month_column: Some("day"),
    },
    MartSpec {
        family: "project_mart",
        columns: &[
            "provider",
            "slug",
            "display_name",
            "first_ts",
            "last_ts",
            "total_messages",
            "total_sessions",
            "total_input_tokens",
            "total_output_tokens",
            "total_cache_read",
            "total_cache_create",
            "total_cost_usd",
        ],
        sql: "SELECT provider, slug, display_name, first_ts, last_ts, \
              SUM(total_messages), SUM(total_sessions), SUM(total_input_tokens), \
              SUM(total_output_tokens), SUM(total_cache_read), \
              SUM(total_cache_create), SUM(total_cost_usd) \
              FROM project_mart \
              GROUP BY provider, slug, display_name, first_ts, last_ts \
              ORDER BY provider, slug",
        month_column: None,
    },
    MartSpec {
        family: "session_mart",
        columns: &[
            "session_id",
            "provider",
            "slug",
            "primary_model",
            "first_ts",
            "last_ts",
            "message_count",
            "user_message_count",
            "assistant_message_count",
            "input_tokens",
            "output_tokens",
            "cache_read",
            "cache_create",
            "cost_usd",
            "is_one_shot",
        ],
        sql: "SELECT s.session_id, s.provider, p.slug, s.primary_model, \
              s.first_ts, s.last_ts, s.message_count, s.user_message_count, \
              s.assistant_message_count, s.input_tokens, s.output_tokens, \
              s.cache_read, s.cache_create, s.cost_usd, s.is_one_shot \
              FROM session_mart s JOIN projects p ON p.id = s.project_id \
              ORDER BY s.session_id",
        month_column: Some("first_ts"),
    },
];

/// `MART_FAMILIES` — the families the MVP syncs, in a stable order.
#[must_use]
pub fn mart_families() -> Vec<&'static str> {
    SPECS.iter().map(|spec| spec.family).collect()
}

/// `SHARD_COLUMNS[family]` — the canonical column list, or `None` if unknown.
#[must_use]
pub fn shard_columns(family: &str) -> Option<&'static [&'static str]> {
    SPECS
        .iter()
        .find(|spec| spec.family == family)
        .map(|spec| spec.columns)
}

/// `MONTH_COLUMN[family]` — `Some(None)` is "a known family with no month".
///
/// The double option is the reference's own distinction: `MONTH_COLUMN` is a
/// dict whose *values* can be `None`, and `pull` indexes it only after checking
/// membership in `MART_FAMILIES`. Collapsing the two would make an unknown
/// family look like `project_mart`.
#[must_use]
pub fn month_column(family: &str) -> Option<Option<&'static str>> {
    SPECS
        .iter()
        .find(|spec| spec.family == family)
        .map(|spec| spec.month_column)
}

/// `remote_table(family)` — `daily_mart` → `daily_mart_remote`.
#[must_use]
pub fn remote_table(family: &str) -> String {
    format!("{family}_remote")
}

/// `FORMAT_VERSION` — embedded in each shard's canonical bytes.
pub const FORMAT_VERSION: i64 = 1;

/// One `(family, month)` shard of re-keyed, canonically-ordered mart rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Shard {
    /// `daily_mart`, `project_mart`, …
    pub family: String,
    /// `YYYY-MM`, or `all` for month-less marts.
    pub month: String,
    /// The canonical columns, in order.
    pub columns: Vec<String>,
    /// The rows, each already in `columns` order.
    pub rows: Vec<Vec<PyValue>>,
}

impl Shard {
    /// `shard_key` — `daily_mart.2026-07` / `project_mart.all`.
    #[must_use]
    pub fn shard_key(&self) -> String {
        format!("{}.{}", self.family, self.month)
    }

    /// `to_bytes()` — the canonical, deterministic serialization.
    ///
    /// `sort_keys=True` puts the five payload keys in `columns, family, month,
    /// rows, v` order; the writer is `ensure_ascii=False` with `(",", ":")`.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let payload = serde_json::json!({
            "columns": self.columns,
            "family": self.family,
            "month": self.month,
            "rows": self
                .rows
                .iter()
                .map(|row| row.iter().map(PyValue::to_json).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "v": FORMAT_VERSION,
        });
        stax_memory::pyjson::dumps_http(&payload).into_bytes()
    }

    /// `content_hash` — SHA-256 hex digest of the canonical bytes.
    #[must_use]
    pub fn content_hash(&self) -> String {
        hex_digest(&self.to_bytes())
    }
}

/// `hashlib.sha256(data).hexdigest()`.
#[must_use]
pub fn hex_digest(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `_month_of(value)` — the `YYYY-MM` prefix, or `"unknown"`.
///
/// `str(value)` first: the reference stringifies whatever SQLite handed it, so
/// a `NULL` `first_ts` becomes the literal `"None"` (four characters, `>= 7`
/// false) and buckets as `unknown`. An integer `202607` would stringify to six
/// characters and also bucket as `unknown`. Both are reproduced through
/// [`PyValue::py_str`] rather than being special-cased.
///
/// The `len(text) >= 7` test counts *characters*; `text[:7]` slices characters
/// too. Every value that reaches here is a date string, but a port that used
/// byte indexing would panic on a multi-byte prefix instead of producing the
/// reference's answer, so this walks chars.
#[must_use]
pub fn month_of(value: &PyValue) -> String {
    let text = value.py_str();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= 7 {
        chars[..7].iter().collect()
    } else {
        "unknown".to_owned()
    }
}

/// `build_shards(conn)` — every current mart shard, re-keyed to `(provider, slug)`.
///
/// Read-only: only the mart tables and `projects`.
///
/// # Errors
/// Any SQLite failure. A store missing the mart tables raises here exactly as
/// the reference's `conn.execute` does — there is deliberately no
/// `table_exists` guard, because a guard would let a half-migrated store push a
/// silently short manifest.
pub fn build_shards(conn: &Connection) -> rusqlite::Result<Vec<Shard>> {
    let mut shards = Vec::new();
    for spec in SPECS {
        let rows = query_rows(conn, spec.sql)?;
        let Some(month_col) = spec.month_column else {
            // `if rows:` — a month-less mart with no rows produces NO shard at
            // all, where a month-ful one simply produces no groups. The
            // asymmetry is the reference's and it is visible in `shard_count`.
            if !rows.is_empty() {
                shards.push(Shard {
                    family: spec.family.to_owned(),
                    month: "all".to_owned(),
                    columns: spec.columns.iter().map(|c| (*c).to_owned()).collect(),
                    rows,
                });
            }
            continue;
        };
        let month_idx = spec
            .columns
            .iter()
            .position(|c| *c == month_col)
            .expect("month_column is one of columns — pinned by a test below");
        // `defaultdict(list)` then `for month in sorted(by_month)`. A BTreeMap
        // gives the sort for free, and the keys are ASCII `YYYY-MM` (plus the
        // literal `unknown`, which sorts after every digit).
        let mut by_month: std::collections::BTreeMap<String, Vec<Vec<PyValue>>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let month = month_of(&row[month_idx]);
            by_month.entry(month).or_default().push(row);
        }
        for (month, rows) in by_month {
            shards.push(Shard {
                family: spec.family.to_owned(),
                month,
                columns: spec.columns.iter().map(|c| (*c).to_owned()).collect(),
                rows,
            });
        }
    }
    Ok(shards)
}

/// `shard_from_bytes(data)` — the inverse of [`Shard::to_bytes`].
///
/// Deliberately as trusting as the reference: `json.loads` then four
/// subscripts. A missing key is a `KeyError` there and an `Err` here, and
/// `pull` catches both into the same warning.
///
/// # Errors
/// Malformed JSON, or a payload missing `family` / `month` / `columns` / `rows`.
pub fn shard_from_bytes(data: &[u8]) -> Result<Shard, String> {
    // `json.loads` — through [`crate::pyerr`], not `serde_json::from_slice`.
    // This error text is INTERPOLATED into `pull`'s warning list, which
    // `sync pull --json` prints; serde's "expected value at line 1 column 1" is
    // not CPython's "Expecting value: line 1 column 1 (char 0)", and the
    // differ's `V-pull-corrupt` row is the one that caught it.
    let value = crate::pyerr::loads(data).map_err(|err| err.to_string())?;
    let family = value
        .get("family")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "'family'".to_owned())?
        .to_owned();
    let month = value
        .get("month")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "'month'".to_owned())?
        .to_owned();
    let columns = value
        .get("columns")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "'columns'".to_owned())?
        .iter()
        .map(|c| c.as_str().unwrap_or_default().to_owned())
        .collect();
    let rows = value
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "'rows'".to_owned())?
        .iter()
        .map(|row| {
            row.as_array()
                .map(|cells| cells.iter().map(PyValue::from_json).collect())
                .unwrap_or_default()
        })
        .collect();
    Ok(Shard {
        family,
        month,
        columns,
        rows,
    })
}

/// `[tuple(r) for r in conn.execute(sql).fetchall()]`, storage classes intact.
fn query_rows(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Vec<PyValue>>> {
    let mut stmt = conn.prepare(sql)?;
    let width = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut cells = Vec::with_capacity(width);
        for index in 0..width {
            cells.push(PyValue::from_sqlite(row.get_ref(index)?));
        }
        out.push(cells);
    }
    Ok(out)
}

/// The `ValueRef` → [`PyValue`] mapping, exposed for `runner`'s landing path.
#[must_use]
pub fn cell_from_sqlite(value: ValueRef<'_>) -> PyValue {
    PyValue::from_sqlite(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_specs_month_column_is_one_of_its_columns() {
        // `spec.columns.index(spec.month_column)` raises in the reference if it
        // is not; here it would panic in `build_shards`. Caught at test time.
        for spec in SPECS {
            if let Some(month) = spec.month_column {
                assert!(
                    spec.columns.contains(&month),
                    "{}: {month} not in columns",
                    spec.family
                );
            }
        }
    }

    #[test]
    fn the_family_list_and_its_order_are_the_references() {
        assert_eq!(
            mart_families(),
            vec![
                "daily_mart",
                "provider_day_mart",
                "model_day_mart",
                "project_mart",
                "session_mart",
            ]
        );
    }

    #[test]
    fn the_canonical_payload_keys_are_sorted_not_declaration_ordered() {
        let shard = Shard {
            family: "daily_mart".to_owned(),
            month: "2026-07".to_owned(),
            columns: vec!["day".to_owned()],
            rows: vec![vec![PyValue::Str("2026-07-01".to_owned())]],
        };
        assert_eq!(
            String::from_utf8(shard.to_bytes()).expect("utf8"),
            r#"{"columns":["day"],"family":"daily_mart","month":"2026-07","rows":[["2026-07-01"]],"v":1}"#
        );
    }

    #[test]
    fn the_shard_writer_is_ensure_ascii_false() {
        // Wave 5's finding 11, met again in a place where it changes a HASH
        // rather than a response body. `display_name` on the maintainer's own
        // store carries an em-dash; escaping it would make Python and Rust
        // compute different content hashes for identical data.
        let shard = Shard {
            family: "project_mart".to_owned(),
            month: "all".to_owned(),
            columns: vec!["display_name".to_owned()],
            rows: vec![vec![PyValue::Str("cursor — main".to_owned())]],
        };
        let bytes = shard.to_bytes();
        assert!(
            String::from_utf8(bytes.clone())
                .expect("utf8")
                .contains('—'),
            "the em-dash ships raw, not as \\u2014"
        );
        // And the three raw UTF-8 bytes are actually in the hashed buffer.
        assert!(bytes.windows(3).any(|w| w == [0xE2, 0x80, 0x94]));
    }

    #[test]
    fn integers_and_floats_keep_their_storage_class_in_the_hash() {
        let ints = Shard {
            family: "daily_mart".to_owned(),
            month: "2026-07".to_owned(),
            columns: vec!["message_count".to_owned()],
            rows: vec![vec![PyValue::Int(8)]],
        };
        let floats = Shard {
            rows: vec![vec![PyValue::Float(8.0)]],
            ..ints.clone()
        };
        assert!(
            String::from_utf8(ints.to_bytes())
                .expect("utf8")
                .contains("[[8]]")
        );
        assert!(
            String::from_utf8(floats.to_bytes())
                .expect("utf8")
                .contains("[[8.0]]")
        );
        assert_ne!(ints.content_hash(), floats.content_hash());
    }

    #[test]
    fn the_shard_key_is_family_dot_month() {
        let shard = Shard {
            family: "project_mart".to_owned(),
            month: "all".to_owned(),
            columns: vec![],
            rows: vec![],
        };
        assert_eq!(shard.shard_key(), "project_mart.all");
    }

    #[test]
    fn month_of_takes_the_first_seven_characters_or_says_unknown() {
        assert_eq!(
            month_of(&PyValue::Str("2026-07-31T12:00:00".to_owned())),
            "2026-07"
        );
        assert_eq!(month_of(&PyValue::Str("2026-07".to_owned())), "2026-07");
        assert_eq!(month_of(&PyValue::Str("2026-0".to_owned())), "unknown");
        // `str(None)` is `"None"` — four characters.
        assert_eq!(month_of(&PyValue::Null), "unknown");
        // `str(202607)` is six characters.
        assert_eq!(month_of(&PyValue::Int(202_607)), "unknown");
        // `str(20260731)` is eight — so an integer date DOES bucket, as
        // `2026073`. Bug-for-bug: the reference slices before it validates.
        assert_eq!(month_of(&PyValue::Int(20_260_731)), "2026073");
    }

    #[test]
    fn round_tripping_bytes_preserves_the_hash() {
        let shard = Shard {
            family: "session_mart".to_owned(),
            month: "2026-07".to_owned(),
            columns: vec!["session_id".to_owned(), "cost_usd".to_owned()],
            rows: vec![
                vec![PyValue::Str("s-1".to_owned()), PyValue::Float(1.5)],
                vec![PyValue::Str("s-2".to_owned()), PyValue::Int(0)],
            ],
        };
        let restored = shard_from_bytes(&shard.to_bytes()).expect("round trip");
        assert_eq!(restored, shard);
        assert_eq!(restored.content_hash(), shard.content_hash());
    }

    #[test]
    fn a_payload_missing_a_key_is_an_error_not_a_default() {
        assert!(shard_from_bytes(br#"{"family":"daily_mart"}"#).is_err());
        assert!(shard_from_bytes(b"not json").is_err());
    }

    #[test]
    fn the_remote_table_name_is_the_suffix_rule() {
        assert_eq!(remote_table("daily_mart"), "daily_mart_remote");
    }

    #[test]
    fn month_column_distinguishes_unknown_from_month_less() {
        assert_eq!(month_column("project_mart"), Some(None));
        assert_eq!(month_column("daily_mart"), Some(Some("day")));
        assert_eq!(month_column("message_tool_mart"), None);
    }

    #[test]
    fn message_tool_mart_is_deliberately_absent() {
        // It carries `file_path`. The spec excludes it and so does this port;
        // a family list that grew one would start shipping paths off-box.
        assert!(!mart_families().contains(&"message_tool_mart"));
    }

    #[test]
    fn the_digest_is_lowercase_hex_of_the_full_32_bytes() {
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

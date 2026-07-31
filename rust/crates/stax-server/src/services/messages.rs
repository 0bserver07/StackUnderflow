//! `api/messages.py` — the pagination envelope and the legacy summary.
//!
//! Four functions, no SQL, no state. They exist as a module rather than as
//! private helpers in `routes/data.rs` for the reason Python gives them their
//! own module: [`page_bounds`] is *the* definition of the pagination arithmetic
//! and three callers share it — the in-memory slicer
//! ([`get_paginated_messages`], which `/api/dashboard-data` uses on the first
//! 50), the SQL-paginated wrapper ([`build_messages_page`], which
//! `/api/messages` uses), and `/api/messages` itself, which calls it a second
//! time to compute the `OFFSET` it pushes into SQL. If those three ever
//! disagreed the envelope would report a page whose contents came from a
//! different one.
//!
//! # The clamp is asymmetric, and that is visible
//!
//! ```text
//! total_pages = (total + per_page - 1) // per_page
//! if page < 1:                page = 1
//! elif page > total_pages:    page = total_pages
//! ```
//!
//! On an EMPTY project `total_pages` is `0`, so `page > 0` clamps `page` to
//! **0** — a 1-indexed field returning zero. `start_index` then computes as
//! `(0 - 1) * per_page`, i.e. **negative**. Both reach the wire. Python does it,
//! so this does it (DIV-121); the alternative is a port that answers `page: 1`
//! where the reference answers `page: 0`, which is a divergence dressed as a
//! fix.
//!
//! # `end_index` is clamped by the caller, not here
//!
//! [`page_bounds`] returns the *raw* stop index (`start + per_page`); both
//! envelope builders emit `min(end_index, total)`. Keeping the raw value inside
//! is what lets `get_paginated_messages` use it directly as a slice bound.

use serde_json::{Map, Value};

/// `page_bounds(total, page, per_page)` → `(page, total_pages, start, end)`.
///
/// Integer division is Python's `//`, which floors. Every input here is
/// non-negative by the time it arrives (`per_page` is clamped to `[1, 500]` by
/// the route and `total` is a `COUNT(*)`), so floor and truncate agree — but
/// the clamp below can drive `page` to `0` and `start` negative, which is why
/// these are signed.
#[must_use]
pub fn page_bounds(total: i64, page: i64, per_page: i64) -> (i64, i64, i64, i64) {
    // `(total + per_page - 1) // per_page`. `per_page >= 1` at every call site.
    let total_pages = if per_page > 0 {
        (total + per_page - 1).div_euclid(per_page)
    } else {
        0
    };
    let page = if page < 1 {
        1
    } else if page > total_pages {
        // NOTE: on an empty project `total_pages == 0`, so this clamps a
        // 1-indexed page number to zero. See the module docs.
        total_pages
    } else {
        page
    };
    let start_idx = (page - 1) * per_page;
    let end_idx = start_idx + per_page;
    (page, total_pages, start_idx, end_idx)
}

/// `build_messages_page(page_messages, total=…, page=…, per_page=…)`.
///
/// Wraps rows that were already sliced in SQL. The key order below is the
/// wire contract — `preserve_order` is on and `json.dumps` writes a dict in
/// insertion order.
#[must_use]
pub fn build_messages_page(
    page_messages: Vec<Value>,
    total: i64,
    page: i64,
    per_page: i64,
) -> Value {
    let (page, total_pages, start_idx, end_idx) = page_bounds(total, page, per_page);
    let mut out = Map::new();
    out.insert("messages".to_owned(), Value::Array(page_messages));
    out.insert("total".to_owned(), Value::from(total));
    out.insert("page".to_owned(), Value::from(page));
    out.insert("per_page".to_owned(), Value::from(per_page));
    out.insert("total_pages".to_owned(), Value::from(total_pages));
    out.insert("start_index".to_owned(), Value::from(start_idx));
    out.insert("end_index".to_owned(), Value::from(end_idx.min(total)));
    Value::Object(out)
}

/// `get_paginated_messages(messages, page, per_page)` — the in-memory slicer.
///
/// `/api/dashboard-data` is the only caller in batch C's scope and it always
/// passes `page=1, per_page=50` with `include_all` left at its default, so the
/// `include_all` branch (which emits a *four*-key envelope with no
/// `start_index` / `end_index`) is not reachable from any ported endpoint. It
/// is ported anyway, as [`get_all_messages`], because leaving a shape out of a
/// shared module is how the next caller finds a hole.
///
/// The slice is Python's `messages[start:end]`, which never panics on
/// out-of-range bounds — a negative `start` (see the module docs) counts from
/// the END in Python. That is reproduced explicitly below rather than being
/// left to differ.
#[must_use]
pub fn get_paginated_messages(messages: Vec<Value>, page: i64, per_page: i64) -> Value {
    let total = i64::try_from(messages.len()).unwrap_or(i64::MAX);
    let (page, total_pages, start_idx, end_idx) = page_bounds(total, page, per_page);
    let page_messages = python_slice(messages, start_idx, end_idx);

    let mut out = Map::new();
    out.insert("messages".to_owned(), Value::Array(page_messages));
    out.insert("total".to_owned(), Value::from(total));
    out.insert("page".to_owned(), Value::from(page));
    out.insert("per_page".to_owned(), Value::from(per_page));
    out.insert("total_pages".to_owned(), Value::from(total_pages));
    out.insert("start_index".to_owned(), Value::from(start_idx));
    out.insert("end_index".to_owned(), Value::from(end_idx.min(total)));
    Value::Object(out)
}

/// `get_paginated_messages(..., include_all=True)` — the four-key envelope.
///
/// Unreachable from the ported endpoints; see [`get_paginated_messages`].
#[must_use]
pub fn get_all_messages(messages: Vec<Value>) -> Value {
    let total = i64::try_from(messages.len()).unwrap_or(i64::MAX);
    // `{"messages": messages, "total": …, "page": 1, "per_page": len(messages),
    //   "total_pages": 1}` — the literal's order is the wire's.
    let mut out = Map::new();
    out.insert("messages".to_owned(), Value::Array(messages));
    out.insert("total".to_owned(), Value::from(total));
    out.insert("page".to_owned(), Value::from(1));
    out.insert("per_page".to_owned(), Value::from(total));
    out.insert("total_pages".to_owned(), Value::from(1));
    Value::Object(out)
}

/// `messages[start:end]` with CPython's slice semantics.
///
/// Negative indices count from the end; both bounds are clamped into range and
/// an inverted range yields an empty list. None of that is decoration here:
/// [`page_bounds`] really does return a negative `start` on an empty project,
/// and Python really would wrap it.
fn python_slice(messages: Vec<Value>, start: i64, end: i64) -> Vec<Value> {
    let len = i64::try_from(messages.len()).unwrap_or(i64::MAX);
    let resolve = |index: i64| -> i64 {
        let shifted = if index < 0 { index + len } else { index };
        shifted.clamp(0, len)
    };
    let (start, end) = (resolve(start), resolve(end));
    if start >= end {
        return Vec::new();
    }
    #[allow(
        clippy::cast_sign_loss,
        reason = "both bounds are clamped to 0..=len above"
    )]
    messages[start as usize..end as usize].to_vec()
}

/// `get_messages_summary(messages)` — the legacy, pipeline-fed summary.
///
/// `/api/messages/summary` takes this path only when the mart gate fails; the
/// fast path is `_messages_summary_from_marts` in `routes/data.rs`. The two
/// bodies are deliberately different shapes — this one has **no**
/// `total_sessions` key — and that difference is on the wire, so it cannot be
/// smoothed over here.
///
/// The defaults are the ones a careless port drops: a record with no `type` is
/// counted under `"unknown"`, and so is a record with no `model`. The mart path
/// keys a model-less row `"N/A"` instead — a real, recorded inconsistency
/// between the two paths that this function must not "fix".
#[must_use]
pub fn get_messages_summary(messages: &[Value]) -> Value {
    // `if not messages:` — the early return omits `total_sessions` too, and it
    // renders `total_tokens` as int `0`.
    if messages.is_empty() {
        let mut out = Map::new();
        out.insert("total".to_owned(), Value::from(0));
        out.insert("by_type".to_owned(), Value::Object(Map::new()));
        out.insert("by_model".to_owned(), Value::Object(Map::new()));
        out.insert("total_tokens".to_owned(), Value::from(0));
        return Value::Object(out);
    }

    let mut by_type: Map<String, Value> = Map::new();
    let mut by_model: Map<String, Value> = Map::new();
    // `total_tokens = 0` then `+=` — an int accumulator. Every contribution is
    // an int too, so this stays an int all the way to the wire.
    let mut total_tokens: i64 = 0;

    for msg in messages {
        let obj = msg.as_object();
        // `msg.get("type", "unknown")` — the default fires on a MISSING key
        // only. The formatter always writes a string here, so the "present but
        // not a string" branch is dead; it maps to `"unknown"` rather than
        // panicking.
        let msg_type = obj
            .and_then(|o| o.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bump(&mut by_type, msg_type);

        let model = obj
            .and_then(|o| o.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bump(&mut by_model, model);

        // `tokens = msg.get("tokens", {})` / `if isinstance(tokens, dict)`.
        if let Some(Value::Object(tokens)) = obj.and_then(|o| o.get("tokens")) {
            total_tokens += token_field(tokens, "input") + token_field(tokens, "output");
        }
    }

    let mut out = Map::new();
    out.insert(
        "total".to_owned(),
        Value::from(i64::try_from(messages.len()).unwrap_or(i64::MAX)),
    );
    out.insert("by_type".to_owned(), Value::Object(by_type));
    out.insert("by_model".to_owned(), Value::Object(by_model));
    out.insert("total_tokens".to_owned(), Value::from(total_tokens));
    Value::Object(out)
}

/// `d[k] = d.get(k, 0) + 1` — insertion-ordered, first-seen wins the position.
fn bump(counts: &mut Map<String, Value>, key: &str) {
    let next = counts.get(key).and_then(Value::as_i64).unwrap_or(0) + 1;
    counts.insert(key.to_owned(), Value::from(next));
}

/// `tokens.get(name, 0)` — a non-integer value would be a `TypeError` in
/// Python's `+`; the enricher only ever writes integers here, so a stray float
/// reads as `0` rather than propagating a lossy cast.
fn token_field(tokens: &Map<String, Value>, name: &str) -> i64 {
    tokens.get(name).and_then(Value::as_i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_envelope_key_order_is_the_python_literals() {
        let page = build_messages_page(vec![json!({"a": 1})], 10, 1, 3);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&page),
            r#"{"messages":[{"a":1}],"total":10,"page":1,"per_page":3,"total_pages":4,"start_index":0,"end_index":3}"#
        );
    }

    #[test]
    fn the_last_page_is_short_and_end_index_is_clamped_to_total() {
        // 10 rows, 3 per page → pages 1..4, the last holding one row.
        let page = build_messages_page(vec![json!(1)], 10, 4, 3);
        let obj = page.as_object().expect("object");
        assert_eq!(obj["page"], json!(4));
        assert_eq!(obj["total_pages"], json!(4));
        assert_eq!(obj["start_index"], json!(9));
        // raw end is 12; `min(end_index, total)` brings it to 10.
        assert_eq!(obj["end_index"], json!(10));
    }

    #[test]
    fn an_over_range_page_is_clamped_down_to_the_last_one() {
        let (page, pages, start, end) = page_bounds(10, 999, 3);
        assert_eq!((page, pages, start, end), (4, 4, 9, 12));
    }

    #[test]
    fn a_page_below_one_is_clamped_up() {
        let (page, _, start, _) = page_bounds(10, -7, 3);
        assert_eq!((page, start), (1, 0));
    }

    #[test]
    fn an_empty_project_reports_page_zero_and_a_negative_start_index() {
        // DIV-121, bug-for-bug: `total_pages` is 0, `page > 0` clamps the
        // 1-indexed page to 0, and `start_index` goes negative. Both reach the
        // wire. A port that "fixed" this would diverge on every empty project.
        let (page, pages, start, end) = page_bounds(0, 1, 100);
        assert_eq!((page, pages, start, end), (0, 0, -100, 0));
        // On the wire: `start_index` is the raw negative, while `end_index`
        // survives as `min(0, 0)` — so the envelope reports a range whose start
        // is greater than its end. Both numbers are Python's.
        let envelope = build_messages_page(Vec::new(), 0, 1, 100);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&envelope),
            r#"{"messages":[],"total":0,"page":0,"per_page":100,"total_pages":0,"start_index":-100,"end_index":0}"#
        );
    }

    #[test]
    fn the_in_memory_slicer_agrees_with_the_sql_one_on_the_same_page() {
        let rows: Vec<Value> = (0..10).map(Value::from).collect();
        let sliced = get_paginated_messages(rows.clone(), 2, 3);
        let sql_side = build_messages_page(rows[3..6].to_vec(), 10, 2, 3);
        assert_eq!(sliced, sql_side);
    }

    #[test]
    fn a_negative_start_index_wraps_the_way_pythons_slice_does() {
        // The empty-project clamp is the only way to reach this, and there the
        // list is empty so the slice is empty either way. Pinned so a future
        // caller that reaches it with a non-empty list gets Python's answer and
        // not a panic.
        let rows: Vec<Value> = (0..4).map(Value::from).collect();
        assert_eq!(python_slice(rows.clone(), -2, 4), vec![json!(2), json!(3)]);
        assert_eq!(python_slice(rows.clone(), 3, 1), Vec::<Value>::new());
        assert_eq!(python_slice(rows, 0, 999), vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_include_all_envelope_is_four_keys_and_no_indices() {
        let all = get_all_messages(vec![json!(1), json!(2)]);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&all),
            r#"{"messages":[1,2],"total":2,"page":1,"per_page":2,"total_pages":1}"#
        );
    }

    #[test]
    fn the_empty_summary_omits_total_sessions_and_counts_in_ints() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&get_messages_summary(&[])),
            r#"{"total":0,"by_type":{},"by_model":{},"total_tokens":0}"#
        );
    }

    #[test]
    fn the_summary_counts_by_first_seen_order_and_sums_only_input_plus_output() {
        let messages = vec![
            json!({"type": "assistant", "model": "claude-x",
                   "tokens": {"input": 10, "output": 5, "cache_read": 900}}),
            json!({"type": "user", "model": "claude-x", "tokens": {}}),
            json!({"type": "assistant", "model": "claude-y",
                   "tokens": {"input": 1, "output": 1}}),
        ];
        // cache_read is NOT in the total — `tokens.get("input") + tokens.get("output")`.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&get_messages_summary(&messages)),
            r#"{"total":3,"by_type":{"assistant":2,"user":1},"by_model":{"claude-x":2,"claude-y":1},"total_tokens":17}"#
        );
    }

    #[test]
    fn a_record_with_no_type_or_model_lands_under_unknown_not_na() {
        // The mart-backed `/api/messages/summary` keys a model-less row "N/A";
        // this legacy path keys it "unknown". The inconsistency is real, it is
        // on the wire, and neither side is adjusted here.
        let messages = vec![json!({"tokens": {"input": 2, "output": 3}})];
        assert_eq!(
            stax_memory::pyjson::dumps_http(&get_messages_summary(&messages)),
            r#"{"total":1,"by_type":{"unknown":1},"by_model":{"unknown":1},"total_tokens":5}"#
        );
    }

    #[test]
    fn a_non_dict_tokens_value_contributes_nothing_rather_than_raising() {
        let messages = vec![json!({"type": "x", "model": "m", "tokens": 7})];
        let summary = get_messages_summary(&messages);
        assert_eq!(summary["total_tokens"], json!(0));
    }
}

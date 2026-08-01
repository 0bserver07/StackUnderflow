//! `reports/anomaly.py` — cost-outlier detection for the optimize surface.
//!
//! | Item | Python | Reached from |
//! |---|---|---|
//! | [`find_cost_anomalies`] | same | `GET /api/optimize` → `"anomalies"` |
//! | [`CostAnomaly`] | `@dataclass(frozen=True)` | the `anomalies` array |
//! | [`MAD_K`], [`MIN_POINTS`], [`TOP_N`] | same | the module's tunables |
//!
//! # What the detector actually does
//!
//! Two series — per-day cost from `daily_mart`, per-session cost from
//! `session_mart` — get the same upper-tail test:
//!
//! ```text
//! cost - median > k · (MAD · 1.4826)
//! ```
//!
//! MAD (median absolute deviation) rather than a standard deviation, because a
//! single huge spike inflates a stddev enough to mask *itself* while barely
//! moving the median. When the MAD is exactly zero — more than half the points
//! sit exactly on the median, which on a cost series means a run of identical
//! (often `0.0`) days — the test degrades to `mean + k·pstdev`, and a truly flat
//! series flags nothing at all.
//!
//! # The three things a careless port gets wrong
//!
//! 1. **`statistics.fmean` is `math.fsum(data)/n`, not a running sum.**
//!    `fsum` is Shewchuk's exact partial-sum algorithm: the result is the
//!    correctly-rounded `f64` of the *exact* sum, which is neither a naive `+=`
//!    chain nor the Neumaier compensation `builtins.sum` uses. It is
//!    transcribed here from `Modules/mathmodule.c`, half-even fixup included.
//!    (DIV-114.)
//! 2. **`statistics.pstdev` is exact-rational in CPython and is NOT here.**
//!    `_ss` accumulates `Σx` and `Σx²` as `Fraction`s and evaluates
//!    `(n·sxx − sx²)/n` with no rounding at all. This port uses the
//!    numerically-stable two-pass form instead; transliterating the *formula*
//!    into `f64` would catastrophically cancel and be far more wrong than the
//!    ULP this costs. Recorded as DIV-113, with the blast radius measured.
//! 3. **`sort(key=…, reverse=True)` does not reverse ties.** Python's sort is
//!    stable and `reverse=True` is applied to the comparison, not to the output,
//!    so equal deviations keep their original order. `slice::sort_by` is stable
//!    too, so a `b.cmp(a)` comparator reproduces it — `sort_by(...).reverse()`
//!    would not.
//!
//! Every read is bounded by the caller's [`Scope`](super::scope::Scope) and the
//! detector never raises: an absent mart, a series under [`MIN_POINTS`], or a
//! flat series all return an empty-but-well-formed payload.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value};

use super::mart_queries;
use super::optimize::round_half_even;
use super::scope::Scope;

// ── tunables ─────────────────────────────────────────────────────────────────

/// `MAD_K = 3.0` — the outlier cut, in normal-consistent σ units.
pub const MAD_K: f64 = 3.0;
/// `MIN_POINTS = 5` — shorter series have no defensible baseline.
pub const MIN_POINTS: usize = 5;
/// `_MAD_TO_SIGMA = 1.4826` — makes MAD a consistent estimator of σ.
const MAD_TO_SIGMA: f64 = 1.4826;
/// `TOP_N = 10` — the panel shows the worst few, not a wall.
pub const TOP_N: usize = 10;
/// `_MIN_ABSOLUTE_USD = 0.05` — below this a "3× the median" spike is pennies.
const MIN_ABSOLUTE_USD: f64 = 0.05;

// ── result type ──────────────────────────────────────────────────────────────

/// One flagged outlier bucket — a day or a session.
///
/// Field order is the dataclass declaration order, because `asdict()` follows
/// it and the rendered key order is the contract.
#[derive(Debug, Clone)]
pub struct CostAnomaly {
    /// `"day"` or `"session"`.
    pub kind: &'static str,
    /// The day (`YYYY-MM-DD`) or the session id.
    pub key: String,
    /// `round(cost, 4)`.
    pub cost_usd: f64,
    /// `round(baseline, 4)` — the median on the MAD path, the mean on the fallback.
    pub baseline_usd: f64,
    /// `round(cost - baseline, 4)`; always positive for a flagged row.
    pub deviation_usd: f64,
    /// `round(cost / baseline, 2)`, or `None` when the baseline is not positive.
    pub ratio: Option<f64>,
    /// `round(deviation / σ, 2)`.
    pub score: f64,
    /// `"mad"` or `"stddev"`.
    pub method: &'static str,
    /// The rendered one-liner.
    pub reason: String,
    /// The per-key extras (`{}` for days).
    pub details: Value,
}

impl CostAnomaly {
    /// `asdict(self)` — the nine keys, in declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("kind".to_owned(), Value::from(self.kind));
        obj.insert("key".to_owned(), Value::from(self.key.clone()));
        obj.insert("cost_usd".to_owned(), json_float(self.cost_usd));
        obj.insert("baseline_usd".to_owned(), json_float(self.baseline_usd));
        obj.insert("deviation_usd".to_owned(), json_float(self.deviation_usd));
        obj.insert(
            "ratio".to_owned(),
            self.ratio.map_or(Value::Null, json_float),
        );
        obj.insert("score".to_owned(), json_float(self.score));
        obj.insert("method".to_owned(), Value::from(self.method));
        obj.insert("reason".to_owned(), Value::from(self.reason.clone()));
        obj.insert("details".to_owned(), self.details.clone());
        Value::Object(obj)
    }
}

/// A Python `float` as JSON — `0.0` stays a float, non-finite becomes `null`.
fn json_float(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

// ── statistics core ──────────────────────────────────────────────────────────

/// `math.fsum` — Shewchuk's exact partial-sum algorithm.
///
/// Transcribed from `Modules/mathmodule.c::math_fsum_impl`, including the
/// half-even fixup at the end that makes `fsum([1e-16, 1, 1e16])` round the way
/// a single exact addition would. `statistics.fmean` is `fsum(data)/n`, so this
/// is on the response path whenever the stddev fallback fires.
///
/// Non-finite inputs cannot reach it here (every value is a cost read out of a
/// `REAL` column and multiplied by nothing), so the special-case ladder CPython
/// carries for infinities is left out; the loop below would produce a NaN
/// rather than CPython's `ValueError`, which is a difference no reachable input
/// can observe.
#[must_use]
pub fn fsum(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for &value in values {
        let mut x = value;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        if x != 0.0 {
            partials.push(x);
        }
    }
    let mut n = partials.len();
    let mut hi = 0.0;
    let mut lo = 0.0;
    if n > 0 {
        n -= 1;
        hi = partials[n];
        // Sum exactly from the top, stopping the moment the sum goes inexact.
        while n > 0 {
            let x = hi;
            n -= 1;
            let y = partials[n];
            hi = x + y;
            let yr = hi - x;
            lo = y - yr;
            if lo != 0.0 {
                break;
            }
        }
        // Half-even across multiple partials.
        if n > 0 && ((lo < 0.0 && partials[n - 1] < 0.0) || (lo > 0.0 && partials[n - 1] > 0.0)) {
            let y = lo * 2.0;
            let x = hi + y;
            let yr = x - hi;
            if (y - yr).abs() == 0.0 && y == yr {
                hi = x;
            }
        }
    }
    hi
}

/// `statistics.median` — sort, then middle (odd) or the mean of the two middles.
///
/// NaN cannot arise from a cost column, so the sort uses `total_cmp`, which is a
/// total order and therefore cannot panic the way `partial_cmp().unwrap()`
/// would if one ever did.
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// `statistics.fmean(data)` — `fsum(data) / len(data)`.
#[must_use]
pub fn fmean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "the point count is a row count, far below 2^53"
    )]
    let n = values.len() as f64;
    fsum(values) / n
}

/// `statistics.pstdev(data)` — the POPULATION standard deviation.
///
/// **DIV-113.** CPython computes the sum of squared deviations in exact
/// rational arithmetic; this is the two-pass `f64` form. See the module docs
/// and the ledger for why the algebraic one-pass form CPython's *formula* uses
/// was not transliterated.
#[must_use]
pub fn pstdev(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = fmean(values);
    let squared: Vec<f64> = values.iter().map(|v| (v - mean) * (v - mean)).collect();
    #[allow(
        clippy::cast_precision_loss,
        reason = "the point count is a row count, far below 2^53"
    )]
    let n = values.len() as f64;
    (fsum(&squared) / n).sqrt()
}

/// `_flag_outliers(points, kind=…, k=…, min_points=…, extra=…)`.
///
/// `extra` maps a key → the `details` object merged into that anomaly; days
/// pass `None` and get `{}`.
fn flag_outliers(
    points: &[(String, f64)],
    kind: &'static str,
    k: f64,
    min_points: usize,
    extra: Option<&HashMap<String, Value>>,
) -> Vec<CostAnomaly> {
    // `usable = [(key, float(cost)) for key, cost in points if cost is not None]`
    // — no cost source here can produce `None`, so `usable == points`.
    if points.len() < min_points {
        return Vec::new();
    }
    let costs: Vec<f64> = points.iter().map(|(_, c)| *c).collect();
    let med = median(&costs);
    let abs_dev: Vec<f64> = costs.iter().map(|c| (c - med).abs()).collect();
    let mad = median(&abs_dev);

    let method: &'static str;
    let baseline: f64;
    let spread_sigma: f64;

    if mad > 0.0 {
        method = "mad";
        baseline = med;
        spread_sigma = mad * MAD_TO_SIGMA;
    } else {
        // Flat-by-median series — `mean + k·pstdev`. A zero spread flags nothing.
        method = "stddev";
        baseline = fmean(&costs);
        let stdev = pstdev(&costs);
        if stdev <= 0.0 {
            return Vec::new();
        }
        spread_sigma = stdev;
    }

    let threshold = baseline + k * spread_sigma;

    let mut out: Vec<CostAnomaly> = Vec::new();
    for (key, cost) in points {
        let cost = *cost;
        if cost < MIN_ABSOLUTE_USD {
            continue;
        }
        if cost <= threshold {
            continue;
        }
        let deviation = cost - baseline;
        let score = if spread_sigma > 0.0 {
            deviation / spread_sigma
        } else {
            0.0
        };
        // `ratio = (cost / baseline) if baseline > 0 else None`.
        let ratio = (baseline > 0.0).then(|| cost / baseline);
        out.push(CostAnomaly {
            kind,
            key: key.clone(),
            cost_usd: round_half_even(cost, 4),
            baseline_usd: round_half_even(baseline, 4),
            deviation_usd: round_half_even(deviation, 4),
            // `round(ratio, 2) if ratio is not None else None`.
            ratio: ratio.map(|r| round_half_even(r, 2)),
            score: round_half_even(score, 2),
            method,
            // `_reason` is handed the UNROUNDED ratio and score.
            reason: reason(kind, cost, baseline, ratio, score, method),
            details: extra
                .and_then(|map| map.get(key.as_str()))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        });
    }

    // Worst first (largest dollar deviation). Stable, so ties keep input order.
    out.sort_by(|a, b| b.deviation_usd.total_cmp(&a.deviation_usd));
    out
}

/// `_reason(...)` — the human one-liner, f-strings and all.
fn reason(
    kind: &str,
    cost: f64,
    baseline: f64,
    ratio: Option<f64>,
    score: f64,
    method: &str,
) -> String {
    let noun = if kind == "day" { "day" } else { "session" };
    let base_label = if method == "mad" { "median" } else { "mean" };
    let cost_s = grouped_2dp(cost);
    let base_s = grouped_2dp(baseline);
    let score_s = format!("{score:.1}");
    match ratio {
        // `if ratio is not None and ratio >= 1.5:`
        Some(ratio) if ratio >= 1.5 => {
            let mult = format!("{ratio:.1}×");
            format!(
                "This {noun} cost ${cost_s} — {mult} the {base_label} \
                 of ${base_s} ({score_s}σ over baseline)."
            )
        }
        _ => format!(
            "This {noun} cost ${cost_s} vs a {base_label} of \
             ${base_s} ({score_s}σ over baseline)."
        ),
    }
}

/// `format(value, ",.2f")` — two decimals, then thousands separators.
///
/// Rust's `{:.2}` rounds the decimal expansion half-to-even, the same rule
/// CPython's `float.__format__` goes through `_Py_dg_dtoa` for, so only the
/// grouping has to be added by hand.
fn grouped_2dp(value: f64) -> String {
    let fixed = format!("{value:.2}");
    let (sign, body) = match fixed.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", fixed.as_str()),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if frac_part.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac_part}")
    }
}

// ── data sourcing ────────────────────────────────────────────────────────────

/// `_daily_cost_points` — per-day totals from `daily_mart`, summed across
/// projects and models.
///
/// The fold is `by_day[day] = by_day.get(day, 0.0) + cost` — a PLAIN `+=`, not
/// a `sum()`, so the accumulation is uncompensated and stays that way here
/// (LAW 3: match the operation, not the accuracy). The result is
/// `sorted(by_day.items())`, i.e. day-string order.
fn daily_cost_points(
    conn: &Connection,
    scope: Option<&Scope>,
) -> rusqlite::Result<Vec<(String, f64)>> {
    let day_from = scope
        .and_then(|s| s.since.as_deref())
        .and_then(|iso| iso.get(..10).map(str::to_owned));
    let day_to = scope
        .and_then(|s| s.until.as_deref())
        .and_then(|iso| iso.get(..10).map(str::to_owned));
    let rows = mart_queries::daily_global(conn, day_from.as_deref(), day_to.as_deref())?;

    let mut order: Vec<String> = Vec::new();
    let mut by_day: HashMap<String, f64> = HashMap::new();
    for row in rows {
        // `day = r.get("day"); if not day: continue` — NULL and "" both skip.
        let Some(day) = row.day.filter(|d| !d.is_empty()) else {
            continue;
        };
        let cost = row.cost_usd.unwrap_or(0.0);
        match by_day.get_mut(&day) {
            Some(total) => *total += cost,
            None => {
                order.push(day.clone());
                by_day.insert(day, cost);
            }
        }
    }
    order.sort();
    Ok(order
        .into_iter()
        .map(|day| {
            let cost = by_day.get(&day).copied().unwrap_or(0.0);
            (day, cost)
        })
        .collect())
}

/// `(points, extra)` — the pair `_session_cost_points` returns.
type SessionSeries = (Vec<(String, f64)>, HashMap<String, Value>);

/// `_session_cost_points` — per-session cost plus the per-session details map.
///
/// The points list keeps duplicates (a repeated `session_id` appends twice)
/// while the details map keeps the LAST one. Both are Python's, and both are
/// unreachable on a `session_mart` with a unique `session_id`.
fn session_cost_points(
    conn: &Connection,
    scope: Option<&Scope>,
) -> rusqlite::Result<SessionSeries> {
    let since = scope.and_then(|s| s.since.as_deref());
    let until = scope.and_then(|s| s.until.as_deref());
    let rows = mart_queries::session_mart_rows_for_compare(conn, since, until)?;

    let mut points = Vec::new();
    let mut extra: HashMap<String, Value> = HashMap::new();
    for row in rows {
        // `sid = r.get("session_id"); if not sid: continue`.
        let Some(sid) = row.session_id.filter(|s| !s.is_empty()) else {
            continue;
        };
        points.push((sid.clone(), row.cost_usd.unwrap_or(0.0)));
        let mut detail = Map::new();
        detail.insert(
            "model".to_owned(),
            row.primary_model.map_or(Value::Null, Value::from),
        );
        detail.insert(
            "provider".to_owned(),
            row.provider.map_or(Value::Null, Value::from),
        );
        detail.insert(
            "first_ts".to_owned(),
            row.first_ts.map_or(Value::Null, Value::from),
        );
        detail.insert(
            "message_count".to_owned(),
            Value::from(row.message_count.unwrap_or(0)),
        );
        extra.insert(sid, Value::Object(detail));
    }
    Ok((points, extra))
}

// ── public entry point ───────────────────────────────────────────────────────

/// `find_cost_anomalies(conn, scope=…)` — the `"anomalies"` block of
/// `GET /api/optimize`.
///
/// The five keys, in order: `method`, `k`, `anomalies`, `day_count`,
/// `session_count`. `k` is a Python `float`, so it renders `3.0` and not `3`.
///
/// The top-level `method` is the DAY series' method (the primary signal),
/// falling back to the session series' and then to `"none"` — per-row `method`
/// stays authoritative for each row.
///
/// # Errors
/// Any SQLite error the two mart reads surface. A *missing* mart is not an
/// error; it is an empty series.
pub fn find_cost_anomalies(conn: &Connection, scope: Option<&Scope>) -> rusqlite::Result<Value> {
    let day_points = daily_cost_points(conn, scope)?;
    let day_anoms = flag_outliers(&day_points, "day", MAD_K, MIN_POINTS, None);

    // `include_sessions` defaults to True and no caller overrides it.
    let (sess_points, sess_extra) = session_cost_points(conn, scope)?;
    let session_count = sess_points.len();
    let session_anoms = flag_outliers(
        &sess_points,
        "session",
        MAD_K,
        MIN_POINTS,
        Some(&sess_extra),
    );

    let mut combined: Vec<CostAnomaly> = day_anoms
        .iter()
        .chain(session_anoms.iter())
        .cloned()
        .collect();
    combined.sort_by(|a, b| b.deviation_usd.total_cmp(&a.deviation_usd));
    combined.truncate(TOP_N);

    let method = if let Some(first) = day_anoms.first() {
        first.method
    } else if let Some(first) = session_anoms.first() {
        first.method
    } else {
        "none"
    };

    let mut obj = Map::new();
    obj.insert("method".to_owned(), Value::from(method));
    obj.insert("k".to_owned(), json_float(MAD_K));
    obj.insert(
        "anomalies".to_owned(),
        Value::Array(combined.iter().map(CostAnomaly::to_dict).collect()),
    );
    obj.insert("day_count".to_owned(), Value::from(day_points.len()));
    obj.insert("session_count".to_owned(), Value::from(session_count));
    Ok(Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsum_is_exact_where_a_running_total_is_not() {
        // `math.fsum([0.1] * 10)` is exactly 1.0 — the exact sum of ten copies
        // of the binary value nearest 0.1 rounds to 1.0 — while a left-to-right
        // `+=` chain (and Python's own `sum()`) gives 0.9999999999999999. This
        // is THE difference `statistics.fmean` inherits, and getting it wrong
        // would move `baseline_usd` on the stddev fallback.
        let tenths = vec![0.1_f64; 10];
        assert_eq!(fsum(&tenths), 1.0);
        let naive: f64 = tenths.iter().sum();
        assert_ne!(naive, 1.0, "a running total does NOT get there");
        // `math.fsum([1e-16, 1.0, 1e16])` == 1.0000000000000002e16.
        assert_eq!(fsum(&[1e-16, 1.0, 1e16]), 1.000_000_000_000_000_2e16);
        assert_eq!(fsum(&[]), 0.0);
    }

    #[test]
    fn median_takes_the_mean_of_the_two_middles_on_an_even_count() {
        assert!((median(&[1.0, 2.0, 3.0]) - 2.0).abs() < f64::EPSILON);
        assert!((median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < f64::EPSILON);
        // Unsorted input must be sorted first.
        assert!((median(&[4.0, 1.0, 3.0, 2.0]) - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_series_under_min_points_flags_nothing_however_extreme() {
        let points: Vec<(String, f64)> = vec![
            ("a".into(), 1.0),
            ("b".into(), 1.0),
            ("c".into(), 1.0),
            ("d".into(), 9999.0),
        ];
        assert!(flag_outliers(&points, "day", MAD_K, MIN_POINTS, None).is_empty());
    }

    #[test]
    fn a_perfectly_flat_series_flags_nothing_because_both_spreads_are_zero() {
        let points: Vec<(String, f64)> = (0..6).map(|i| (format!("d{i}"), 1.0)).collect::<Vec<_>>();
        assert!(flag_outliers(&points, "day", MAD_K, MIN_POINTS, None).is_empty());
    }

    #[test]
    fn the_stddev_fallback_fires_only_when_the_mad_is_exactly_zero() {
        // Sixteen identical points plus one spike: the median is 1.0 and so is
        // every-but-one absolute deviation, so the MAD is exactly 0.0 and the
        // `mean + k·pstdev` leg runs with a MEAN baseline.
        //
        // Sixteen and not five, and that is arithmetic rather than taste: with
        // ONE outlier among n identical points the flag needs n > 1 + 3·√(n−1),
        // i.e. n ≥ 11. A five-point version of this test looks right and can
        // never flag anything.
        let mut points: Vec<(String, f64)> = (0..16).map(|i| (format!("d{i}"), 1.0)).collect();
        points.push(("spike".into(), 100.0));
        let flagged = flag_outliers(&points, "day", MAD_K, MIN_POINTS, None);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].method, "stddev");
        assert_eq!(flagged[0].key, "spike");
        // statistics.fmean → 6.823529411764706, round(_, 4) → 6.8235
        assert!((flagged[0].baseline_usd - 6.8235).abs() < 1e-12);
    }

    #[test]
    fn a_spike_under_five_cents_is_never_flagged() {
        // The ratio test would pass spectacularly (0.04 against a 0.0001
        // median) and `_MIN_ABSOLUTE_USD` vetoes it anyway.
        let mut points: Vec<(String, f64)> = (0..8)
            .map(|i| (format!("d{i}"), 0.0001 + f64::from(i) * 1e-6))
            .collect();
        points.push(("tiny".into(), 0.04));
        let flagged = flag_outliers(&points, "day", MAD_K, MIN_POINTS, None);
        assert!(flagged.is_empty(), "0.04 < _MIN_ABSOLUTE_USD");
    }

    #[test]
    fn equal_deviations_keep_their_input_order_reverse_does_not_reverse_ties() {
        // Two identical spikes: Python's stable sort with reverse=True leaves
        // `zzz_first` before `aaa_second`. A `sort_by(…).reverse()` would swap
        // them, and so would sorting on the key name.
        let points: Vec<(String, f64)> = vec![
            ("zzz_first".into(), 50.0),
            ("aaa_second".into(), 50.0),
            ("d2".into(), 1.0),
            ("d3".into(), 2.0),
            ("d4".into(), 3.0),
            ("d5".into(), 4.0),
        ];
        let flagged = flag_outliers(&points, "day", MAD_K, MIN_POINTS, None);
        assert_eq!(flagged.len(), 2);
        assert_eq!(flagged[0].key, "zzz_first");
        assert_eq!(flagged[1].key, "aaa_second");
        assert_eq!(flagged[0].method, "mad");
    }

    #[test]
    fn the_rendered_anomaly_is_nine_keys_in_declaration_order() {
        // costs [1,2,3,4,5,40] → median 3.5, MAD 1.5, σ-equivalent 2.2239,
        // threshold 10.1717. Only the 40 clears it.
        let mut points: Vec<(String, f64)> =
            (1..=5).map(|i| (format!("d{i}"), f64::from(i))).collect();
        points.push(("2026-07-30".into(), 40.0));
        let flagged = flag_outliers(&points, "day", MAD_K, MIN_POINTS, None);
        assert_eq!(flagged.len(), 1);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&flagged[0].to_dict()),
            concat!(
                r#"{"kind":"day","key":"2026-07-30","cost_usd":40.0,"#,
                r#""baseline_usd":3.5,"deviation_usd":36.5,"ratio":11.43,"#,
                r#""score":16.41,"method":"mad","#,
                r#""reason":"This day cost $40.00 — 11.4× the median of $3.50 "#,
                r#"(16.4σ over baseline).","details":{}}"#,
            )
        );
    }

    #[test]
    fn the_reason_groups_thousands_and_uses_the_unrounded_ratio() {
        // ratio 2.5 ≥ 1.5, so the multiplier form is used.
        let text = reason("day", 2500.0, 1000.0, Some(2.5), 4.25, "mad");
        assert_eq!(
            text,
            "This day cost $2,500.00 — 2.5× the median of $1,000.00 (4.2σ over baseline)."
        );
        // 4.25 formats as "4.2" — Rust and CPython both round the decimal
        // expansion half-to-even here, and 4.25 is exactly representable.
        // Below 1.5 the sentence loses the multiplier clause entirely.
        let text = reason("session", 1.5, 1.2, Some(1.25), 3.0, "stddev");
        assert_eq!(
            text,
            "This session cost $1.50 vs a mean of $1.20 (3.0σ over baseline)."
        );
    }

    #[test]
    fn a_store_with_no_marts_answers_the_five_key_empty_payload() {
        let conn = Connection::open_in_memory().expect("in-memory");
        let payload = find_cost_anomalies(&conn, None).expect("guarded");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            r#"{"method":"none","k":3.0,"anomalies":[],"day_count":0,"session_count":0}"#
        );
    }

    #[test]
    fn days_are_summed_across_mart_rows_and_sorted_by_day_string() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE daily_mart (day TEXT, cost_usd REAL);
             INSERT INTO daily_mart VALUES
                 ('2026-07-03', 1.0), ('2026-07-01', 0.5), ('2026-07-01', 0.25),
                 ('2026-07-02', 1.0), ('2026-07-04', 1.0), ('2026-07-05', 90.0);",
        )
        .expect("schema");
        let points = daily_cost_points(&conn, None).expect("query");
        assert_eq!(points.len(), 5);
        assert_eq!(points[0].0, "2026-07-01");
        assert!((points[0].1 - 0.75).abs() < 1e-12, "two rows folded");
        assert_eq!(points[4].0, "2026-07-05");
    }

    #[test]
    fn the_top_level_method_is_the_day_series_not_the_sessions() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE daily_mart (day TEXT, cost_usd REAL);
             CREATE TABLE session_mart (
                 session_id TEXT, project_id INTEGER, provider TEXT,
                 primary_model TEXT, first_ts TEXT, message_count INTEGER,
                 input_tokens INTEGER, cache_create INTEGER, cost_usd REAL);
             INSERT INTO daily_mart VALUES
                 ('2026-07-01', 1.0), ('2026-07-02', 2.0), ('2026-07-03', 3.0),
                 ('2026-07-04', 4.0), ('2026-07-05', 5.0), ('2026-07-06', 40.0);",
        )
        .expect("schema");
        let payload = find_cost_anomalies(&conn, None).expect("query");
        assert_eq!(payload["method"], Value::from("mad"));
        assert_eq!(payload["day_count"], Value::from(6));
        assert_eq!(payload["session_count"], Value::from(0));
        assert_eq!(payload["anomalies"].as_array().expect("array").len(), 1);
    }
}

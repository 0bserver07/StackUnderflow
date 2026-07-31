//! `services/burn.py` — burn projector v2: the month-end forecast and its alert
//! ladder.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `DEFAULT_WEIGHTED_WINDOW` / `_DECAY` | `7` / `0.85` | [`DEFAULT_WEIGHTED_WINDOW`] / [`DEFAULT_WEIGHTED_DECAY`] |
//! | `DEFAULT_THRESHOLDS` | `(50, 75, 90)` | [`DEFAULT_THRESHOLDS`] |
//! | `linear_projection` | mean of the window | [`linear_projection`] |
//! | `weighted_projection` | exponentially-weighted mean | [`weighted_projection`] |
//! | `pick_projection_method` | ≥3 non-zero samples → weighted | [`pick_projection_method`] |
//! | `days_to_limit` | budget ÷ burn, floored | [`days_to_limit`] |
//! | `crossed_thresholds` | highest threshold met | [`crossed_thresholds`] |
//! | `build_projection` | the block the route emits | [`build_projection`] |
//! | `_alert_message` | one human-readable banner | [`alert_message`] |
//!
//! The module is pure — no SQL, no settings. The caller slices the daily-cost
//! array out of the store and hands it in; see `routes/plan.rs`.
//!
//! # What a careless port gets wrong
//!
//! * **`sum(cleaned) / len(cleaned)` is a `sum()`, not a `+=` chain.** CPython
//!   3.12 runs Neumaier-compensated summation on `sum()`'s float fast path
//!   (`gh-100425`), so a plain accumulator drifts an ULP or two past a few
//!   thousand rows. `linear_projection` therefore uses
//!   [`stax_etl::stats::aggregator::Neumaier`]. Four lines away,
//!   `weighted_projection` writes `total += value * weight` — an explicit
//!   statement, which is a *plain* accumulation. Both are reproduced as
//!   written; matching the operation is the rule, and "more accurate" would be
//!   the divergence (law 3).
//! * **`max(0.0, float(c))` is not `f64::max`'s twin by accident.** CPython's
//!   two-argument `max` keeps the first value unless the second is strictly
//!   greater, so `max(0.0, NaN)` is `0.0` — NaN loses. Written here as an
//!   explicit `> 0.0` test so the NaN case is visible rather than inferred.
//! * **`pick_projection_method` reads the RAW series, not the cleaned one.**
//!   `[c for c in daily_costs if c > 0]` runs before any clamping, so a negative
//!   day is excluded from the sample count and a `0.0` day is too.
//! * **The stale-store fallback rewrites `projection_method`.** When
//!   weighted-7d computes exactly `0.0` but the whole-period linear mean is
//!   positive, the burn switches to linear *and so does the reported method*.
//!   That is the user-visible tell that the last seven days were empty.
//! * **`thresholds or DEFAULT_THRESHOLDS` is truthiness.** An EMPTY list falls
//!   back to `(50, 75, 90)`; `[0]` does not. [`build_projection`] takes
//!   `Option<&[i64]>` and treats `Some(&[])` exactly like `None`.

use std::fmt::Write as _;

use stax_etl::stats::aggregator::Neumaier;

/// `DEFAULT_WEIGHTED_WINDOW` — days in the weighted look-back.
pub const DEFAULT_WEIGHTED_WINDOW: i64 = 7;

/// `DEFAULT_WEIGHTED_DECAY` — yesterday weighs 85% of today.
pub const DEFAULT_WEIGHTED_DECAY: f64 = 0.85;

/// `DEFAULT_THRESHOLDS` — the built-in 50/75/90 alert ladder.
pub const DEFAULT_THRESHOLDS: [i64; 3] = [50, 75, 90];

/// `_MIN_SAMPLES_FOR_WEIGHTED` — below this the weighted mean is too jumpy.
const MIN_SAMPLES_FOR_WEIGHTED: usize = 3;

/// `ProjectionMethod = Literal["linear", "weighted-7d"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMethod {
    /// `"linear"` — the v1 whole-window mean.
    Linear,
    /// `"weighted-7d"` — the exponentially-weighted recent mean.
    Weighted7d,
}

impl ProjectionMethod {
    /// The literal string the JSON carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Weighted7d => "weighted-7d",
        }
    }
}

/// `max(0.0, float(c))` — see the module docs on the NaN case.
fn clamp_non_negative(value: f64) -> f64 {
    if value > 0.0 { value } else { 0.0 }
}

/// `linear_projection(daily_costs)` — the mean per-day cost across the window.
///
/// Empty input is `0.0`. Negative days are clamped to zero (a refund should not
/// subtract from the forecast) but still count toward the denominator.
#[must_use]
pub fn linear_projection(daily_costs: &[f64]) -> f64 {
    if daily_costs.is_empty() {
        return 0.0;
    }
    // `sum(cleaned)` — the compensated one (law 3).
    let mut acc = Neumaier::default();
    for value in daily_costs {
        acc.add(clamp_non_negative(*value));
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "len() of a per-day series; a billing window is under 32 entries"
    )]
    let count = daily_costs.len() as f64;
    acc.finish() / count
}

/// `weighted_projection(daily_costs, window=…, decay=…)`.
///
/// The input is oldest-first, so `daily_costs.last()` is *today* and carries
/// weight `1.0`; each step further back multiplies by `decay`. The result is
/// divided by the weight sum, so it stays a per-day dollar figure rather than a
/// tally.
///
/// A `window <= 0` and a `decay` outside `(0, 1]` both silently revert to the
/// defaults — Python validates by overwriting the argument, not by raising.
#[must_use]
pub fn weighted_projection(daily_costs: &[f64], window: i64, decay: f64) -> f64 {
    let cleaned: Vec<f64> = daily_costs.iter().map(|c| clamp_non_negative(*c)).collect();
    if cleaned.is_empty() {
        return 0.0;
    }

    let window = if window <= 0 {
        DEFAULT_WEIGHTED_WINDOW
    } else {
        window
    };
    // `cleaned[-window:]` — a slice longer than the list is the whole list.
    let keep = usize::try_from(window)
        .unwrap_or(usize::MAX)
        .min(cleaned.len());
    let tail = &cleaned[cleaned.len() - keep..];
    // `if not (0.0 < decay <= 1.0)` — NaN fails both comparisons and reverts.
    let decay = if decay > 0.0 && decay <= 1.0 {
        decay
    } else {
        DEFAULT_WEIGHTED_DECAY
    };

    // Newest → oldest, so weight 1.0 lands on today's spend. Plain `+=`, not a
    // compensated sum: Python writes the loop out.
    let mut total_weight = 0.0_f64;
    let mut total = 0.0_f64;
    let mut weight = 1.0_f64;
    for value in tail.iter().rev() {
        total += value * weight;
        total_weight += weight;
        weight *= decay;
    }
    if total_weight == 0.0 {
        return 0.0;
    }
    total / total_weight
}

/// `pick_projection_method(daily_costs)`.
///
/// Counts *strictly positive* days in the RAW series — before any clamping — so
/// three quiet days keep the projection linear even though the weighted mean
/// would happily compute over them.
#[must_use]
pub fn pick_projection_method(daily_costs: &[f64]) -> ProjectionMethod {
    if daily_costs.iter().filter(|c| **c > 0.0).count() >= MIN_SAMPLES_FOR_WEIGHTED {
        ProjectionMethod::Weighted7d
    } else {
        ProjectionMethod::Linear
    }
}

/// `days_to_limit(spent, daily_avg, limit)` — calendar days until the cap.
///
/// `None` when the answer is undefined: no burn, no limit, or already over.
/// Otherwise the integer FLOOR of remaining ÷ burn, so a "days left" callout
/// never promises a fraction of a day. The math is not constrained to the
/// billing window; comparing against `days_in_period - days_so_far` is the
/// caller's job.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "int(float) — Python yields an arbitrary-precision int, `as i64` saturates (DIV-094)"
)]
pub fn days_to_limit(spent: f64, daily_avg: f64, limit: f64) -> Option<i64> {
    if limit <= 0.0 || daily_avg <= 0.0 || spent >= limit {
        return None;
    }
    let remaining = limit - spent;
    Some(py_floordiv(remaining, daily_avg) as i64)
}

/// Python's `x // y` on two floats, then `int(...)`.
///
/// Transcribed from CPython's `float_divmod`: `fmod` first, then a correction
/// pass, then a `floor` with a half-ULP nudge. `(remaining / daily).floor()`
/// agrees with it almost everywhere and disagrees exactly where the quotient
/// lands a hair under an integer — which is where a "3 days left" turns into
/// "2".
fn py_floordiv(numerator: f64, denominator: f64) -> f64 {
    let modulus = libm_fmod(numerator, denominator);
    let mut div = (numerator - modulus) / denominator;
    // The `else` leg in C is `mod = copysign(0.0, wx)`, which only sets the sign
    // of a remainder this function then discards — so it has no counterpart here.
    if modulus != 0.0 && ((denominator < 0.0) != (modulus < 0.0)) {
        div -= 1.0;
    }
    if div == 0.0 {
        // `floordiv = copysign(0.0, vx / wx)` — a signed zero, and its sign is
        // observable through `1.0 / result`.
        return (numerator / denominator).signum() * 0.0;
    }
    let mut floordiv = div.floor();
    if div - floordiv > 0.5 {
        floordiv += 1.0;
    }
    floordiv
}

/// C's `fmod` — the truncated remainder, which is what CPython calls.
///
/// Rust's `%` on `f64` is already `fmod` (truncated, sign of the dividend), so
/// this is a named alias rather than an implementation; the name is here so the
/// transcription above reads against the C.
fn libm_fmod(numerator: f64, denominator: f64) -> f64 {
    numerator % denominator
}

/// `crossed_thresholds(pct, thresholds)` — the *highest* threshold met.
///
/// "Show one alert line, not three": only the most severe tripped threshold is
/// surfaced. `None` when none of them have been.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "float(t) — a threshold is a percentage, far under 2^53"
)]
pub fn crossed_thresholds(pct: f64, thresholds: &[i64]) -> Option<i64> {
    thresholds
        .iter()
        .copied()
        .filter(|threshold| pct >= *threshold as f64)
        .max()
}

/// The dict `build_projection` returns. Field order is the dict-literal order.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// `projected_month_end_usd` — the full-month forecast, `used` included.
    pub projected_month_end_usd: f64,
    /// `projection_method` — which estimator actually produced the burn.
    pub projection_method: ProjectionMethod,
    /// `daily_burn_usd` — the rate the projection extrapolated.
    pub daily_burn_usd: f64,
    /// `days_to_limit` — `None` renders as `null`.
    pub days_to_limit: Option<i64>,
    /// `thresholds` — deduplicated and sorted ascending.
    pub thresholds: Vec<i64>,
    /// `crossed_threshold` — the highest one met, or `null`.
    pub crossed_threshold: Option<i64>,
    /// `alert` — one human-readable banner, or `null`.
    pub alert: Option<String>,
}

/// `build_projection(...)` — compose the block the route, the CLI and the MCP
/// surface all emit.
///
/// `thresholds` of `None` **or of an empty slice** both fall back to
/// [`DEFAULT_THRESHOLDS`]; that is `thresholds or DEFAULT_THRESHOLDS`, and it is
/// truthiness, not a null check. `method` of `None` runs
/// [`pick_projection_method`].
#[must_use]
pub fn build_projection(
    daily_costs: &[f64],
    used: f64,
    budget: f64,
    days_so_far: i64,
    days_in_period: i64,
    thresholds: Option<&[i64]>,
    method: Option<ProjectionMethod>,
) -> Projection {
    // `sorted({int(t) for t in (thresholds or DEFAULT_THRESHOLDS)})` — a SET,
    // so duplicates collapse, and then sorted ascending.
    let source = match thresholds {
        Some(list) if !list.is_empty() => list,
        _ => &DEFAULT_THRESHOLDS,
    };
    let mut threshold_list: Vec<i64> = source.to_vec();
    threshold_list.sort_unstable();
    threshold_list.dedup();

    let mut chosen = method.unwrap_or_else(|| pick_projection_method(daily_costs));
    let daily_burn = if chosen == ProjectionMethod::Weighted7d {
        let weighted =
            weighted_projection(daily_costs, DEFAULT_WEIGHTED_WINDOW, DEFAULT_WEIGHTED_DECAY);
        // Stale-store fallback: seven silent days collapse the weighted figure
        // to exactly $0 and would forecast a $0 month-end. That is *technically*
        // right and usually misleading, so the whole-period linear mean takes
        // over — and `projection_method` flips with it, which is the only signal
        // the user gets that it happened.
        if weighted == 0.0 {
            let linear_burn = linear_projection(daily_costs);
            if linear_burn > 0.0 {
                chosen = ProjectionMethod::Linear;
                linear_burn
            } else {
                weighted
            }
        } else {
            weighted
        }
    } else {
        linear_projection(daily_costs)
    };

    let days_left = 0.max(days_in_period - days_so_far);
    #[allow(
        clippy::cast_precision_loss,
        reason = "days_left is a count of days in a billing window"
    )]
    let projected = used + daily_burn * days_left as f64;

    let pct = if budget > 0.0 {
        100.0 * used / budget
    } else {
        0.0
    };
    let crossed = crossed_thresholds(pct, &threshold_list);
    let dtl = days_to_limit(used, daily_burn, budget);

    let alert = alert_message(crossed, dtl, days_left, budget, projected);

    Projection {
        projected_month_end_usd: projected,
        projection_method: chosen,
        daily_burn_usd: daily_burn,
        days_to_limit: dtl,
        thresholds: threshold_list,
        crossed_threshold: crossed,
        alert,
    }
}

/// `_alert_message(...)` — one banner, or `None`.
///
/// Priority, in the order the Python tests it:
/// 1. Projected to overrun (`projected > budget * 1.0001`, a rounding epsilon),
///    with a "by day N" note when `days_to_limit` lands inside the window.
/// 2. Otherwise, a crossed threshold.
/// 3. Otherwise nothing, and the UI suppresses the line.
///
/// Note that leg 1 subsumes the already-over case: once `used > budget`,
/// `days_to_limit` is `None`, so the branchless "Projected to exceed plan"
/// wording is what an over-budget user actually sees.
#[must_use]
pub fn alert_message(
    crossed: Option<i64>,
    days_to_limit_value: Option<i64>,
    days_left: i64,
    budget: f64,
    projected: f64,
) -> Option<String> {
    if budget > 0.0 && projected > budget * 1.0001 {
        return Some(match days_to_limit_value {
            Some(days) if (0..=days_left).contains(&days) => {
                // The f-string is three concatenated pieces and the plural `s`
                // sits between them, so the space before "(forecast" belongs to
                // the middle piece.
                let plural = if days == 1 { "" } else { "s" };
                let mut out = format!("Projected to hit plan limit in ~{days} day{plural} ");
                let _ = write!(out, "(forecast ${})", money(projected));
                out
            }
            _ => format!(
                "Projected to exceed plan: ${} vs ${}",
                money(projected),
                money(budget)
            ),
        });
    }

    crossed.map(|threshold| format!("Crossed {threshold}% of plan budget"))
}

/// `f"{value:,.2f}"` — two decimals with thousands separators.
///
/// Rust's `{:.2}` and CPython's `.2f` are both correctly-rounded, ties-to-even
/// conversions of the exact binary value, so only the grouping has to be added
/// (checked against CPython on `0.125` / `0.135` / `2.675` / `1.005`, the usual
/// half-way suspects).
fn money(value: f64) -> String {
    let rendered = format!("{value:.2}");
    let (sign, body) = match rendered.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", rendered.as_str()),
    };
    // `inf` / `nan` have no decimal point and no grouping.
    let Some((whole, fraction)) = body.split_once('.') else {
        return rendered;
    };
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{sign}{grouped}.{fraction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linear_mean_counts_clamped_days_in_the_denominator() {
        // A negative day becomes 0.0 but still divides — the mean of
        // [10, -5, 20] is 30/3, not 30/2.
        assert_eq!(linear_projection(&[10.0, -5.0, 20.0]), 10.0);
        assert_eq!(linear_projection(&[]), 0.0);
        assert_eq!(linear_projection(&[0.0, 0.0]), 0.0);
        // NaN loses to 0.0 in `max(0.0, c)`, so it does not poison the mean.
        assert_eq!(linear_projection(&[f64::NAN, 6.0]), 3.0);
    }

    #[test]
    fn the_linear_mean_is_a_compensated_sum_not_an_accumulator() {
        // 1e16 followed by ten 1.0s: a plain `+=` chain loses every one of them
        // (1e16 + 1.0 == 1e16), while CPython's `sum()` keeps them in the
        // compensation term. The mean therefore differs in the last digits.
        let mut series = vec![1e16_f64];
        series.extend(std::iter::repeat_n(1.0_f64, 10));
        let mut naive = 0.0_f64;
        for value in &series {
            naive += *value;
        }
        #[allow(clippy::cast_precision_loss, reason = "11 elements")]
        let count = series.len() as f64;
        assert_ne!(linear_projection(&series), naive / count);
        assert_eq!(linear_projection(&series), (1e16 + 10.0) / 11.0);
    }

    #[test]
    fn the_weighted_mean_puts_full_weight_on_the_last_element() {
        // Oldest-first: [0, 0, 10] with decay 0.85 weights today 1.0, yesterday
        // 0.85, the day before 0.7225.
        let value = weighted_projection(&[0.0, 0.0, 10.0], 7, 0.85);
        assert!((value - 10.0 / (1.0 + 0.85 + 0.7225)).abs() < 1e-12);
        // Reverse the series and the same numbers give a much smaller answer —
        // orientation is the contract, not a detail.
        let reversed = weighted_projection(&[10.0, 0.0, 0.0], 7, 0.85);
        assert!(reversed < value);
    }

    #[test]
    fn a_decay_of_one_collapses_the_weighted_mean_to_a_plain_one() {
        assert!((weighted_projection(&[1.0, 2.0, 3.0], 7, 1.0) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn an_out_of_range_window_or_decay_silently_reverts_to_the_defaults() {
        let baseline = weighted_projection(&[1.0, 2.0, 3.0], 7, 0.85);
        // `if window <= 0: window = DEFAULT_WEIGHTED_WINDOW`
        assert_eq!(weighted_projection(&[1.0, 2.0, 3.0], 0, 0.85), baseline);
        assert_eq!(weighted_projection(&[1.0, 2.0, 3.0], -4, 0.85), baseline);
        // `if not (0.0 < decay <= 1.0): decay = DEFAULT_WEIGHTED_DECAY`
        assert_eq!(weighted_projection(&[1.0, 2.0, 3.0], 7, 0.0), baseline);
        assert_eq!(weighted_projection(&[1.0, 2.0, 3.0], 7, 1.5), baseline);
        assert_eq!(weighted_projection(&[1.0, 2.0, 3.0], 7, f64::NAN), baseline);
        assert_eq!(weighted_projection(&[], 7, 0.85), 0.0);
    }

    #[test]
    fn the_window_slices_the_tail_and_ignores_older_days() {
        // Eight days, window 7: the leading $1000 spike is outside the window.
        let series = [1000.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        assert!((weighted_projection(&series, 7, 0.85) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_method_heuristic_counts_positive_days_in_the_raw_series() {
        assert_eq!(
            pick_projection_method(&[1.0, 2.0, 3.0]),
            ProjectionMethod::Weighted7d
        );
        // Two positives and a zero is not three samples.
        assert_eq!(
            pick_projection_method(&[1.0, 0.0, 3.0]),
            ProjectionMethod::Linear
        );
        // Negatives are excluded BEFORE the clamp, so they do not count either.
        assert_eq!(
            pick_projection_method(&[1.0, -2.0, 3.0]),
            ProjectionMethod::Linear
        );
        assert_eq!(pick_projection_method(&[]), ProjectionMethod::Linear);
    }

    #[test]
    fn days_to_limit_floors_and_returns_none_for_every_undefined_case() {
        // 100 budget, 40 spent, $7/day → 60/7 = 8.57 → 8.
        assert_eq!(days_to_limit(40.0, 7.0, 100.0), Some(8));
        // Exactly divisible is the whole number, not one less.
        assert_eq!(days_to_limit(40.0, 10.0, 100.0), Some(6));
        assert_eq!(days_to_limit(40.0, 0.0, 100.0), None); // no burn
        assert_eq!(days_to_limit(40.0, -1.0, 100.0), None); // negative burn
        assert_eq!(days_to_limit(100.0, 7.0, 100.0), None); // already at the cap
        assert_eq!(days_to_limit(140.0, 7.0, 100.0), None); // already over
        assert_eq!(days_to_limit(40.0, 7.0, 0.0), None); // no plan
    }

    #[test]
    fn the_floor_division_is_pythons_not_a_naive_truncation() {
        // CPython's float `//` computes `(x - fmod(x, y)) / y` and then floors
        // with a half-ULP nudge, which is not always what `(x / y).floor()`
        // gives. Both spellings must agree on the ordinary cases, and the
        // transcribed one is the reference.
        assert_eq!(py_floordiv(60.0, 7.0), 8.0);
        assert_eq!(py_floordiv(60.0, 10.0), 6.0);
        assert_eq!(py_floordiv(1.0, 0.3), 3.0);
        assert_eq!(py_floordiv(0.5, 1.0), 0.0);
    }

    #[test]
    fn only_the_highest_crossed_threshold_is_surfaced() {
        assert_eq!(crossed_thresholds(92.0, &DEFAULT_THRESHOLDS), Some(90));
        assert_eq!(crossed_thresholds(75.0, &DEFAULT_THRESHOLDS), Some(75));
        assert_eq!(crossed_thresholds(74.9, &DEFAULT_THRESHOLDS), Some(50));
        assert_eq!(crossed_thresholds(10.0, &DEFAULT_THRESHOLDS), None);
        assert_eq!(crossed_thresholds(10.0, &[]), None);
        // An out-of-order list is fine — `max()` does not need it sorted.
        assert_eq!(crossed_thresholds(80.0, &[90, 25, 60]), Some(60));
    }

    #[test]
    fn an_empty_threshold_list_falls_back_to_the_defaults_but_a_zero_does_not() {
        // `thresholds or DEFAULT_THRESHOLDS` — Python truthiness. This is the
        // one line where `Some(&[])` and `None` MUST behave identically, and
        // where `[0]` must not.
        let empty = build_projection(&[], 0.0, 100.0, 1, 30, Some(&[]), None);
        assert_eq!(empty.thresholds, vec![50, 75, 90]);
        let none = build_projection(&[], 0.0, 100.0, 1, 30, None, None);
        assert_eq!(none.thresholds, vec![50, 75, 90]);

        let zero = build_projection(&[], 0.0, 100.0, 1, 30, Some(&[0]), None);
        assert_eq!(zero.thresholds, vec![0]);
        // pct is 0.0 and the threshold is 0, so `pct >= 0` crosses it.
        assert_eq!(zero.crossed_threshold, Some(0));
        assert_eq!(zero.alert.as_deref(), Some("Crossed 0% of plan budget"));
    }

    #[test]
    fn thresholds_are_deduplicated_and_sorted() {
        let projection = build_projection(&[], 0.0, 100.0, 1, 30, Some(&[90, 50, 90, 10]), None);
        assert_eq!(projection.thresholds, vec![10, 50, 90]);
    }

    #[test]
    fn a_quiet_recent_week_falls_back_to_linear_and_says_so() {
        // Twelve days: five with spend, then seven silent ones. weighted-7d is
        // exactly 0.0 over that tail, so the whole-period linear mean takes
        // over AND the reported method flips.
        let mut series = vec![10.0; 5];
        series.extend(std::iter::repeat_n(0.0_f64, 7));
        let projection = build_projection(&series, 50.0, 500.0, 12, 30, None, None);
        assert_eq!(projection.projection_method, ProjectionMethod::Linear);
        assert!((projection.daily_burn_usd - 50.0 / 12.0).abs() < 1e-12);

        // With no spend at all there is nothing to fall back TO, so the method
        // stays weighted-7d with a zero burn.
        let silent = build_projection(&[0.0; 12], 0.0, 500.0, 12, 30, None, None);
        assert_eq!(silent.projection_method, ProjectionMethod::Linear);
        // (…linear, because zero positive days never picks weighted in the
        // first place. The fallback needs a series that DID pick weighted.)
        let spiky = build_projection(
            &[1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            3.0,
            500.0,
            10,
            30,
            None,
            Some(ProjectionMethod::Weighted7d),
        );
        assert_eq!(spiky.projection_method, ProjectionMethod::Linear);
    }

    #[test]
    fn an_explicit_method_overrides_the_heuristic() {
        let series = [10.0, 10.0, 10.0, 10.0];
        let forced = build_projection(
            &series,
            40.0,
            500.0,
            4,
            30,
            None,
            Some(ProjectionMethod::Linear),
        );
        assert_eq!(forced.projection_method, ProjectionMethod::Linear);
        assert_eq!(forced.daily_burn_usd, 10.0);
    }

    #[test]
    fn the_projection_adds_the_tail_to_what_is_already_spent() {
        let series = [10.0, 10.0, 10.0];
        let projection = build_projection(&series, 30.0, 500.0, 3, 30, None, None);
        // 27 days left at ~$10/day on top of the $30 already spent.
        assert!((projection.projected_month_end_usd - 300.0).abs() < 1e-9);
        // …and `days_to_limit` is 46, not the 47 the arithmetic "should" give.
        // The weighted mean of three equal $10 days is 10.000000000000002, not
        // 10.0, so $470 of headroom buys 46 whole days. CPython prints exactly
        // this (`(500.0-30.0) // (25.725/2.5725)` -> 46.0); an implementation
        // that rounded the burn first, or that used a tolerant division, would
        // answer 47 and be wrong in the same visible way the alert line is.
        assert_eq!(projection.days_to_limit, Some(46));
    }

    #[test]
    fn the_overrun_alert_names_the_day_when_the_limit_lands_inside_the_window() {
        // $8/day against a $100 plan with $40 spent and 20 days left: the limit
        // arrives in 7 days, inside the window, so the dated wording wins.
        let series = [8.0, 8.0, 8.0, 8.0, 8.0];
        let projection = build_projection(&series, 40.0, 100.0, 10, 30, None, None);
        assert_eq!(
            projection.alert.as_deref(),
            Some("Projected to hit plan limit in ~7 days (forecast $200.00)")
        );
        assert_eq!(projection.days_to_limit, Some(7));
    }

    #[test]
    fn the_day_count_in_the_alert_is_singular_at_exactly_one() {
        assert_eq!(
            alert_message(None, Some(1), 5, 100.0, 200.0).as_deref(),
            Some("Projected to hit plan limit in ~1 day (forecast $200.00)")
        );
        assert_eq!(
            alert_message(None, Some(0), 5, 100.0, 200.0).as_deref(),
            Some("Projected to hit plan limit in ~0 days (forecast $200.00)")
        );
        // Beyond the remaining window, the undated wording is used instead.
        assert_eq!(
            alert_message(None, Some(9), 5, 100.0, 200.0).as_deref(),
            Some("Projected to exceed plan: $200.00 vs $100.00")
        );
    }

    #[test]
    fn already_over_budget_reads_as_the_undated_overrun_line() {
        // `days_to_limit` is None once spent >= limit, so leg 1's `is None`
        // branch is what an over-budget user sees.
        let projection = build_projection(&[20.0, 20.0, 20.0], 120.0, 100.0, 3, 30, None, None);
        assert_eq!(projection.days_to_limit, None);
        assert_eq!(projection.crossed_threshold, Some(90));
        assert!(
            projection
                .alert
                .as_deref()
                .expect("over budget alerts")
                .starts_with("Projected to exceed plan: $660.00 vs $100.00")
        );
    }

    #[test]
    fn a_forecast_inside_the_rounding_epsilon_is_not_an_overrun() {
        // `projected > budget * 1.0001` — a forecast one cent over a $100 plan
        // is inside the epsilon and reports the crossed threshold instead.
        assert_eq!(
            alert_message(Some(90), None, 5, 100.0, 100.01).as_deref(),
            Some("Crossed 90% of plan budget")
        );
        assert_eq!(
            alert_message(Some(90), None, 5, 100.0, 100.02).as_deref(),
            Some("Projected to exceed plan: $100.02 vs $100.00")
        );
        // Nothing crossed and nothing projected is a suppressed line.
        assert_eq!(alert_message(None, None, 5, 100.0, 10.0), None);
        // A zero budget cannot overrun.
        assert_eq!(alert_message(None, None, 5, 0.0, 1e9), None);
    }

    #[test]
    fn the_dollar_formatter_groups_thousands_the_way_pythons_comma_spec_does() {
        assert_eq!(money(1234.567), "1,234.57");
        assert_eq!(money(999.0), "999.00");
        assert_eq!(money(1000.0), "1,000.00");
        assert_eq!(money(1_234_567.891), "1,234,567.89");
        assert_eq!(money(-1234.5), "-1,234.50");
        assert_eq!(money(0.0), "0.00");
        // The half-way cases CPython and Rust must agree on.
        assert_eq!(money(0.125), "0.12");
        assert_eq!(money(0.135), "0.14");
        assert_eq!(money(2.675), "2.67");
    }

    #[test]
    fn the_method_literal_is_the_hyphenated_spelling_the_ui_switches_on() {
        assert_eq!(ProjectionMethod::Weighted7d.as_str(), "weighted-7d");
        assert_eq!(ProjectionMethod::Linear.as_str(), "linear");
    }
}

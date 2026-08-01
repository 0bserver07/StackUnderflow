//! Port of `stackunderflow/stats/aggregator.py` — the whole eighteen-section
//! statistics payload (RS-3-062).
//!
//! [`summarise`] is a transcription, section by section, in Python's order. The
//! wire contract it produces is not just the *values*: `json.dumps` writes a
//! dict in insertion order and renders `5` and `5.0` differently, so key order
//! and the int/float split are as load-bearing here as the arithmetic. Three
//! rules follow from that and are applied everywhere below.
//!
//! 1. **Every map is insertion-ordered.** Python's `dict` and `Counter` are;
//!    [`OrderMap`] is the local stand-in, and the output objects are built by
//!    inserting keys in the order the Python source writes them (the workspace
//!    turns on serde_json's `preserve_order`).
//! 2. **`sum([])` is `int` 0 and `0.0` is not.** `_CommandCostCollector` seeds
//!    its cost with a bare `sum(...)`, so an interaction with no priced
//!    response serialises `"cost": 0`, while `_SessionCostCollector` seeds
//!    `cost = 0.0` and serialises `"cost": 0.0` for the same emptiness. Ten
//!    places make that distinction; [`PyNum`] carries it.
//! 3. **Accumulation order is arithmetic.** Floats are summed in Python's
//!    iteration order — insertion order of the `by_model` dict, append order of
//!    the duration list — because reassociating a float sum moves the last
//!    bits, which is exactly what the parity gate measures.
//!
//! # `round()` is not `f64::round`
//!
//! Python's two-argument `round` is correctly-rounded *decimal* rounding with
//! ties to even (`Python/pymath.c` → `_Py_dg_dtoa`, mode 3), and Rust's
//! `f64::round` is half-away-from-zero on the binary value. Fourteen fields go
//! through it. [`round_py`] is the one implementation; nothing calls
//! `f64::round`.
//!
//! # Injection, not module state
//!
//! Python reaches `infra.costs.compute_cost` through module globals. Here the
//! [`PricingEngine`] is a parameter, per the campaign's `set_var` ban and
//! findings ledger #5. `provider` is still resolved exactly as Python resolves
//! it — `ds.records[0].provider`, once, for the whole dataset.
//!
//! # `_safe` and the exceptions this port cannot raise
//!
//! Python wraps nine sections in `_safe(fn, fallback)`, which swallows *any*
//! exception. This port is total, so those wrappers are almost all no-ops —
//! with one exception that is real and reproduced: `_trends` compares a naive
//! `datetime` with an aware one **outside** its own `try`, so a project whose
//! timestamps mix awareness falls back to `_empty_trends()`. See [`trends`].
//! The two places where Python raises and this port cannot are recorded as
//! DIV-062 and DIV-063 in the module's divergence notes below.
//!
//! # Divergences filed by this module
//!
//! * **DIV-060** — `models_used` / `by_model` keys coerce a non-string
//!   `message.model` to its JSON dict-key spelling. Python keeps the object and
//!   `sorted()` on a mixed-type set raises `TypeError`. Unreachable on the live
//!   store (every `model` is a string or absent).
//! * **DIV-061** — a non-integer `usage.*_tokens` value is truncated to an
//!   `i64` (see `enricher::usage_from`); Python would carry the float through
//!   every sum. Counted, not assumed: `enricher::non_integer_token_count`.
//! * **DIV-062** — `_is_search_invocation` calls `.get("command", "")` on a
//!   tool block's `input` without checking it is a dict. A non-dict `input`
//!   raises `AttributeError` in Python, which is **not** caught by `_safe`
//!   (`user_interactions` is not wrapped) and takes down the whole payload.
//!   Here it reads as an absent command. Counted by [`div_062_count`].
//! * **DIV-063** — the same call site does `cmd.lower()` on a non-string
//!   `command`. Same shape, same counter.

use serde_json::{Map, Value};

use super::classifier::{INTERRUPT_API, INTERRUPT_PREFIX};
use super::enricher::{EnrichedDataset, Interaction, Record, TokenBag, ToolRef};
use super::pydatetime::{local_day, local_hour, parse_ts};
use super::pytext::{py_char_prefix, py_strip, py_truthy};
use crate::pricing::{PricingEngine, RawTokens};

// ── small Python-shaped primitives ──────────────────────────────────────────

/// An insertion-ordered string-keyed map — Python's `dict` / `Counter`.
///
/// `std::collections::HashMap` would randomise the key order of eleven output
/// objects (`tools.usage_counts`, `models`, `daily_stats`, `token_composition.
/// daily`, …), and `BTreeMap` would sort them, which Python does not.
#[derive(Debug, Clone)]
pub struct OrderMap<V> {
    keys: Vec<String>,
    index: std::collections::HashMap<String, usize>,
    values: Vec<V>,
}

impl<V> Default for OrderMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> OrderMap<V> {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            index: std::collections::HashMap::new(),
            values: Vec::new(),
        }
    }

    /// `d.setdefault(key, default())` — returns a mutable reference either way.
    pub fn entry(&mut self, key: &str, default: impl FnOnce() -> V) -> &mut V {
        if let Some(&i) = self.index.get(key) {
            return &mut self.values[i];
        }
        self.index.insert(key.to_string(), self.values.len());
        self.keys.push(key.to_string());
        self.values.push(default());
        self.values
            .last_mut()
            .expect("just pushed, so the vector is non-empty")
    }

    /// `d.get(key)`.
    pub fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&i| &self.values[i])
    }

    /// `d.items()`, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.keys.iter().map(String::as_str).zip(self.values.iter())
    }

    /// `len(d)`.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `not d`.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl OrderMap<i64> {
    /// `counter[key] += by`.
    fn incr(&mut self, key: &str, by: i64) {
        *self.entry(key, || 0) += by;
    }

    /// `counter[key]` — a Counter read never inserts.
    fn count(&self, key: &str) -> i64 {
        self.get(key).copied().unwrap_or(0)
    }

    /// `dict(counter)`.
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        for (k, v) in self.iter() {
            map.insert(k.to_string(), (*v).into());
        }
        Value::Object(map)
    }
}

/// A number that knows whether Python would call it an `int` or a `float`.
///
/// `json.dumps` writes `0` for one and `0.0` for the other, and the difference
/// is produced by expressions like `x if cond else 0` and `sum(empty)` rather
/// than by any declared type. Ten fields need it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PyNum {
    /// A Python `int`.
    Int(i64),
    /// A Python `float`.
    Float(f64),
}

impl PyNum {
    /// The value as an `f64`, for comparisons and sort keys.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }

    /// `json.dumps` of this number.
    ///
    /// A non-finite `float` has no JSON literal; CPython writes the bare tokens
    /// `NaN` / `Infinity`, which nothing downstream parses. It is unreachable
    /// from any arithmetic here (every input is a finite token count or rate),
    /// and `Value::Null` is the honest stand-in for "CPython would have written
    /// something no parser accepts".
    #[must_use]
    pub fn to_json(self) -> Value {
        match self {
            Self::Int(i) => i.into(),
            Self::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        }
    }
}

/// `float` → `Value`, keeping `0.0` a float. See [`PyNum::to_json`].
#[must_use]
pub fn jf(value: f64) -> Value {
    PyNum::Float(value).to_json()
}

/// `int` → `Value`.
#[must_use]
pub fn ji(value: i64) -> Value {
    Value::from(value)
}

/// `usize` → `Value` as a Python `int`.
#[allow(clippy::cast_possible_wrap)]
fn jz(value: usize) -> Value {
    Value::from(value as i64)
}

/// The string `json.dumps` writes for a value used as a `dict` KEY.
///
/// `str` keys pass through; CPython coerces `None`/`True`/`False`/numbers and
/// rejects everything else. The aggregator keys dicts on `Record.model` and on
/// tool names, both of which are wire values and neither of which is validated.
#[must_use]
pub fn py_dict_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        // DIV-060: a container is unhashable in Python; this spelling at least
        // keeps distinct containers distinct.
        other => other.to_string(),
    }
}

/// Python's two-argument `round`.
///
/// CPython computes the correctly-rounded decimal expansion (`_Py_dg_dtoa`,
/// mode 3, ties to even) and reads it back with `_Py_dg_strtod`. Rust's
/// `{:.*}` formatter is the same correctly-rounded, ties-to-even conversion,
/// so formatting and re-parsing is the same two steps in the same order —
/// which is why this is a two-liner and not a decimal library.
///
/// `f64::round` is *not* this: it is half-away-from-zero on the binary value
/// and would move `round(0.125, 2)` from `0.12` to `0.13`.
#[must_use]
pub fn round_py(value: f64, ndigits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.ndigits$}").parse().unwrap_or(value)
}

/// `min(100, x)` where `100` is an `int` and `x` a `float`.
///
/// CPython's `min` returns the *object* that compared smaller, so `min(100,
/// 250.0)` is the `int` `100` and `round(100, 1)` is the `int` `100`. That
/// reaches `cache.efficiency` as `100`, not `100.0`.
fn min_100(value: PyNum) -> PyNum {
    if value.as_f64() < 100.0 {
        value
    } else {
        PyNum::Int(100)
    }
}

/// `aggregator._preview`.
#[must_use]
pub fn preview(text: &str, limit: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let flattened: String = text.replace(['\n', '\r'], " ");
    py_char_prefix(py_strip(&flattened), limit).to_string()
}

/// `PurePath(s).name` — the last component, ignoring `.` and trailing slashes.
fn path_name(path: &str) -> String {
    path.split('/')
        .rfind(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
        .to_string()
}

// ── DIV-062 / DIV-063 counters ──────────────────────────────────────────────

static DIV_062: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many tool blocks this process has seen whose `input` was not a dict, or
/// whose `input.command` was not a string — the two shapes Python raises
/// `AttributeError` on inside `_is_search_invocation`. See the module docs.
#[must_use]
pub fn div_062_count() -> u64 {
    DIV_062.load(std::sync::atomic::Ordering::Relaxed)
}

// ── cost helpers ────────────────────────────────────────────────────────────

/// One `(model, speed)` bucket of accumulated tokens — the shape every cost
/// collector groups on so the priority/fast multiplier applies only to the
/// records that used it.
#[derive(Debug, Clone)]
struct ModelBucket {
    model: Value,
    speed: String,
    tokens: TokenBag,
}

impl ModelBucket {
    fn of(rec: &Record) -> Self {
        Self {
            model: rec.model.clone(),
            speed: rec.speed.clone(),
            tokens: TokenBag::default(),
        }
    }
}

/// `compute_cost(dict(tokens), model, provider=…, speed=…)["total_cost"]`.
fn total_cost(engine: &PricingEngine, bucket: &ModelBucket, provider: &str) -> f64 {
    engine
        .compute_cost(
            &bucket.tokens.raw(),
            &py_dict_key(&bucket.model),
            provider,
            &bucket.speed,
            None,
        )
        .total_cost
}

/// `sum(compute_cost(...)["total_cost"] for (model, speed), tokens in d.items())`
/// — an `int` `0` when the map is empty, a `float` otherwise, summed in
/// insertion order.
fn sum_bucket_costs(
    engine: &PricingEngine,
    buckets: &OrderMap<ModelBucket>,
    provider: &str,
) -> PyNum {
    let mut acc = Neumaier::default();
    for (_, bucket) in buckets.iter() {
        acc.add(total_cost(engine, bucket, provider));
    }
    acc.finish_pynum()
}

/// `compute_cost({"input": 0, "output": tokens, …}, model, …)["total_cost"]` —
/// the output-only charge `_RetryCollector` and `_ErrorCostCollector` apply.
fn output_only_cost(
    engine: &PricingEngine,
    tokens: i64,
    model: &Value,
    provider: &str,
    speed: &str,
) -> f64 {
    engine
        .compute_cost(
            &RawTokens::canonical(0, tokens, 0, 0),
            &py_dict_key(model),
            provider,
            speed,
            None,
        )
        .total_cost
}

/// `rec.model and rec.model != "N/A"` for an [`Interaction`]'s rolled-up model.
fn model_named(model: &Value) -> bool {
    py_truthy(model) && model.as_str() != Some("N/A")
}

// ── public entry ────────────────────────────────────────────────────────────

/// `aggregator.summarise` — the full statistics dict matching the API contract.
///
/// Sections are emitted in the order the Python `return` statement lists them,
/// because that is the order `json.dumps` writes.
#[must_use]
pub fn summarise(
    ds: &EnrichedDataset,
    log_dir: &str,
    tz_offset: i64,
    engine: &PricingEngine,
) -> Value {
    // All records in a single dataset come from one project, so they share a
    // provider. Resolved once, exactly as Python resolves it.
    let provider: &str = ds
        .records
        .first()
        .map_or("anthropic", |r| r.provider.as_str());

    let mut tools_c = ToolsCollector::default();
    let mut models_c = ModelsCollector::default();
    let mut sessions_c = SessionsCollector::default();
    let mut errors_c = ErrorsCollector::default();
    let mut cache_c = CacheCollector::default();
    let mut sess_cost_c = SessionCostCollector::default();
    let mut tool_cost_c = ToolCostCollector::default();
    let mut token_comp_c = TokenCompositionCollector::new(tz_offset);
    let mut sess_eff_c = SessionEfficiencyCollector::default();
    let mut err_cost_c = ErrorCostCollector::default();

    for rec in &ds.records {
        tools_c.ingest(rec);
        models_c.ingest(rec);
        sessions_c.ingest(rec);
        errors_c.ingest(rec);
        cache_c.ingest(rec);
        sess_cost_c.ingest(rec);
        tool_cost_c.ingest(rec, engine, provider);
        token_comp_c.ingest(rec);
        sess_eff_c.ingest(rec);
        err_cost_c.ingest(rec);
    }

    let mut cmd_cost_c = CommandCostCollector::default();
    let mut outlier_c = OutlierCollector::default();
    let mut retry_c = RetryCollector::default();
    for ix in &ds.interactions {
        cmd_cost_c.ingest(ix, ds, engine, provider);
        outlier_c.ingest(ix, ds, engine, provider);
        retry_c.ingest(ix, ds, engine, provider);
    }

    let mut out = Map::new();
    out.insert(
        "overview".into(),
        build_overview(ds, log_dir, engine, provider),
    );
    out.insert("tools".into(), tools_c.result());
    out.insert("sessions".into(), sessions_c.result());
    out.insert(
        "daily_stats".into(),
        daily(&ds.records, tz_offset, engine, provider),
    );
    out.insert("hourly_pattern".into(), hourly(&ds.records, tz_offset));
    out.insert("errors".into(), errors_c.result(&ds.records));
    out.insert("models".into(), models_c.result());
    out.insert("user_interactions".into(), command_analysis(ds));
    out.insert("cache".into(), cache_c.result(engine, provider));
    // ── analytics expansion (docs/specs/analytics-expansion.md §1) ──────
    out.insert(
        "session_costs".into(),
        sess_cost_c.result(ds, engine, provider),
    );
    out.insert("command_costs".into(), cmd_cost_c.result());
    out.insert("tool_costs".into(), tool_cost_c.result());
    out.insert("token_composition".into(), token_comp_c.result());
    out.insert("outliers".into(), outlier_c.result());
    out.insert("retry_signals".into(), retry_c.result());
    out.insert("session_efficiency".into(), sess_eff_c.result());
    out.insert("error_cost".into(), err_cost_c.result(ds, engine, provider));
    out.insert("trends".into(), trends(ds, engine, provider));
    Value::Object(out)
}

/// `aggregator.summarise_session_costs` — just the `session_costs` section.
///
/// `/api/sessions/compare` reads exactly one of [`summarise`]'s eighteen
/// sections. Reaching it through the full sweep means feeding nine other record
/// collectors, three interaction collectors and building overview / daily /
/// hourly / trends, all of it discarded. This runs ONLY
/// [`SessionCostCollector`], over the same records and interactions and with the
/// same provider resolution, so the rows it returns are element-for-element what
/// `summarise(ds, …)["session_costs"]` returns.
///
/// # Why this exists now
///
/// Wave 5 recorded this function as deliberately outside the ported subset:
/// nothing in the mart path or in `get_project_stats` calls it, and the one
/// consumer that does — the compare endpoint — was itself unported (DIV-070).
/// The reason was scope, never a hazard, and batch E's `compare` member closes
/// RS-5-105, so the exclusion is lifted here rather than worked around from the
/// server crate. `stats/mod.rs`'s scope paragraph is updated to match.
///
/// # `_safe` is not reproduced, for the same reason [`summarise`] does not
///
/// Python wraps the call in `_safe(fn, [])` and the route then writes `or []`.
/// Both are exception guards over a port that is total: the collector cannot
/// raise here, and an empty dataset already returns an empty array.
#[must_use]
pub fn summarise_session_costs(ds: &EnrichedDataset, engine: &PricingEngine) -> Value {
    // `ds.records[0].provider if ds.records else "anthropic"` — resolved once
    // for the whole dataset, exactly as `summarise` resolves it.
    let provider: &str = ds
        .records
        .first()
        .map_or("anthropic", |r| r.provider.as_str());

    let mut sess_cost_c = SessionCostCollector::default();
    for rec in &ds.records {
        sess_cost_c.ingest(rec);
    }
    sess_cost_c.result(ds, engine, provider)
}

// ── overview ────────────────────────────────────────────────────────────────

fn build_overview(
    ds: &EnrichedDataset,
    log_dir: &str,
    engine: &PricingEngine,
    provider: &str,
) -> Value {
    let recs = &ds.records;
    let mut tok = TokenBag::default();
    for r in recs {
        tok.add(&r.tokens);
    }

    let mut name = "Unknown Project".to_string();
    let dir_name = path_name(log_dir);
    if let Some(pos) = log_dir.rfind("/.claude/projects/") {
        let tail = &log_dir[pos + "/.claude/projects/".len()..];
        if !tail.is_empty() {
            // `tail.lstrip("-").replace("-", "/").rsplit("/", 1)[-1]`
            let stripped = tail.trim_start_matches('-').replace('-', "/");
            name = stripped
                .rsplit_once('/')
                .map_or(stripped.clone(), |(_, last)| last.to_string());
        }
    }

    let mut kind_counts: OrderMap<i64> = OrderMap::new();
    for r in recs {
        kind_counts.incr(&r.kind, 1);
    }

    let mut sessions: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for r in recs {
        sessions.insert(r.session_id.as_str());
    }

    let mut map = Map::new();
    map.insert("project_name".into(), Value::String(name));
    map.insert("log_dir_name".into(), Value::String(dir_name));
    map.insert("project_path".into(), Value::String(log_dir.to_string()));
    map.insert("total_messages".into(), jz(recs.len()));
    map.insert("date_range".into(), time_bounds(recs));
    map.insert("sessions".into(), jz(sessions.len()));
    map.insert("message_types".into(), kind_counts.to_json());
    map.insert("total_tokens".into(), tok.to_json());
    map.insert(
        "total_cost".into(),
        aggregate_cost(recs, engine, provider).to_json(),
    );
    Value::Object(map)
}

/// `aggregator._time_bounds`.
fn time_bounds(recs: &[Record]) -> Value {
    let mut map = Map::new();
    let stamps: Vec<&str> = recs
        .iter()
        .filter(|r| !r.timestamp.is_empty())
        .map(|r| r.timestamp.as_str())
        .collect();
    if stamps.is_empty() {
        map.insert("start".into(), Value::Null);
        map.insert("end".into(), Value::Null);
    } else {
        map.insert(
            "start".into(),
            Value::String((*stamps.iter().min().expect("non-empty")).to_string()),
        );
        map.insert(
            "end".into(),
            Value::String((*stamps.iter().max().expect("non-empty")).to_string()),
        );
    }
    Value::Object(map)
}

/// `aggregator._aggregate_cost`.
fn aggregate_cost(recs: &[Record], engine: &PricingEngine, provider: &str) -> PyNum {
    let mut by_model: OrderMap<ModelBucket> = OrderMap::new();
    for r in recs {
        if r.kind == "assistant" && r.model_named {
            by_model
                .entry(&r.model_speed_key(), || ModelBucket::of(r))
                .tokens
                .add(&r.tokens);
        }
    }
    sum_bucket_costs(engine, &by_model, provider)
}

// ── collectors ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolsCollector {
    usage: OrderMap<i64>,
    errs: OrderMap<i64>,
}

impl ToolsCollector {
    fn ingest(&mut self, r: &Record) {
        for t in &r.tools {
            self.usage.incr(&t.name_key(), 1);
        }
        if r.is_error {
            for t in &r.tools {
                self.errs.incr(&t.name_key(), 1);
            }
        }
    }

    fn result(&self) -> Value {
        let mut rates = Map::new();
        for (name, count) in self.usage.iter() {
            // `self.errs[n] / c if c else 0` — `c` is a Counter increment so it
            // is never zero for a key that is present, but the guard is ported.
            let value = if *count == 0 {
                PyNum::Int(0)
            } else {
                #[allow(clippy::cast_precision_loss)]
                PyNum::Float(self.errs.count(name) as f64 / *count as f64)
            };
            rates.insert(name.to_string(), value.to_json());
        }
        let mut map = Map::new();
        map.insert("usage_counts".into(), self.usage.to_json());
        map.insert("error_counts".into(), self.errs.to_json());
        map.insert("error_rates".into(), Value::Object(rates));
        Value::Object(map)
    }
}

#[derive(Default)]
struct ModelStats {
    count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
}

#[derive(Default)]
struct ModelsCollector {
    data: OrderMap<ModelStats>,
}

impl ModelsCollector {
    fn ingest(&mut self, r: &Record) {
        // `if r.kind != "assistant" or r.model == "N/A": return` — note this is
        // `== "N/A"`, NOT `not r.model_named`: a null or empty-string model is
        // NOT skipped and becomes a dict key of its own.
        if r.kind != "assistant" || r.model.as_str() == Some("N/A") {
            return;
        }
        let m = self.data.entry(&py_dict_key(&r.model), ModelStats::default);
        m.count += 1;
        m.input_tokens += r.tokens.input;
        m.output_tokens += r.tokens.output;
        m.cache_creation_tokens += r.tokens.cache_creation;
        m.cache_read_tokens += r.tokens.cache_read;
    }

    fn result(&self) -> Value {
        let mut out = Map::new();
        for (model, m) in self.data.iter() {
            let mut entry = Map::new();
            entry.insert("count".into(), ji(m.count));
            entry.insert("input_tokens".into(), ji(m.input_tokens));
            entry.insert("output_tokens".into(), ji(m.output_tokens));
            entry.insert("cache_creation_tokens".into(), ji(m.cache_creation_tokens));
            entry.insert("cache_read_tokens".into(), ji(m.cache_read_tokens));
            out.insert(model.to_string(), Value::Object(entry));
        }
        Value::Object(out)
    }
}

#[derive(Default)]
struct SessionSpan {
    n: i64,
    t0: String,
    t1: String,
    errs: i64,
}

#[derive(Default)]
struct SessionsCollector {
    s: OrderMap<SessionSpan>,
}

impl SessionsCollector {
    fn ingest(&mut self, r: &Record) {
        let s = self.s.entry(&r.session_id, SessionSpan::default);
        s.n += 1;
        if !r.timestamp.is_empty() {
            if s.t0.is_empty() || r.timestamp < s.t0 {
                s.t0.clone_from(&r.timestamp);
            }
            if s.t1.is_empty() || r.timestamp > s.t1 {
                s.t1.clone_from(&r.timestamp);
            }
        }
        if r.is_error {
            s.errs += 1;
        }
    }

    fn result(&self) -> Value {
        let mut durations: Vec<f64> = Vec::new();
        for (_, s) in self.s.iter() {
            if !s.t0.is_empty() && !s.t1.is_empty() {
                // `except (ValueError, TypeError): pass` — a bad parse or a
                // naive/aware mix skips the session entirely.
                if let Some(d) = span_seconds(&s.t0, &s.t1)
                    && d > 0.0
                {
                    durations.push(d);
                }
            }
        }
        let n = self.s.len();
        let avg_duration = if durations.is_empty() {
            PyNum::Int(0)
        } else {
            #[allow(clippy::cast_precision_loss)]
            PyNum::Float(sum_in_order(&durations) / durations.len() as f64)
        };
        let avg_messages = if n == 0 {
            PyNum::Int(0)
        } else {
            let total: i64 = self.s.iter().map(|(_, s)| s.n).sum();
            #[allow(clippy::cast_precision_loss)]
            PyNum::Float(total as f64 / n as f64)
        };
        let with_errors = self.s.iter().filter(|(_, s)| s.errs != 0).count();

        let mut map = Map::new();
        map.insert("count".into(), jz(n));
        map.insert("average_duration_seconds".into(), avg_duration.to_json());
        map.insert("average_messages".into(), avg_messages.to_json());
        map.insert("sessions_with_errors".into(), jz(with_errors));
        Value::Object(map)
    }
}

/// CPython's `sum()` over floats — **not** a plain left-to-right `+=`.
///
/// Since 3.12 (`gh-100425`) `builtins.sum` runs the improved Kahan–Babuška
/// (Neumaier) compensated summation on its float fast path, and returns
/// `f_result + c`. A plain accumulator differs from it by an ULP or two on any
/// list long enough to lose bits — which on the maintainer's store is every
/// project past about five thousand messages. This is transcribed from
/// `Python/bltinmodule.c::builtin_sum_impl` rather than approximated, because
/// "close enough" is not what the gate measures.
///
/// Where Python writes an explicit `x += y` loop instead of `sum()` — the
/// per-session cost, the per-day cost, the retry-cost estimate, the cache
/// saving — the accumulation is plain and the callers here keep it plain. The
/// two are not interchangeable and Python uses both, four lines apart.
#[derive(Debug, Clone, Copy, Default)]
pub struct Neumaier {
    total: f64,
    compensation: f64,
    seen: bool,
}

impl Neumaier {
    /// One `sum()` step.
    pub fn add(&mut self, x: f64) {
        let t = self.total + x;
        if self.total.abs() >= x.abs() {
            self.compensation += (self.total - t) + x;
        } else {
            self.compensation += (x - t) + self.total;
        }
        self.total = t;
        self.seen = true;
    }

    /// `f_result + c`.
    #[must_use]
    pub fn finish(self) -> f64 {
        self.total + self.compensation
    }

    /// The value `sum()` returns: the `int` `0` of the `start` argument when
    /// the iterable was empty, a `float` otherwise.
    #[must_use]
    pub fn finish_pynum(self) -> PyNum {
        if self.seen {
            PyNum::Float(self.finish())
        } else {
            PyNum::Int(0)
        }
    }
}

/// `sum(iterable_of_floats)` — [`Neumaier`] over an iterator, in one call.
///
/// The wave-5 dedup pass's home for the three-line kernel that had been copied
/// into `routes/pricing.rs` and `routes/commands.rs` file-locally, each with a
/// comment asserting `Neumaier` was unreachable from `stax-server`. It was
/// reachable; the copies are gone and this is what they call.
///
/// Callers that need `sum()`'s int-vs-float result (the `int 0` an empty
/// iterable returns) want [`Neumaier::finish_pynum`], not this.
#[must_use]
pub fn neumaier_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut acc = Neumaier::default();
    for v in values {
        acc.add(v);
    }
    acc.finish()
}

/// `sum(list_of_floats)`.
fn sum_in_order(values: &[f64]) -> f64 {
    neumaier_sum(values.iter().copied())
}

/// `(_parse_ts(t1) - _parse_ts(t0)).total_seconds()`, or `None` where Python
/// raises `ValueError` (unparseable) or `TypeError` (naive vs aware).
fn span_seconds(t0: &str, t1: &str) -> Option<f64> {
    parse_ts(t1)?.sub_total_seconds(parse_ts(t0)?)
}

#[derive(Default)]
struct ErrorsCollector {
    cats: OrderMap<i64>,
    details: Vec<Value>,
    by_kind: OrderMap<i64>,
    total: i64,
}

impl ErrorsCollector {
    fn ingest(&mut self, r: &Record) {
        if !r.is_error {
            return;
        }
        self.total += 1;
        self.by_kind.incr(&r.kind, 1);
        if !r.timestamp.is_empty() {
            let mut d = Map::new();
            d.insert("timestamp".into(), Value::String(r.timestamp.clone()));
            d.insert("session_id".into(), Value::String(r.session_id.clone()));
            d.insert("model".into(), r.model.clone());
            self.details.push(Value::Object(d));
        }
        match &r.error_category {
            Some(cat) if !cat.is_empty() => self.cats.incr(cat, 1),
            // `if cat:` — a `None` category *and* an empty-string one both land
            // in "Other".
            _ => self.cats.incr("Other", 1),
        }
    }

    fn result(&self, all_records: &[Record]) -> Value {
        let mut asst_details: Vec<Value> = Vec::new();
        for (i, r) in all_records.iter().enumerate() {
            if r.kind == "assistant" && !r.timestamp.is_empty() {
                let nxt_err = all_records.get(i + 1).is_some_and(|n| n.is_error);
                let mut d = Map::new();
                d.insert("timestamp".into(), Value::String(r.timestamp.clone()));
                d.insert("is_error".into(), Value::Bool(nxt_err));
                asst_details.push(Value::Object(d));
            }
        }
        let rate = if all_records.is_empty() {
            PyNum::Int(0)
        } else {
            #[allow(clippy::cast_precision_loss)]
            PyNum::Float(self.total as f64 / all_records.len() as f64)
        };
        let mut map = Map::new();
        map.insert("total".into(), ji(self.total));
        map.insert("rate".into(), rate.to_json());
        map.insert("by_type".into(), self.by_kind.to_json());
        map.insert("by_category".into(), self.cats.to_json());
        map.insert("error_details".into(), Value::Array(self.details.clone()));
        map.insert("assistant_details".into(), Value::Array(asst_details));
        Value::Object(map)
    }
}

// ── analytics expansion collectors ──────────────────────────────────────────

#[derive(Default)]
struct SessionCost {
    t0: String,
    t1: String,
    msgs: i64,
    errs: i64,
    tokens: TokenBag,
    by_model: OrderMap<ModelBucket>,
    models: std::collections::BTreeSet<String>,
}

/// §1.1 — per-session cost/tokens/messages/errors, ranked desc by cost.
#[derive(Default)]
struct SessionCostCollector {
    s: OrderMap<SessionCost>,
}

impl SessionCostCollector {
    fn ingest(&mut self, r: &Record) {
        let s = self.s.entry(&r.session_id, SessionCost::default);
        s.msgs += 1;
        if r.is_error {
            s.errs += 1;
        }
        if !r.timestamp.is_empty() {
            if s.t0.is_empty() || r.timestamp < s.t0 {
                s.t0.clone_from(&r.timestamp);
            }
            if s.t1.is_empty() || r.timestamp > s.t1 {
                s.t1.clone_from(&r.timestamp);
            }
        }
        s.tokens.add(&r.tokens);
        if r.kind == "assistant" && r.model_named {
            s.models.insert(py_dict_key(&r.model));
            s.by_model
                .entry(&r.model_speed_key(), || ModelBucket::of(r))
                .tokens
                .add(&r.tokens);
        }
    }

    fn result(&self, ds: &EnrichedDataset, engine: &PricingEngine, provider: &str) -> Value {
        // `sorted(interactions, key=lambda ix: ix.start_time or "")`, stable.
        let mut order: Vec<usize> = (0..ds.interactions.len()).collect();
        order.sort_by(|&a, &b| {
            ds.interactions[a]
                .start_time
                .cmp(&ds.interactions[b].start_time)
        });
        let mut cmds_by_session: OrderMap<i64> = OrderMap::new();
        let mut first_prompt: OrderMap<String> = OrderMap::new();
        for &i in &order {
            let ix = &ds.interactions[i];
            cmds_by_session.incr(&ix.session_id, 1);
            if first_prompt.get(&ix.session_id).is_none() {
                let content = ds.records[ix.command].content.clone();
                first_prompt.entry(&ix.session_id, || content);
            }
        }

        let mut rows: Vec<(PyNum, Value)> = Vec::with_capacity(self.s.len());
        for (sid, s) in self.s.iter() {
            let mut duration = 0.0_f64;
            if !s.t0.is_empty() && !s.t1.is_empty() {
                duration = span_seconds(&s.t0, &s.t1).map_or(0.0, |d| d.max(0.0));
            }
            // `cost = 0.0` then `+=` — a float even with no priced model.
            let mut cost = 0.0_f64;
            for (_, bucket) in s.by_model.iter() {
                cost += total_cost(engine, bucket, provider);
            }
            let first = first_prompt.get(sid).cloned().unwrap_or_default();

            let mut row = Map::new();
            row.insert("session_id".into(), Value::String(sid.to_string()));
            row.insert("started_at".into(), Value::String(s.t0.clone()));
            row.insert("ended_at".into(), Value::String(s.t1.clone()));
            row.insert("duration_s".into(), jf(duration));
            row.insert("cost".into(), jf(cost));
            row.insert("tokens".into(), s.tokens.to_json());
            row.insert("messages".into(), ji(s.msgs));
            row.insert("commands".into(), ji(cmds_by_session.count(sid)));
            row.insert("errors".into(), ji(s.errs));
            row.insert(
                "first_prompt_preview".into(),
                Value::String(preview(&first, 140)),
            );
            row.insert(
                "models_used".into(),
                Value::Array(
                    s.models
                        .iter()
                        .map(|m| Value::String(m.clone()))
                        .collect::<Vec<_>>(),
                ),
            );
            rows.push((PyNum::Float(cost), Value::Object(row)));
        }
        sort_desc_by_num(&mut rows);
        Value::Array(rows.into_iter().map(|(_, v)| v).collect())
    }
}

/// `sorted(rows, key=…, reverse=True)` — stable, so equal keys keep their
/// original relative order exactly as CPython's reverse-stable sort does.
fn sort_desc_by_num(rows: &mut [(PyNum, Value)]) {
    rows.sort_by(|a, b| {
        b.0.as_f64()
            .partial_cmp(&a.0.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// §1.2 — one entry per real user prompt, top 50 desc by cost.
#[derive(Default)]
struct CommandCostCollector {
    items: Vec<(PyNum, Value)>,
}

impl CommandCostCollector {
    fn ingest(
        &mut self,
        ix: &Interaction,
        ds: &EnrichedDataset,
        engine: &PricingEngine,
        provider: &str,
    ) {
        let mut tokens = TokenBag::default();
        let mut by_model: OrderMap<ModelBucket> = OrderMap::new();
        let mut had_error = false;
        let mut models_used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for r in chain_records(ix, ds) {
            if r.is_error {
                had_error = true;
            }
            tokens.add(&r.tokens);
            if r.kind == "assistant" && r.model_named {
                models_used.insert(py_dict_key(&r.model));
                by_model
                    .entry(&r.model_speed_key(), || ModelBucket::of(r))
                    .tokens
                    .add(&r.tokens);
            }
        }
        // `sum(...)` — an `int` 0 when nothing was priced.
        let cost = sum_bucket_costs(engine, &by_model, provider);

        let mut row = Map::new();
        row.insert(
            "interaction_id".into(),
            Value::String(ix.interaction_id.clone()),
        );
        row.insert("session_id".into(), Value::String(ix.session_id.clone()));
        row.insert("timestamp".into(), Value::String(ix.start_time.clone()));
        row.insert(
            "prompt_preview".into(),
            Value::String(preview(&ds.records[ix.command].content, 200)),
        );
        row.insert("cost".into(), cost.to_json());
        row.insert("tokens".into(), tokens.to_json());
        row.insert("tools_used".into(), jz(ix.tool_count));
        row.insert("steps".into(), jz(ix.assistant_steps));
        row.insert(
            "models_used".into(),
            Value::Array(
                models_used
                    .iter()
                    .map(|m| Value::String(m.clone()))
                    .collect::<Vec<_>>(),
            ),
        );
        row.insert("had_error".into(), Value::Bool(had_error));
        self.items.push((cost, Value::Object(row)));
    }

    fn result(&self) -> Value {
        let mut items = self.items.clone();
        sort_desc_by_num(&mut items);
        items.truncate(50);
        Value::Array(items.into_iter().map(|(_, v)| v).collect())
    }
}

/// `ix.responses + ix.tool_results` — list concatenation, in that order.
fn chain_records<'a>(
    ix: &'a Interaction,
    ds: &'a EnrichedDataset,
) -> impl Iterator<Item = &'a Record> {
    ix.responses
        .iter()
        .chain(ix.tool_results.iter())
        .map(move |&i| &ds.records[i])
}

#[derive(Default)]
struct ToolCost {
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    cost: f64,
}

/// §1.3 — per-tool cost with 1/N attribution across distinct tools per message.
#[derive(Default)]
struct ToolCostCollector {
    data: OrderMap<ToolCost>,
}

impl ToolCostCollector {
    fn ingest(&mut self, r: &Record, engine: &PricingEngine, provider: &str) {
        if r.kind != "assistant" || r.tools.is_empty() {
            return;
        }
        let mut name_counts: OrderMap<i64> = OrderMap::new();
        for t in &r.tools {
            name_counts.incr(&t.name_key(), 1);
        }
        if name_counts.is_empty() {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let share = 1.0 / name_counts.len() as f64;
        let mut msg_cost = 0.0_f64;
        if r.model_named {
            msg_cost = engine
                .compute_cost(
                    &r.tokens.raw(),
                    &py_dict_key(&r.model),
                    provider,
                    &r.speed,
                    None,
                )
                .total_cost;
        }
        let names: Vec<(String, i64)> = name_counts
            .iter()
            .map(|(n, c)| (n.to_string(), *c))
            .collect();
        for (name, count) in names {
            let d = self.data.entry(&name, ToolCost::default);
            d.calls += count;
            d.input_tokens += r.tokens.input;
            d.output_tokens += r.tokens.output;
            d.cache_read_tokens += r.tokens.cache_read;
            d.cache_creation_tokens += r.tokens.cache_creation;
            d.cost += msg_cost * share;
        }
    }

    fn result(&self) -> Value {
        let mut out = Map::new();
        for (name, d) in self.data.iter() {
            let mut entry = Map::new();
            entry.insert("calls".into(), ji(d.calls));
            entry.insert("input_tokens".into(), ji(d.input_tokens));
            entry.insert("output_tokens".into(), ji(d.output_tokens));
            entry.insert("cache_read_tokens".into(), ji(d.cache_read_tokens));
            entry.insert("cache_creation_tokens".into(), ji(d.cache_creation_tokens));
            entry.insert("cost".into(), jf(d.cost));
            out.insert(name.to_string(), Value::Object(entry));
        }
        Value::Object(out)
    }
}

/// §1.4 — token totals per day, globally, and per session.
struct TokenCompositionCollector {
    tz: i64,
    daily: OrderMap<TokenBag>,
    totals: TokenBag,
    per_session: OrderMap<TokenBag>,
}

impl TokenCompositionCollector {
    fn new(tz: i64) -> Self {
        Self {
            tz,
            daily: OrderMap::new(),
            totals: TokenBag::default(),
            per_session: OrderMap::new(),
        }
    }

    fn ingest(&mut self, r: &Record) {
        // `if not r.tokens: return` — `_usage_from` always returns four keys, so
        // this never fires for a record built by the enricher.
        if !r.tokens.touched {
            return;
        }
        self.totals.add(&r.tokens);
        self.per_session
            .entry(&r.session_id, TokenBag::default)
            .add(&r.tokens);
        if let Some(day) = local_day(&r.timestamp, self.tz) {
            self.daily.entry(&day, TokenBag::default).add(&r.tokens);
        }
    }

    fn result(&self) -> Value {
        let bags = |m: &OrderMap<TokenBag>| {
            let mut out = Map::new();
            for (k, v) in m.iter() {
                out.insert(k.to_string(), v.to_json());
            }
            Value::Object(out)
        };
        let mut map = Map::new();
        map.insert("daily".into(), bags(&self.daily));
        map.insert("totals".into(), self.totals.to_json());
        map.insert("per_session".into(), bags(&self.per_session));
        let reasoning = self.totals.reasoning.unwrap_or(0);
        if reasoning > 0 {
            let output = self.totals.output;
            let share = if output > 0 {
                #[allow(clippy::cast_precision_loss)]
                {
                    reasoning as f64 / output as f64
                }
            } else {
                0.0
            };
            map.insert("reasoning_share".into(), jf(share));
        }
        Value::Object(map)
    }
}

/// §1.5 — interactions with abnormally high tool/step counts.
#[derive(Default)]
struct OutlierCollector {
    high_tool: Vec<(PyNum, Value)>,
    high_step: Vec<(PyNum, Value)>,
}

impl OutlierCollector {
    fn ingest(
        &mut self,
        ix: &Interaction,
        ds: &EnrichedDataset,
        engine: &PricingEngine,
        provider: &str,
    ) {
        let tc = ix.tool_count;
        let steps = ix.assistant_steps;
        if tc <= 20 && steps <= 15 {
            return;
        }
        let entry = interaction_to_outlier_command(ix, ds, engine, provider);
        if tc > 20 {
            self.high_tool.push((jz_num(tc), entry.clone()));
        }
        if steps > 15 {
            self.high_step.push((jz_num(steps), entry));
        }
    }

    fn result(&self) -> Value {
        let mut high_tool = self.high_tool.clone();
        let mut high_step = self.high_step.clone();
        sort_desc_by_num(&mut high_tool);
        sort_desc_by_num(&mut high_step);
        let mut map = Map::new();
        map.insert(
            "high_tool_commands".into(),
            Value::Array(high_tool.into_iter().map(|(_, v)| v).collect()),
        );
        map.insert(
            "high_step_commands".into(),
            Value::Array(high_step.into_iter().map(|(_, v)| v).collect()),
        );
        Value::Object(map)
    }
}

#[allow(clippy::cast_possible_wrap)]
fn jz_num(value: usize) -> PyNum {
    PyNum::Int(value as i64)
}

/// `aggregator._interaction_to_outlier_command`.
fn interaction_to_outlier_command(
    ix: &Interaction,
    ds: &EnrichedDataset,
    engine: &PricingEngine,
    provider: &str,
) -> Value {
    let mut by_model: OrderMap<ModelBucket> = OrderMap::new();
    for r in chain_records(ix, ds) {
        if r.kind == "assistant" && r.model_named {
            by_model
                .entry(&r.model_speed_key(), || ModelBucket::of(r))
                .tokens
                .add(&r.tokens);
        }
    }
    let cost = sum_bucket_costs(engine, &by_model, provider);
    let mut row = Map::new();
    row.insert(
        "interaction_id".into(),
        Value::String(ix.interaction_id.clone()),
    );
    row.insert("session_id".into(), Value::String(ix.session_id.clone()));
    row.insert("timestamp".into(), Value::String(ix.start_time.clone()));
    row.insert(
        "prompt_preview".into(),
        Value::String(preview(&ds.records[ix.command].content, 200)),
    );
    row.insert("tool_count".into(), jz(ix.tool_count));
    row.insert("step_count".into(), jz(ix.assistant_steps));
    row.insert("cost".into(), cost.to_json());
    Value::Object(row)
}

/// §1.6 / polish §A1 — retry signals inside an Interaction.
#[derive(Default)]
struct RetryCollector {
    items: Vec<Value>,
}

impl RetryCollector {
    fn ingest(
        &mut self,
        ix: &Interaction,
        ds: &EnrichedDataset,
        engine: &PricingEngine,
        provider: &str,
    ) {
        // `sorted(list(responses) + list(tool_results), key=lambda r: r.timestamp or "")`
        let mut events: Vec<&Record> = chain_records(ix, ds).collect();
        events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        if events.is_empty() {
            return;
        }

        let mut per_tool_flags: OrderMap<Vec<bool>> = OrderMap::new();
        let mut per_tool_wasted: OrderMap<OrderMap<i64>> = OrderMap::new();

        for i in 0..events.len() {
            let r = events[i];
            if r.kind != "assistant" || r.tools.is_empty() {
                continue;
            }
            let failed = next_record_signals_error(&events, i);
            let out_tok = r.tokens.output;
            for t in &r.tools {
                let name = t.name_key();
                per_tool_flags.entry(&name, Vec::new).push(failed);
                if failed {
                    per_tool_wasted
                        .entry(&name, OrderMap::new)
                        .incr(&r.speed, out_tok);
                }
            }
        }

        for (name, flags) in per_tool_flags.iter() {
            if flags.len() < 2 {
                continue;
            }
            // "at least one *preceding* invocation was followed by an error".
            if !flags[..flags.len() - 1].iter().any(|f| *f) {
                continue;
            }
            let mut run = 0_i64;
            let mut max_run = 0_i64;
            for f in flags {
                if *f {
                    run += 1;
                    if run > max_run {
                        max_run = run;
                    }
                } else {
                    run = 0;
                }
            }
            let empty = OrderMap::new();
            let wasted = per_tool_wasted.get(name).unwrap_or(&empty);
            let wt: i64 = wasted.iter().map(|(_, v)| *v).sum();
            let mut wc = 0.0_f64;
            if model_named(&ix.model) && wt != 0 {
                for (speed, tokens) in wasted.iter() {
                    if *tokens == 0 {
                        continue;
                    }
                    wc += output_only_cost(engine, *tokens, &ix.model, provider, speed);
                }
            }
            let mut row = Map::new();
            row.insert(
                "interaction_id".into(),
                Value::String(ix.interaction_id.clone()),
            );
            row.insert("session_id".into(), Value::String(ix.session_id.clone()));
            row.insert("timestamp".into(), Value::String(ix.start_time.clone()));
            row.insert("tool".into(), Value::String(name.to_string()));
            row.insert("consecutive_failures".into(), ji(max_run));
            row.insert("total_invocations".into(), jz(flags.len()));
            row.insert("estimated_wasted_tokens".into(), ji(wt));
            row.insert("estimated_wasted_cost".into(), jf(wc));
            self.items.push(Value::Object(row));
        }
    }

    fn result(&self) -> Value {
        Value::Array(self.items.clone())
    }
}

/// `aggregator._next_record_signals_error`.
fn next_record_signals_error(events: &[&Record], idx: usize) -> bool {
    for r in &events[idx + 1..] {
        if r.kind == "assistant" {
            return r.content.starts_with(INTERRUPT_API) || r.content.starts_with(INTERRUPT_PREFIX);
        }
        if r.is_error {
            return true;
        }
        let stripped = super::pytext::py_lstrip(&r.content);
        if stripped.starts_with("Error") || stripped.starts_with("failed") {
            return true;
        }
    }
    false
}

/// `aggregator._is_search_tool_name`.
fn is_search_tool_name(name: &str) -> bool {
    name == "Grep" || name == "Glob" || name.to_lowercase().contains("search")
}

/// §1.7 — per-session tool-mix ratios, idle gaps, classification.
#[derive(Default)]
struct SessionEfficiency {
    timestamps: Vec<String>,
    tools: OrderMap<i64>,
}

#[derive(Default)]
struct SessionEfficiencyCollector {
    s: OrderMap<SessionEfficiency>,
}

impl SessionEfficiencyCollector {
    const IDLE_THRESHOLD_S: f64 = 30.0;
    const IDLE_CLASS_RATIO: f64 = 0.4;
    const EDIT_HEAVY_MIN: f64 = 0.25;
    const RESEARCH_SUM_MIN: f64 = 0.6;
    const RESEARCH_EDIT_MAX: f64 = 0.1;

    fn ingest(&mut self, r: &Record) {
        let s = self.s.entry(&r.session_id, SessionEfficiency::default);
        if !r.timestamp.is_empty() {
            s.timestamps.push(r.timestamp.clone());
        }
        for t in &r.tools {
            s.tools.incr(&t.name_key(), 1);
        }
    }

    fn result(&self) -> Value {
        let mut out: Vec<Value> = Vec::with_capacity(self.s.len());
        for (sid, s) in self.s.iter() {
            let total: i64 = s.tools.iter().map(|(_, c)| *c).sum();
            let search: i64 = s
                .tools
                .iter()
                .filter(|(n, _)| is_search_tool_name(n))
                .map(|(_, c)| *c)
                .sum();
            let edit = s.tools.count("Edit") + s.tools.count("Write");
            let read = s.tools.count("Read");
            let bash = s.tools.count("Bash");
            #[allow(clippy::cast_precision_loss)]
            let ratio = |n: i64| {
                if total == 0 {
                    0.0
                } else {
                    n as f64 / total as f64
                }
            };
            let (sr, er, rr, br) = (ratio(search), ratio(edit), ratio(read), ratio(bash));

            let mut times: Vec<&String> = s.timestamps.iter().filter(|t| !t.is_empty()).collect();
            times.sort();
            let mut total_idle = 0.0_f64;
            let mut max_idle = 0.0_f64;
            let mut duration_s = 0.0_f64;
            if let (Some(first), Some(last)) = (times.first(), times.last()) {
                duration_s = span_seconds(first, last).map_or(0.0, |d| d.max(0.0));
            }
            for pair in times.windows(2) {
                let Some(gap) = span_seconds(pair[0], pair[1]) else {
                    continue;
                };
                if gap >= Self::IDLE_THRESHOLD_S {
                    total_idle += gap;
                    if gap > max_idle {
                        max_idle = gap;
                    }
                }
            }

            let classification = if er >= Self::EDIT_HEAVY_MIN {
                "edit-heavy"
            } else if sr + rr >= Self::RESEARCH_SUM_MIN && er < Self::RESEARCH_EDIT_MAX {
                "research-heavy"
            } else if duration_s > 0.0 && total_idle > duration_s * Self::IDLE_CLASS_RATIO {
                "idle-heavy"
            } else {
                "balanced"
            };

            let mut row = Map::new();
            row.insert("session_id".into(), Value::String(sid.to_string()));
            row.insert("search_ratio".into(), jf(sr));
            row.insert("edit_ratio".into(), jf(er));
            row.insert("read_ratio".into(), jf(rr));
            row.insert("bash_ratio".into(), jf(br));
            row.insert("idle_gap_total_s".into(), jf(total_idle));
            row.insert("idle_gap_max_s".into(), jf(max_idle));
            row.insert(
                "classification".into(),
                Value::String(classification.to_string()),
            );
            out.push(Value::Object(row));
        }
        Value::Array(out)
    }
}

/// §1.8 — total errors, retry-cost estimate, errors-by-tool, top interactions.
#[derive(Default)]
struct ErrorCostCollector {
    total_errors: i64,
    tool_id_to_name: std::collections::HashMap<String, String>,
}

impl ErrorCostCollector {
    fn ingest(&mut self, r: &Record) {
        if r.kind == "assistant" {
            for t in &r.tools {
                if let Some(block) = &t.block
                    && py_truthy(&block.id)
                    && py_truthy(&block.name)
                {
                    self.tool_id_to_name
                        .insert(py_dict_key(&block.id), py_dict_key(&block.name));
                }
            }
        }
        if r.is_error {
            self.total_errors += 1;
        }
    }

    fn result(&self, ds: &EnrichedDataset, engine: &PricingEngine, provider: &str) -> Value {
        let mut errors_by_tool: OrderMap<i64> = OrderMap::new();
        let mut est_retry_tokens = 0_i64;
        let mut est_retry_cost = 0.0_f64;
        let mut ranked: Vec<(i64, usize)> = Vec::new();

        for (ix_idx, ix) in ds.interactions.iter().enumerate() {
            let mut timeline: Vec<&Record> = chain_records(ix, ds).collect();
            timeline.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            let mut err_count = 0_i64;
            for idx in 0..timeline.len() {
                let rec = timeline[idx];
                if !rec.is_error {
                    continue;
                }
                err_count += 1;
                for name in self.tool_names_for_error(rec) {
                    errors_by_tool.incr(&name, 1);
                }
                // Fallback for error records carrying their own tool_use blocks.
                if rec.kind != "user" {
                    for t in &rec.tools {
                        if let Some(nm) = t.name_if_truthy() {
                            errors_by_tool.incr(&nm, 1);
                        }
                    }
                }
                let (tokens, model, speed) = retry_tokens_and_model(rec, &timeline, idx);
                if tokens != 0 && model_named(&model) {
                    est_retry_tokens += tokens;
                    est_retry_cost += output_only_cost(engine, tokens, &model, provider, &speed);
                }
            }
            if err_count > 0 {
                ranked.push((err_count, ix_idx));
            }
        }

        ranked.sort_by_key(|pair| std::cmp::Reverse(pair.0));
        let top: Vec<Value> = ranked
            .iter()
            .take(10)
            .map(|&(_, i)| {
                interaction_to_outlier_command(&ds.interactions[i], ds, engine, provider)
            })
            .collect();

        let mut map = Map::new();
        map.insert("total_errors".into(), ji(self.total_errors));
        map.insert("estimated_retry_tokens".into(), ji(est_retry_tokens));
        map.insert("estimated_retry_cost".into(), jf(est_retry_cost));
        map.insert("errors_by_tool".into(), errors_by_tool.to_json());
        map.insert("top_error_commands".into(), Value::Array(top));
        Value::Object(map)
    }

    /// `_ErrorCostCollector._tool_names_for_error`.
    fn tool_names_for_error(&self, r: &Record) -> Vec<String> {
        let Some(content) = r
            .raw_data
            .get("message")
            .filter(|m| m.is_object())
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if !b.get("is_error").is_some_and(py_truthy) {
                continue;
            }
            if let Some(tid) = b.get("tool_use_id")
                && py_truthy(tid)
                && let Some(name) = self.tool_id_to_name.get(&py_dict_key(tid))
            {
                names.push(name.clone());
            }
        }
        names
    }
}

/// `_ErrorCostCollector._retry_tokens_and_model`.
fn retry_tokens_and_model(
    error_rec: &Record,
    timeline: &[&Record],
    idx: usize,
) -> (i64, Value, String) {
    if error_rec.kind == "assistant" {
        let out = error_rec.tokens.output;
        if out != 0 && error_rec.model_named {
            return (out, error_rec.model.clone(), error_rec.speed.clone());
        }
    }
    for cand in &timeline[idx + 1..] {
        if cand.kind == "assistant" && cand.model_named {
            return (cand.tokens.output, cand.model.clone(), cand.speed.clone());
        }
    }
    (0, Value::String(String::new()), "standard".to_string())
}

// ── cache ───────────────────────────────────────────────────────────────────

/// Frontend `CacheRoiCard` divides by 1e6 to get dollars; we keep that wire
/// contract and emit the REAL USD saving × 1e6.
const CACHE_COST_BASE_UNIT_SCALE: f64 = 1_000_000.0;

#[derive(Default)]
struct CacheByModel {
    model: Value,
    speed: String,
    read: i64,
    created: i64,
}

#[derive(Default)]
struct CacheCollector {
    created: i64,
    read: i64,
    w_created: i64,
    w_read: i64,
    asst: i64,
    cache_by_ms: OrderMap<CacheByModel>,
}

impl CacheCollector {
    fn ingest(&mut self, r: &Record) {
        if r.kind != "assistant" {
            return;
        }
        self.asst += 1;
        let cc = r.tokens.cache_creation;
        let cr = r.tokens.cache_read;
        if cc != 0 {
            self.w_created += 1;
            self.created += cc;
        }
        if cr != 0 {
            self.w_read += 1;
            self.read += cr;
        }
        if (cc != 0 || cr != 0) && r.model_named {
            let b = self
                .cache_by_ms
                .entry(&r.model_speed_key(), || CacheByModel {
                    model: r.model.clone(),
                    speed: r.speed.clone(),
                    read: 0,
                    created: 0,
                });
            b.read += cr;
            b.created += cc;
        }
    }

    fn result(&self, engine: &PricingEngine, provider: &str) -> Value {
        #[allow(clippy::cast_precision_loss)]
        let hr = if self.asst == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(self.w_read as f64 / self.asst as f64 * 100.0)
        };
        #[allow(clippy::cast_precision_loss)]
        let eff = if self.created == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(self.read as f64 / self.created as f64 * 100.0)
        };
        #[allow(clippy::cast_precision_loss)]
        let roi = if self.created == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float((self.read as f64 / self.created as f64 - 1.0) * 100.0)
        };
        let saved = self.read - self.created;
        let cost_saved = self.cost_saved_base_units(engine, provider);

        let mut map = Map::new();
        map.insert("total_created".into(), ji(self.created));
        map.insert("total_read".into(), ji(self.read));
        map.insert("messages_with_cache_read".into(), ji(self.w_read));
        map.insert("messages_with_cache_created".into(), ji(self.w_created));
        map.insert("assistant_messages".into(), ji(self.asst));
        map.insert("hit_rate".into(), round_pynum(hr, 1).to_json());
        map.insert("efficiency".into(), round_pynum(min_100(eff), 1).to_json());
        map.insert("tokens_saved".into(), ji(saved));
        map.insert("cost_saved_base_units".into(), jf(cost_saved));
        map.insert(
            "break_even_achieved".into(),
            Value::Bool(self.read > self.created),
        );
        map.insert("cache_roi".into(), round_pynum(roi, 1).to_json());
        Value::Object(map)
    }

    /// `aggregator.cache_cost_saved_base_units`, over this collector's buckets.
    fn cost_saved_base_units(&self, engine: &PricingEngine, provider: &str) -> f64 {
        let mut total_usd = 0.0_f64;
        for (_, b) in self.cache_by_ms.iter() {
            if (b.read == 0 && b.created == 0) || !model_named(&b.model) {
                continue;
            }
            let cb = engine.compute_cost(
                &RawTokens::canonical(b.read + b.created, 0, b.created, b.read),
                &py_dict_key(&b.model),
                // `provider or "anthropic"` — the collector always passes the
                // dataset provider, which is never empty.
                if provider.is_empty() {
                    "anthropic"
                } else {
                    provider
                },
                if b.speed.is_empty() {
                    "standard"
                } else {
                    &b.speed
                },
                None,
            );
            total_usd += cb.input_cost - cb.cache_read_cost - cb.cache_creation_cost;
        }
        round_py(total_usd * CACHE_COST_BASE_UNIT_SCALE, 2)
    }
}

/// `round(x, n)` where `x` may be a Python `int` — `round(0, 1)` is the `int`
/// `0`, and that reaches `cache.hit_rate` as `0` rather than `0.0`.
fn round_pynum(value: PyNum, ndigits: usize) -> PyNum {
    match value {
        PyNum::Int(i) => PyNum::Int(i),
        PyNum::Float(f) => PyNum::Float(round_py(f, ndigits)),
    }
}

// ── time-bucketed stats ─────────────────────────────────────────────────────

#[derive(Default)]
struct DayBucket {
    msgs: i64,
    tokens: TokenBag,
    session_ids: std::collections::HashSet<String>,
    model_tokens: OrderMap<ModelBucket>,
    user_cmds: i64,
    int_cmds: i64,
    errs: i64,
    asst: i64,
}

fn daily(records: &[Record], tz_offset: i64, engine: &PricingEngine, provider: &str) -> Value {
    let mut buckets: OrderMap<DayBucket> = OrderMap::new();

    for r in records {
        let Some(day) = local_day(&r.timestamp, tz_offset) else {
            continue;
        };
        let b = buckets.entry(&day, DayBucket::default);
        b.msgs += 1;
        b.session_ids.insert(r.session_id.clone());
        if r.is_error {
            b.errs += 1;
        }
        if r.kind == "assistant" {
            b.asst += 1;
            if r.model_named {
                b.model_tokens
                    .entry(&r.model_speed_key(), || ModelBucket::of(r))
                    .tokens
                    .add(&r.tokens);
            }
        }
        b.tokens.add(&r.tokens);
    }

    // interruption tracking via sorted scan
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].timestamp.cmp(&records[b].timestamp));
    let ordered: Vec<&Record> = order.into_iter().map(|i| &records[i]).collect();
    for (i, r) in ordered.iter().enumerate() {
        if r.kind != "user" || r.has_tool_result || r.timestamp.is_empty() {
            continue;
        }
        if is_interrupt_text(&r.content) {
            continue;
        }
        let Some(day) = local_day(&r.timestamp, tz_offset) else {
            continue;
        };
        let b = buckets.entry(&day, DayBucket::default);
        b.user_cmds += 1;
        if next_is_interrupt(&ordered, i) {
            b.int_cmds += 1;
        }
    }

    let mut out = Map::new();
    for (day, b) in buckets.iter() {
        // Two-stage merge: price each (model, speed) bucket separately, adding
        // its total to `day_cost` as it goes (a plain running `+=`, not a
        // `sum()`), then collapse by model NAME for the public `by_model`
        // payload. One pass, because Python does both in one loop and the
        // pricing call is the expensive part.
        let (day_cost, model_costs) = merge_model_costs(&b.model_tokens, engine, provider);

        #[allow(clippy::cast_precision_loss)]
        let ir = if b.user_cmds == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(b.int_cmds as f64 / b.user_cmds as f64 * 100.0)
        };
        #[allow(clippy::cast_precision_loss)]
        let er = if b.asst == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(b.errs as f64 / b.asst as f64 * 100.0)
        };

        let mut cost = Map::new();
        cost.insert("total".into(), jf(day_cost));
        cost.insert("by_model".into(), model_costs);

        let mut entry = Map::new();
        entry.insert("messages".into(), ji(b.msgs));
        entry.insert("sessions".into(), jz(b.session_ids.len()));
        entry.insert("tokens".into(), b.tokens.to_json());
        entry.insert("cost".into(), Value::Object(cost));
        entry.insert("user_commands".into(), ji(b.user_cmds));
        entry.insert("interrupted_commands".into(), ji(b.int_cmds));
        entry.insert("interruption_rate".into(), round_pynum(ir, 1).to_json());
        entry.insert("errors".into(), ji(b.errs));
        entry.insert("assistant_messages".into(), ji(b.asst));
        entry.insert("error_rate".into(), round_pynum(er, 1).to_json());
        out.insert(day.to_string(), Value::Object(entry));
    }
    Value::Object(out)
}

/// The `by_model` roll-up: price each `(model, speed)` bucket, then sum the
/// five cost fields per model NAME, in first-appearance order.
///
/// Returns `(day_cost, by_model)` — `day_cost` is Python's `day_cost +=
/// cb["total_cost"]`, accumulated in the same loop and in the same order.
fn merge_model_costs(
    model_tokens: &OrderMap<ModelBucket>,
    engine: &PricingEngine,
    provider: &str,
) -> (f64, Value) {
    let mut day_cost = 0.0_f64;
    let mut order: Vec<String> = Vec::new();
    let mut acc: std::collections::HashMap<String, [f64; 5]> = std::collections::HashMap::new();
    for (_, bucket) in model_tokens.iter() {
        let cb = engine.compute_cost(
            &bucket.tokens.raw(),
            &py_dict_key(&bucket.model),
            provider,
            &bucket.speed,
            None,
        );
        day_cost += cb.total_cost;
        let key = py_dict_key(&bucket.model);
        let values = [
            cb.input_cost,
            cb.output_cost,
            cb.cache_creation_cost,
            cb.cache_read_cost,
            cb.total_cost,
        ];
        match acc.get_mut(&key) {
            None => {
                order.push(key.clone());
                acc.insert(key, values);
            }
            Some(slot) => {
                for (s, v) in slot.iter_mut().zip(values) {
                    *s += v;
                }
            }
        }
    }
    let mut out = Map::new();
    for key in order {
        let values = acc.remove(&key).unwrap_or([0.0; 5]);
        let mut entry = Map::new();
        for (name, value) in [
            "input_cost",
            "output_cost",
            "cache_creation_cost",
            "cache_read_cost",
            "total_cost",
        ]
        .iter()
        .zip(values)
        {
            entry.insert((*name).to_string(), jf(value));
        }
        out.insert(key, Value::Object(entry));
    }
    (day_cost, Value::Object(out))
}

fn hourly(records: &[Record], tz_offset: i64) -> Value {
    let mut msg_h: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut tok_h: std::collections::HashMap<i64, TokenBag> = std::collections::HashMap::new();
    for r in records {
        let Some(h) = local_hour(&r.timestamp, tz_offset) else {
            continue;
        };
        *msg_h.entry(h).or_insert(0) += 1;
        tok_h.entry(h).or_default().add(&r.tokens);
    }
    let mut messages = Map::new();
    let mut tokens = Map::new();
    for h in 0..24_i64 {
        messages.insert(h.to_string(), ji(msg_h.get(&h).copied().unwrap_or(0)));
        // `defaultdict(Counter)` — `h in tok_h` is only true for hours a record
        // actually landed in, and the `else` branch is a literal four-key dict
        // with NO `reasoning` key even on a reasoning project.
        let value = match tok_h.get(&h) {
            Some(bag) => bag.to_json(),
            None => {
                let mut zero = Map::new();
                zero.insert("input".into(), ji(0));
                zero.insert("output".into(), ji(0));
                zero.insert("cache_creation".into(), ji(0));
                zero.insert("cache_read".into(), ji(0));
                Value::Object(zero)
            }
        };
        tokens.insert(h.to_string(), value);
    }
    let mut map = Map::new();
    map.insert("messages".into(), Value::Object(messages));
    map.insert("tokens".into(), Value::Object(tokens));
    Value::Object(map)
}

// ── command analysis ────────────────────────────────────────────────────────

/// `aggregator._FILE_SEARCH_TOOLS`.
const FILE_SEARCH_TOOLS: [&str; 3] = ["Grep", "Glob", "LS"];

/// `aggregator._SEARCH_VERBS` — the first token of a pipe segment.
const SEARCH_VERBS: [&str; 9] = [
    "grep", "rg", "find", "fd", "locate", "which", "whereis", "ls", "ag", // "ack" below
];

/// `_SEARCH_VERBS` has ten entries; the array above holds nine so the literal
/// stays readable. Kept separate rather than reformatted, because a silently
/// dropped verb changes `search_tool_percentage` on every project.
const SEARCH_VERB_ACK: &str = "ack";

fn is_search_verb(token: &str) -> bool {
    SEARCH_VERBS.contains(&token) || token == SEARCH_VERB_ACK
}

/// `aggregator._cmd_has_search_verb`.
fn cmd_has_search_verb(cmd: &str) -> bool {
    let lowered = cmd.to_lowercase();
    for segment in lowered.split('|') {
        let segment = py_strip(segment);
        if segment.is_empty() {
            continue;
        }
        for sub in segment.replace("&&", ";").split(';') {
            if let Some(first) = py_strip(sub)
                .split(super::pytext::is_py_space)
                .find(|t| !t.is_empty())
                && is_search_verb(first)
            {
                return true;
            }
        }
    }
    false
}

/// `aggregator._is_search_invocation`.
fn is_search_invocation(tool: &ToolRef) -> bool {
    let Some(block) = &tool.block else {
        return false;
    };
    let name = py_dict_key(&block.name);
    if FILE_SEARCH_TOOLS.contains(&name.as_str()) {
        return true;
    }
    if name != "Bash" {
        return false;
    }
    // DIV-062 / DIV-063: Python calls `.get(...)` on a non-dict `input` and
    // `.lower()` on a non-str `command`, both `AttributeError`.
    let Some(input) = block.input.as_object() else {
        DIV_062.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return false;
    };
    match input.get("command") {
        None => false,
        Some(Value::String(cmd)) => !cmd.is_empty() && cmd_has_search_verb(cmd),
        Some(other) => {
            if py_truthy(other) {
                DIV_062.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            false
        }
    }
}

fn count_search_tools(tools: &[ToolRef]) -> i64 {
    tools.iter().filter(|t| is_search_invocation(t)).count() as i64
}

/// `aggregator._is_interrupt_text`.
#[must_use]
pub fn is_interrupt_text(text: &str) -> bool {
    text.starts_with(INTERRUPT_PREFIX) || text.starts_with(INTERRUPT_API)
}

/// `aggregator._next_is_interrupt`.
fn next_is_interrupt(ordered: &[&Record], idx: usize) -> bool {
    for nxt in &ordered[idx + 1..] {
        if nxt.kind == "assistant" && py_strip(&nxt.content) == INTERRUPT_API {
            return true;
        }
        if nxt.kind == "user" && !nxt.has_tool_result {
            return is_interrupt_text(&nxt.content);
        }
    }
    false
}

/// `aggregator._command_analysis` — the whole 21-key section.
fn command_analysis(ds: &EnrichedDataset) -> Value {
    let records = &ds.records;
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].timestamp.cmp(&records[b].timestamp));
    let ordered: Vec<&Record> = order.into_iter().map(|i| &records[i]).collect();

    let mut ix_lut: std::collections::HashMap<&str, &Interaction> =
        std::collections::HashMap::with_capacity(ds.interactions.len());
    for ix in &ds.interactions {
        ix_lut.insert(ix.key.as_str(), ix);
    }

    struct Detail {
        is_interruption: bool,
        followed: bool,
        tools_used: i64,
        has_tools: bool,
        model: Value,
        estimated_tokens: f64,
    }

    let mut details: Vec<Value> = Vec::new();
    let mut flat: Vec<Detail> = Vec::new();
    let mut n_cmds = 0_i64;
    let mut n_tooled = 0_i64;
    let mut total_tools = 0_i64;
    let mut total_search = 0_i64;
    let mut total_steps = 0_i64;
    let mut dist: OrderMap<i64> = OrderMap::new();

    for (i, r) in ordered.iter().enumerate() {
        if r.kind != "user" || r.has_tool_result {
            continue;
        }
        let is_int = is_interrupt_text(&r.content);
        let key = super::enricher::interaction_key(r);
        let (tc, model, steps, tnames, search_n) = match ix_lut.get(key.as_str()) {
            Some(ix) => {
                let tc = ix.tool_count;
                let slice = &ix.tools_used[..tc.min(ix.tools_used.len())];
                (
                    tc as i64,
                    ix.model.clone(),
                    ix.assistant_steps as i64,
                    slice.iter().map(|t| t.name_key()).collect::<Vec<_>>(),
                    count_search_tools(slice),
                )
            }
            None => scan_forward(&ordered, i),
        };
        let followed = next_is_interrupt(&ordered, i);
        #[allow(clippy::cast_precision_loss)]
        let est_tok = (r.content.chars().count() as f64 / 4.0).max(1.0);

        let mut d = Map::new();
        d.insert("user_message".into(), Value::String(r.content.clone()));
        let char_count = r.content.chars().count();
        d.insert(
            "user_message_truncated".into(),
            Value::String(if char_count > 100 {
                format!("{}...", py_char_prefix(&r.content, 100))
            } else {
                r.content.clone()
            }),
        );
        d.insert("timestamp".into(), Value::String(r.timestamp.clone()));
        d.insert("session_id".into(), Value::String(r.session_id.clone()));
        d.insert("tools_used".into(), ji(tc));
        d.insert(
            "tool_names".into(),
            Value::Array(tnames.into_iter().map(Value::String).collect()),
        );
        d.insert("has_tools".into(), Value::Bool(tc > 0));
        d.insert("assistant_steps".into(), ji(steps));
        d.insert("model".into(), model.clone());
        d.insert("is_interruption".into(), Value::Bool(is_int));
        d.insert("followed_by_interruption".into(), Value::Bool(followed));
        d.insert("estimated_tokens".into(), jf(est_tok));
        d.insert("search_tools_used".into(), ji(search_n));
        details.push(Value::Object(d));
        flat.push(Detail {
            is_interruption: is_int,
            followed,
            tools_used: tc,
            has_tools: tc > 0,
            model,
            estimated_tokens: est_tok,
        });

        if !is_int {
            n_cmds += 1;
            total_steps += steps;
            total_search += search_n;
            if tc != 0 {
                n_tooled += 1;
                total_tools += tc;
            }
            dist.incr(&tc.to_string(), 1);
        }
    }

    let non_int = flat.iter().filter(|d| !d.is_interruption).count() as i64;
    let int_followed = flat
        .iter()
        .filter(|d| !d.is_interruption && d.followed)
        .count() as i64;
    #[allow(clippy::cast_precision_loss)]
    let ir = if non_int == 0 {
        PyNum::Int(0)
    } else {
        PyNum::Float(int_followed as f64 / non_int as f64 * 100.0)
    };

    let mut tc_buckets: OrderMap<Vec<bool>> = OrderMap::new();
    for d in &flat {
        if !d.is_interruption {
            tc_buckets
                .entry(&d.tools_used.to_string(), Vec::new)
                .push(d.followed);
        }
    }
    let mut by_tc = Map::new();
    for (tc_val, flags) in tc_buckets.iter() {
        let n_int = flags.iter().filter(|f| **f).count() as i64;
        let rate = if flags.is_empty() {
            PyNum::Int(0)
        } else {
            #[allow(clippy::cast_precision_loss)]
            PyNum::Float(round_py(n_int as f64 / flags.len() as f64 * 100.0, 1))
        };
        let mut entry = Map::new();
        entry.insert("rate".into(), rate.to_json());
        entry.insert("total_commands".into(), jz(flags.len()));
        entry.insert("interrupted_commands".into(), ji(n_int));
        by_tc.insert(tc_val.to_string(), Value::Object(entry));
    }

    let mut mdist: OrderMap<i64> = OrderMap::new();
    for d in &flat {
        if !d.is_interruption && d.model.as_str() != Some("N/A") {
            mdist.incr(&py_dict_key(&d.model), 1);
        }
    }

    // `sum(d["estimated_tokens"] for d in details if not d["is_interruption"])`
    // — floats, so the compensated path.
    let tok_sum: f64 = {
        let mut acc = Neumaier::default();
        for d in &flat {
            if !d.is_interruption {
                acc.add(d.estimated_tokens);
            }
        }
        acc.finish()
    };
    #[allow(clippy::cast_precision_loss)]
    let ratio = |num: f64, den: i64| -> PyNum {
        if den == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(num / den as f64)
        }
    };
    #[allow(clippy::cast_precision_loss)]
    let pct_t = ratio(n_tooled as f64 * 100.0, n_cmds);
    #[allow(clippy::cast_precision_loss)]
    let avg_t = ratio(total_tools as f64, n_cmds);
    #[allow(clippy::cast_precision_loss)]
    let avg_tw = ratio(total_tools as f64, n_tooled);
    #[allow(clippy::cast_precision_loss)]
    let avg_s = ratio(total_steps as f64, n_cmds);
    let avg_tok = ratio(tok_sum, n_cmds);
    let ni_with_tools = flat
        .iter()
        .filter(|d| d.has_tools && !d.is_interruption)
        .count() as i64;
    #[allow(clippy::cast_precision_loss)]
    let pct_st = ratio(ni_with_tools as f64 * 100.0, n_cmds);
    #[allow(clippy::cast_precision_loss)]
    let srch_pct = ratio(total_search as f64 * 100.0, total_tools);

    let real_user_messages = records
        .iter()
        .filter(|r| r.kind == "user" && !r.has_tool_result)
        .count() as i64;

    let mut map = Map::new();
    map.insert("real_user_messages".into(), ji(real_user_messages));
    map.insert("user_commands_analyzed".into(), ji(n_cmds));
    map.insert("commands_requiring_tools".into(), ji(n_tooled));
    map.insert("commands_without_tools".into(), ji(n_cmds - n_tooled));
    map.insert(
        "percentage_requiring_tools".into(),
        round_pynum(pct_t, 1).to_json(),
    );
    map.insert("total_tools_used".into(), ji(total_tools));
    map.insert("total_search_tools".into(), ji(total_search));
    map.insert(
        "search_tool_percentage".into(),
        round_pynum(srch_pct, 1).to_json(),
    );
    map.insert("total_assistant_steps".into(), ji(total_steps));
    map.insert(
        "avg_tools_per_command".into(),
        round_pynum(avg_t, 2).to_json(),
    );
    map.insert(
        "avg_tools_when_used".into(),
        round_pynum(avg_tw, 2).to_json(),
    );
    map.insert(
        "avg_steps_per_command".into(),
        round_pynum(avg_s, 2).to_json(),
    );
    map.insert(
        "avg_tokens_per_command".into(),
        round_pynum(avg_tok, 1).to_json(),
    );
    map.insert(
        "percentage_steps_with_tools".into(),
        round_pynum(pct_st, 1).to_json(),
    );
    map.insert("tool_count_distribution".into(), dist.to_json());
    map.insert("command_details".into(), Value::Array(details));
    map.insert("interruption_rate".into(), round_pynum(ir, 1).to_json());
    map.insert("non_interruption_commands".into(), ji(non_int));
    map.insert("commands_followed_by_interruption".into(), ji(int_followed));
    map.insert("tool_interruption_rates".into(), Value::Object(by_tc));
    map.insert("model_distribution".into(), mdist.to_json());
    Value::Object(map)
}

/// `aggregator._scan_forward`.
fn scan_forward(ordered: &[&Record], idx: usize) -> (i64, Value, i64, Vec<String>, i64) {
    let mut tc = 0_i64;
    let mut model = Value::String("N/A".to_string());
    let mut steps = 0_i64;
    let mut names: Vec<String> = Vec::new();
    let mut search = 0_i64;
    for nxt in &ordered[idx + 1..] {
        if nxt.kind == "user" && !nxt.has_tool_result {
            break;
        }
        if nxt.kind == "assistant" {
            steps += 1;
            if nxt.model_named {
                model = nxt.model.clone();
            }
            for t in &nxt.tools {
                tc += 1;
                names.push(t.name_key());
                if is_search_invocation(t) {
                    search += 1;
                }
            }
        }
    }
    (tc, model, steps, names, search)
}

// ── trends (§1.9) ───────────────────────────────────────────────────────────

/// `aggregator._TREND_ZERO`, in its declared key order.
fn trend_zero() -> Value {
    let mut map = Map::new();
    map.insert("cost_per_command".into(), jf(0.0));
    map.insert("errors_per_command".into(), jf(0.0));
    map.insert("tools_per_command".into(), jf(0.0));
    map.insert("tokens_per_command".into(), jf(0.0));
    map.insert("commands".into(), ji(0));
    map.insert("cost".into(), jf(0.0));
    Value::Object(map)
}

fn empty_trends() -> Value {
    let mut map = Map::new();
    map.insert("current_week".into(), trend_zero());
    map.insert("prior_week".into(), trend_zero());
    map.insert("delta_pct".into(), trend_zero());
    Value::Object(map)
}

/// `aggregator._trends` — the last 7 days against the prior 7.
///
/// `tz_offset` is a parameter in Python and unused there (`noqa: ARG001`), so
/// it is not taken here.
fn trends(ds: &EnrichedDataset, engine: &PricingEngine, provider: &str) -> Value {
    let interactions = &ds.interactions;
    let Some(max_stamp) = ds
        .records
        .iter()
        .filter(|r| !r.timestamp.is_empty())
        .map(|r| r.timestamp.as_str())
        .max()
    else {
        return empty_trends();
    };
    let Some(end) = parse_ts(max_stamp) else {
        return empty_trends();
    };
    let cur_start = end.plus_minutes(-7 * 24 * 60);
    let prior_start = end.plus_minutes(-14 * 24 * 60);

    let mut current: Vec<&Interaction> = Vec::new();
    let mut prior: Vec<&Interaction> = Vec::new();
    for ix in interactions {
        if ix.start_time.is_empty() {
            continue;
        }
        let Some(t) = parse_ts(&ix.start_time) else {
            continue;
        };
        // The comparison is OUTSIDE Python's `try`, so a naive/aware mix raises
        // `TypeError` here and `_safe` collapses the whole section.
        let (Some(after_cur), Some(before_end)) = (cur_start.cmp_instant(t), t.cmp_instant(end))
        else {
            return empty_trends();
        };
        if after_cur.is_lt() && before_end.is_le() {
            current.push(ix);
            continue;
        }
        let (Some(after_prior), Some(before_cur)) =
            (prior_start.cmp_instant(t), t.cmp_instant(cur_start))
        else {
            return empty_trends();
        };
        if after_prior.is_lt() && before_cur.is_le() {
            prior.push(ix);
        }
    }

    let cur_m = trend_metrics(&current, ds, engine, provider);
    let prior_m = trend_metrics(&prior, ds, engine, provider);

    let mut delta = Map::new();
    for (k, cur_v) in &cur_m {
        let prior_v = prior_m
            .iter()
            .find(|(pk, _)| pk == k)
            .map_or(PyNum::Int(0), |(_, v)| *v);
        let value = if k == "commands" {
            // `int - int` stays an `int`.
            match (*cur_v, prior_v) {
                (PyNum::Int(a), PyNum::Int(b)) => PyNum::Int(a - b),
                (a, b) => PyNum::Float(a.as_f64() - b.as_f64()),
            }
        } else if prior_v.as_f64() == 0.0 {
            PyNum::Float(0.0)
        } else {
            PyNum::Float((cur_v.as_f64() - prior_v.as_f64()) / prior_v.as_f64() * 100.0)
        };
        delta.insert(k.clone(), value.to_json());
    }

    let to_obj = |m: &Vec<(String, PyNum)>| {
        let mut out = Map::new();
        for (k, v) in m {
            out.insert(k.clone(), v.to_json());
        }
        Value::Object(out)
    };
    let mut map = Map::new();
    map.insert("current_week".into(), to_obj(&cur_m));
    map.insert("prior_week".into(), to_obj(&prior_m));
    map.insert("delta_pct".into(), Value::Object(delta));
    Value::Object(map)
}

/// `aggregator._trend_metrics`, as an ordered key/value list.
///
/// `dict(_TREND_ZERO)` for an empty window — note `commands` is the only `int`
/// in the six, on both the populated and the empty path.
fn trend_metrics(
    ixs: &[&Interaction],
    ds: &EnrichedDataset,
    engine: &PricingEngine,
    provider: &str,
) -> Vec<(String, PyNum)> {
    if ixs.is_empty() {
        return vec![
            ("cost_per_command".into(), PyNum::Float(0.0)),
            ("errors_per_command".into(), PyNum::Float(0.0)),
            ("tools_per_command".into(), PyNum::Float(0.0)),
            ("tokens_per_command".into(), PyNum::Float(0.0)),
            ("commands".into(), PyNum::Int(0)),
            ("cost".into(), PyNum::Float(0.0)),
        ];
    }
    let mut total_cost_acc = 0.0_f64;
    let mut total_errors = 0_i64;
    let mut total_tools = 0_i64;
    let mut total_tokens = 0_i64;
    for ix in ixs {
        let mut by_model: OrderMap<ModelBucket> = OrderMap::new();
        for r in chain_records(ix, ds) {
            if r.is_error {
                total_errors += 1;
            }
            // `for v in r.tokens.values(): total_tokens += v` — every key,
            // including `reasoning` when present.
            total_tokens += r.tokens.input
                + r.tokens.output
                + r.tokens.cache_creation
                + r.tokens.cache_read
                + r.tokens.reasoning.unwrap_or(0);
            if r.kind == "assistant" && r.model_named {
                by_model
                    .entry(&r.model_speed_key(), || ModelBucket::of(r))
                    .tokens
                    .add(&r.tokens);
            }
        }
        total_tools += ix.tool_count as i64;
        // `total_cost += sum(...)` — the inner `sum` is an `int` 0 when empty,
        // and adding it to a float leaves a float.
        total_cost_acc += sum_bucket_costs(engine, &by_model, provider).as_f64();
    }
    #[allow(clippy::cast_precision_loss)]
    let n = ixs.len() as f64;
    #[allow(clippy::cast_possible_wrap)]
    let commands = ixs.len() as i64;
    #[allow(clippy::cast_precision_loss)]
    let per = |total: i64| PyNum::Float(total as f64 / n);
    vec![
        ("cost_per_command".into(), PyNum::Float(total_cost_acc / n)),
        ("errors_per_command".into(), per(total_errors)),
        ("tools_per_command".into(), per(total_tools)),
        ("tokens_per_command".into(), per(total_tokens)),
        ("commands".into(), PyNum::Int(commands)),
        ("cost".into(), PyNum::Float(total_cost_acc)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classifier::{RawEntry, tag};
    use crate::stats::enricher::build_detailed;
    use serde_json::json;

    /// The eighteen sections, in the order Python's `return` lists them.
    const SECTIONS: [&str; 18] = [
        "overview",
        "tools",
        "sessions",
        "daily_stats",
        "hourly_pattern",
        "errors",
        "models",
        "user_interactions",
        "cache",
        "session_costs",
        "command_costs",
        "tool_costs",
        "token_composition",
        "outliers",
        "retry_signals",
        "session_efficiency",
        "error_cost",
        "trends",
    ];

    fn engine() -> PricingEngine {
        PricingEngine::from_manifest(crate::pricing::test_support::sample_manifest())
    }

    fn dataset(payloads: Vec<Value>) -> EnrichedDataset {
        build_detailed(tag(payloads
            .into_iter()
            .map(|payload| RawEntry {
                payload,
                session_id: "sess-1".into(),
                provider: "anthropic".into(),
            })
            .collect()))
    }

    /// One user turn, one tool-using assistant turn, one erroring tool result.
    fn fixture() -> EnrichedDataset {
        dataset(vec![
            json!({"type": "human", "timestamp": "2026-03-04T10:00:00+00:00",
                   "uuid": "u1", "message": {"content": "please read the file"}}),
            json!({"type": "assistant", "timestamp": "2026-03-04T10:00:05+00:00",
                   "uuid": "u2", "message": {
                       "id": "m1", "model": "claude-opus-4-8",
                       "usage": {"input_tokens": 100, "output_tokens": 200,
                                 "cache_creation_input_tokens": 300,
                                 "cache_read_input_tokens": 400},
                       "content": [{"type": "tool_use", "id": "t1", "name": "Read",
                                    "input": {"file_path": "/a"}}]}}),
            json!({"type": "human", "timestamp": "2026-03-04T10:00:09+00:00",
                   "uuid": "u3", "message": {"content": [
                       {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                        "content": "Error: file has not been read yet"}]}}),
            json!({"type": "assistant", "timestamp": "2026-03-04T10:01:00+00:00",
                   "uuid": "u4", "message": {
                       "id": "m2", "model": "claude-opus-4-8",
                       "usage": {"input_tokens": 10, "output_tokens": 20,
                                 "cache_creation_input_tokens": 0,
                                 "cache_read_input_tokens": 0},
                       "content": "done"}}),
        ])
    }

    #[test]
    fn the_payload_has_eighteen_sections_in_pythons_order() {
        let stats = summarise(&fixture(), "", 0, &engine());
        let keys: Vec<&str> = stats
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, SECTIONS);
    }

    #[test]
    fn the_int_float_split_survives_to_the_wire() {
        // An empty dataset drives every "x if cond else 0" to its INT branch,
        // which is the half a naive port gets wrong: `json.dumps` writes `0`
        // there and `0.0` for the seeded-float accumulators beside it.
        let stats = summarise(&EnrichedDataset::default(), "", 0, &engine());
        assert_eq!(
            stats["sessions"]["average_duration_seconds"].to_string(),
            "0"
        );
        assert_eq!(stats["sessions"]["average_messages"].to_string(), "0");
        assert_eq!(stats["errors"]["rate"].to_string(), "0");
        assert_eq!(stats["cache"]["hit_rate"].to_string(), "0");
        assert_eq!(stats["cache"]["efficiency"].to_string(), "0");
        assert_eq!(stats["cache"]["cache_roi"].to_string(), "0");
        // …while `cost_saved_base_units` is `round(0.0 * 1e6, 2)` — a float.
        assert_eq!(stats["cache"]["cost_saved_base_units"].to_string(), "0.0");
        assert_eq!(stats["overview"]["total_cost"].to_string(), "0");
        assert_eq!(
            stats["user_interactions"]["avg_tools_per_command"].to_string(),
            "0"
        );
        assert_eq!(stats["trends"]["current_week"]["cost"].to_string(), "0.0");
        assert_eq!(stats["trends"]["current_week"]["commands"].to_string(), "0");
        assert_eq!(stats["trends"]["delta_pct"]["commands"].to_string(), "0");
        assert_eq!(
            stats["trends"]["delta_pct"]["cost_per_command"].to_string(),
            "0.0"
        );
    }

    #[test]
    fn command_cost_is_an_int_zero_and_session_cost_a_float_zero() {
        // The same emptiness, two different literals — `sum([])` against
        // `cost = 0.0`. Four lines apart in `aggregator.py`.
        let ds = dataset(vec![
            json!({"type": "human", "timestamp": "2026-03-04T10:00:00+00:00",
                                     "message": {"content": "no reply ever came"}}),
        ]);
        let stats = summarise(&ds, "", 0, &engine());
        assert_eq!(stats["command_costs"][0]["cost"].to_string(), "0");
        assert_eq!(stats["session_costs"][0]["cost"].to_string(), "0.0");
        // An interaction with no responses also has an EMPTY token dict, not
        // four zeros — `dict(Counter())` is `{}`.
        assert_eq!(stats["command_costs"][0]["tokens"], json!({}));
        // …while the session's token bag saw the command record, so it has all
        // four keys.
        assert_eq!(
            stats["session_costs"][0]["tokens"],
            json!({"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0})
        );
    }

    #[test]
    fn the_fixture_lands_where_the_python_source_says_it_should() {
        let stats = summarise(&fixture(), "", 0, &engine());
        assert_eq!(stats["overview"]["total_messages"], json!(4));
        assert_eq!(stats["overview"]["sessions"], json!(1));
        assert_eq!(
            stats["overview"]["total_tokens"],
            json!({"input": 110, "output": 220, "cache_creation": 300, "cache_read": 400})
        );
        assert_eq!(stats["tools"]["usage_counts"], json!({"Read": 1}));
        assert_eq!(stats["errors"]["total"], json!(1));
        assert_eq!(stats["errors"]["by_category"], json!({"File Not Read": 1}));
        assert_eq!(stats["models"]["claude-opus-4-8"]["count"], json!(2));
        assert_eq!(stats["user_interactions"]["real_user_messages"], json!(1));
        assert_eq!(stats["user_interactions"]["total_tools_used"], json!(1));
        assert_eq!(
            stats["user_interactions"]["total_assistant_steps"],
            json!(2)
        );
        // The day bucket is keyed by the LOCAL day, and every timestamp here is
        // 2026-03-04 UTC.
        let days: Vec<&str> = stats["daily_stats"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(days, vec!["2026-03-04"]);
        assert_eq!(stats["daily_stats"]["2026-03-04"]["messages"], json!(4));
        assert_eq!(stats["daily_stats"]["2026-03-04"]["errors"], json!(1));
        // `hourly_pattern` is keyed 0..23 as STRINGS, in order, always 24 wide.
        let hours = stats["hourly_pattern"]["messages"]
            .as_object()
            .expect("object");
        assert_eq!(hours.len(), 24);
        assert_eq!(hours.keys().next().map(String::as_str), Some("0"));
        assert_eq!(hours["10"], json!(4));
        // The error's tool is attributed through `tool_use_id`, which only
        // works because `raw_data` survives a `Detail::Full` build.
        assert_eq!(stats["error_cost"]["errors_by_tool"], json!({"Read": 1}));
        // Cache: one message wrote and one read, out of two assistant turns.
        assert_eq!(stats["cache"]["assistant_messages"], json!(2));
        assert_eq!(stats["cache"]["hit_rate"], json!(50.0));
        assert_eq!(stats["cache"]["tokens_saved"], json!(100));
        assert_eq!(stats["cache"]["break_even_achieved"], json!(true));
    }

    #[test]
    fn the_tz_offset_moves_the_day_bucket_by_wall_minutes() {
        // Python ADDS `timedelta(minutes=tz_offset)`; the React client believes
        // it is sending the opposite sign (spec §6b) and this port does not
        // correct that.
        let ds = dataset(vec![json!({"type": "assistant",
                                     "timestamp": "2026-03-04T23:30:00+00:00",
                                     "message": {"content": "late"}})]);
        for (offset, day, hour) in [
            (0_i64, "2026-03-04", "23"),
            (60, "2026-03-05", "0"),
            (-60, "2026-03-04", "22"),
        ] {
            let stats = summarise(&ds, "", offset, &engine());
            assert_eq!(
                stats["daily_stats"]
                    .as_object()
                    .expect("object")
                    .keys()
                    .next()
                    .map(String::as_str),
                Some(day),
                "offset {offset}"
            );
            assert_eq!(stats["hourly_pattern"]["messages"][hour], json!(1));
        }
    }

    #[test]
    fn neumaier_is_cpython_sum_and_not_a_running_total() {
        // `sum([0.1]*10 + [1e16, -1e16])` is `1.0` in CPython 3.12 and `0.0`
        // with a plain accumulator. The compensated answer is the one on the
        // wire.
        let mut values = vec![0.1_f64; 10];
        values.push(1e16);
        values.push(-1e16);
        assert!((sum_in_order(&values) - 1.0).abs() < f64::EPSILON);
        let mut plain = 0.0_f64;
        for v in &values {
            plain += *v;
        }
        assert!((plain - 0.0).abs() < f64::EPSILON);
        // An empty `sum()` is the `int` 0 of the `start` argument.
        assert_eq!(Neumaier::default().finish_pynum(), PyNum::Int(0));
    }

    #[test]
    fn round_py_is_ties_to_even_on_the_decimal_expansion() {
        // Every right-hand side here is CPython's answer, taken from
        // `round(x, n)` on 3.12.
        assert!((round_py(0.125, 2) - 0.12).abs() < f64::EPSILON);
        assert!((round_py(0.135, 2) - 0.14).abs() < f64::EPSILON);
        assert!((round_py(2.675, 2) - 2.67).abs() < f64::EPSILON);
        assert!((round_py(0.25, 1) - 0.2).abs() < f64::EPSILON);
        assert!((round_py(0.35, 1) - 0.3).abs() < f64::EPSILON);
        assert!((round_py(0.45, 1) - 0.5).abs() < f64::EPSILON);
        assert!((round_py(1.5, 1) - 1.5).abs() < f64::EPSILON);
        assert!((round_py(-0.125, 2) + 0.12).abs() < f64::EPSILON);
        // `f64::round` would say 0.13 for the first of those.
        assert!(((0.125_f64 * 100.0).round() / 100.0 - 0.13).abs() < f64::EPSILON);
    }

    #[test]
    fn pynum_keeps_the_int_float_split_json_dumps_writes() {
        assert_eq!(PyNum::Int(0).to_json().to_string(), "0");
        assert_eq!(PyNum::Float(0.0).to_json().to_string(), "0.0");
        assert_eq!(min_100(PyNum::Float(250.0)), PyNum::Int(100));
        assert_eq!(min_100(PyNum::Float(99.5)), PyNum::Float(99.5));
    }

    #[test]
    fn preview_flattens_then_strips_then_truncates() {
        assert_eq!(preview("  a\nb\rc  ", 100), "a b c");
        assert_eq!(preview("", 10), "");
        assert_eq!(preview("abcdef", 3), "abc");
        // The strip happens BEFORE the slice, so leading space is not counted.
        assert_eq!(preview("   abcdef", 3), "abc");
    }

    #[test]
    fn path_name_matches_purepath_name() {
        assert_eq!(path_name("/a/b/c"), "c");
        assert_eq!(path_name("/a/b/"), "b");
        assert_eq!(path_name(""), "");
        assert_eq!(path_name("/"), "");
        assert_eq!(path_name("a"), "a");
        assert_eq!(path_name("a/./b"), "b");
        assert_eq!(path_name("a/."), "a");
    }

    // ── summarise_session_costs (batch E, RS-5-105) ─────────────────────────

    #[test]
    fn the_session_cost_shortcut_is_the_section_summarise_would_have_built() {
        // The whole contract of the shortcut: same rows, same order, same
        // int/float split, same last bits on every float — for a dataset with
        // priced assistant turns, an error, and a real interaction chain.
        let ds = fixture();
        assert_eq!(
            summarise_session_costs(&ds, &engine()),
            summarise(&ds, "", 0, &engine())["session_costs"]
        );
    }

    #[test]
    fn the_shortcut_seeds_a_float_zero_cost_like_the_full_sweep() {
        // LAW 3's other half — a session with no priced model must still write
        // `"cost": 0.0`, because `_SessionCostCollector` seeds `cost = 0.0`
        // where `_CommandCostCollector` seeds `sum([])`.
        let ds = dataset(vec![
            json!({"type": "human", "timestamp": "2026-03-04T10:00:00+00:00",
                                     "message": {"content": "no reply ever came"}}),
        ]);
        let rows = summarise_session_costs(&ds, &engine());
        assert_eq!(rows[0]["cost"].to_string(), "0.0");
        assert_eq!(rows[0]["duration_s"].to_string(), "0.0");
        assert_eq!(rows[0]["messages"].to_string(), "1");
        assert_eq!(
            rows[0]["tokens"],
            json!({"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0})
        );
    }

    #[test]
    fn an_empty_dataset_is_an_empty_array_not_a_null() {
        // Python's `_safe(…, [])` fallback and the route's `or []` both land on
        // `[]`; so does the collector with nothing to iterate.
        assert_eq!(
            summarise_session_costs(&EnrichedDataset::default(), &engine()),
            json!([])
        );
    }

    #[test]
    fn the_rows_carry_every_key_the_compare_endpoint_reads() {
        // `/api/sessions/compare` indexes `session_id`, `cost`, `tokens`,
        // `commands`, `errors` and `duration_s` by name and would answer a
        // silently-renamed key with a 404 rather than a crash.
        let rows = summarise_session_costs(&fixture(), &engine());
        let keys: Vec<&str> = rows[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "session_id",
                "started_at",
                "ended_at",
                "duration_s",
                "cost",
                "tokens",
                "messages",
                "commands",
                "errors",
                "first_prompt_preview",
                "models_used",
            ]
        );
        assert_eq!(rows[0]["session_id"], json!("sess-1"));
        assert_eq!(rows[0]["errors"], json!(1));
        assert_eq!(rows[0]["commands"], json!(1));
    }

    #[test]
    fn the_shortcut_resolves_the_provider_from_the_first_record() {
        // Not a parameter and not per-record: `ds.records[0].provider`, once.
        // An unknown provider prices to nothing, which is the observable proof
        // that the resolution happened at all.
        let ds = build_detailed(tag(vec![RawEntry {
            payload: json!({"type": "assistant", "timestamp": "2026-03-04T10:00:00+00:00",
                            "message": {"model": "claude-opus-4-8",
                                        "usage": {"input_tokens": 1000, "output_tokens": 1000}}}),
            session_id: "sess-1".into(),
            provider: "no-such-provider".into(),
        }]));
        assert_eq!(
            summarise_session_costs(&ds, &engine()),
            summarise(&ds, "", 0, &engine())["session_costs"]
        );
    }

    #[test]
    fn search_verbs_survive_pipes_semicolons_and_and_and() {
        assert!(cmd_has_search_verb("rg foo"));
        assert!(cmd_has_search_verb("cat x | grep y"));
        assert!(cmd_has_search_verb("cd /tmp && ls"));
        assert!(cmd_has_search_verb("true; ack pattern"));
        assert!(cmd_has_search_verb("RG FOO"));
        assert!(!cmd_has_search_verb("cat x"));
        assert!(!cmd_has_search_verb("mygrep x"));
        assert!(!cmd_has_search_verb(""));
    }
}

//! `python-legacy: settings.py` — the descriptor-based settings model.
//!
//! Python declares each setting as a `_Opt(default, env, validator)` class
//! variable and resolves **env → file → default** on *every* attribute read.
//! Two properties of that model are byte-visible and are what this port exists
//! to preserve:
//!
//! * **Declaration order is a wire contract.** `Settings._keys()` walks
//!   `cls.__dict__`, which is insertion-ordered, so `cfg ls --json` emits the
//!   keys in the order they appear in `settings.py` — *not* sorted. The text
//!   view of the same data *is* sorted (`for key in sorted(data)`), so the two
//!   orders must both be reproduced. [`SPECS`] is therefore an array, not a map.
//! * **`type(default)` drives both the env cast and `cfg set`'s parse.**
//!   `_cast` branches on `bool` / `int` / `float` and otherwise hands back the
//!   raw string; `cfg set` branches on `bool` / `int` and otherwise stores the
//!   string. A key whose default is `None` (`budget_monthly_usd`) therefore
//!   stores the **string** `"50"`, not the number — reproduced here, because it
//!   is what `cfg ls` then prints and what `config.json` then carries.
//!
//! The file itself is `$STACKUNDERFLOW_HOME/config.json`, written by
//! `json.dumps(data, indent=2)` with **no trailing newline**.
//!
//! # Deviation from Python that is not observable
//!
//! Python binds `_APP_DIR` / `_CFG_FILE` at *import*, so a test that mutates
//! `$STACKUNDERFLOW_HOME` after import keeps writing to the old path. This port
//! resolves the directory per call ([`stax_core::settings::app_dir`]), which is
//! strictly more permissive: no caller can observe a stale value, and every
//! process the parity harness starts sets the variable before exec.

use std::path::PathBuf;

use stax_core::queries::paths::py_repr;
use stax_core::queries::pyint::PyInt;
use stax_core::queries::pyjson::{self, Value};
use stax_core::settings::app_dir;

/// The literal a `_Opt` was declared with — `type(self.default)` is the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Default {
    /// `_Opt(True, …)` / `_Opt(False, …)` — `bool`.
    Bool(bool),
    /// `_Opt(8081, …)` — `int`.
    Int(i64),
    /// `_Opt("USD", …)` — `str`.
    Str(&'static str),
    /// `_Opt(None, …)` — `NoneType`.
    None,
    /// `_Opt({}, None)` — `dict`.
    Dict,
    /// `_Opt([50, 75, 90], None)` — `list`.
    IntList(&'static [i64]),
}

impl Default {
    /// A fresh copy of the declared default, as `__get__`'s third leg returns.
    #[must_use]
    pub fn value(self) -> Value {
        match self {
            Self::Bool(flag) => Value::Bool(flag),
            Self::Int(number) => Value::Int(number),
            Self::Str(text) => Value::Str(text.to_owned()),
            Self::None => Value::Null,
            Self::Dict => Value::Object(Vec::new()),
            Self::IntList(items) => Value::Array(items.iter().copied().map(Value::Int).collect()),
        }
    }

    /// `_Opt._cast(raw)` — the env leg's `type(self.default)` switch.
    ///
    /// `bool` is the membership test, `int` is CPython's `int()` with a
    /// *silent fallback to the default* on `ValueError` (not an error — the
    /// reference swallows it), and everything else is the raw string. There is
    /// no `float`-defaulted setting today; the arm exists because `_cast` has
    /// one and a new setting must not change behaviour by omission.
    #[must_use]
    pub fn cast(self, raw: &str) -> Value {
        match self {
            Self::Bool(_) => Value::Bool(matches!(
                raw.to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )),
            Self::Int(fallback) => match PyInt::parse(raw) {
                Some(parsed) => Value::Int(parsed.saturating_i64()),
                None => Value::Int(fallback),
            },
            _ => Value::Str(raw.to_owned()),
        }
    }
}

/// One `_Opt` declaration.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// The attribute name — the config-file key and the `cfg` CLI key.
    pub key: &'static str,
    /// The environment variable, or `None` for a file-only setting.
    pub env: Option<&'static str>,
    /// The declared default.
    pub default: Default,
    /// True for `currency`, the one setting with a `validator=`.
    pub validated: bool,
}

/// Every setting, **in `settings.py` declaration order**.
///
/// `Settings.DEFAULTS` and `Settings.ENV_MAPPINGS` are both built from
/// `_opt_descriptors()`, which is this same order; `cfg ls --json` emits it
/// verbatim and `cfg set`'s "Valid:" list sorts it.
pub const SPECS: &[Spec] = &[
    spec("port", Some("PORT"), Default::Int(8081)),
    spec("host", Some("HOST"), Default::Str("127.0.0.1")),
    spec("auto_browser", Some("AUTO_BROWSER"), Default::Bool(true)),
    spec(
        "max_date_range_days",
        Some("MAX_DATE_RANGE_DAYS"),
        Default::Int(30),
    ),
    spec(
        "messages_initial_load",
        Some("MESSAGES_INITIAL_LOAD"),
        Default::Int(500),
    ),
    spec("log_level", Some("LOG_LEVEL"), Default::Str("INFO")),
    spec(
        "auto_reindex_on_ingest",
        Some("AUTO_REINDEX_ON_INGEST"),
        Default::Bool(true),
    ),
    Spec {
        key: "currency",
        env: Some("STACKUNDERFLOW_CURRENCY"),
        default: Default::Str("USD"),
        validated: true,
    },
    spec("model_aliases", None, Default::Dict),
    spec("plan_name", None, Default::None),
    spec("plan_monthly_usd", None, Default::None),
    spec("plan_reset_day", None, Default::Int(1)),
    spec("budget_monthly_usd", None, Default::None),
    spec("budget_daily_usd", None, Default::None),
    spec(
        "plan_alert_thresholds",
        None,
        Default::IntList(&[50, 75, 90]),
    ),
    spec(
        "discovery_budget_tokens",
        Some("STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS"),
        Default::Int(2000),
    ),
    spec(
        "discovery_rank_weights",
        Some("STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS"),
        Default::Str("0.5,0.2,0.3"),
    ),
    spec(
        "proactive_enabled",
        Some("STACKUNDERFLOW_PROACTIVE_ENABLED"),
        Default::Bool(false),
    ),
    spec(
        "proactive_types",
        Some("STACKUNDERFLOW_PROACTIVE_TYPES"),
        Default::Str("command-cluster,file-risk"),
    ),
    spec(
        "proactive_max_per_session",
        Some("STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION"),
        Default::Int(3),
    ),
    spec(
        "proactive_cooldown_hours",
        Some("STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS"),
        Default::Int(24),
    ),
    spec("proactive_dismiss_suppress_after", None, Default::Int(3)),
];

const fn spec(key: &'static str, env: Option<&'static str>, default: Default) -> Spec {
    Spec {
        key,
        env,
        default,
        validated: false,
    }
}

/// The [`Spec`] for `key`, or `None` — `type(self).__dict__.get(key)`.
#[must_use]
pub fn spec_of(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|spec| spec.key == key)
}

/// Every key, sorted — the `', '.join(sorted(Settings.DEFAULTS))` in `cfg set`'s
/// unknown-key message and the `sorted(data)` the text view iterates.
#[must_use]
pub fn sorted_keys() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = SPECS.iter().map(|spec| spec.key).collect();
    keys.sort_unstable();
    keys
}

// ── the config file ──────────────────────────────────────────────────────────

/// `$STACKUNDERFLOW_HOME/config.json`.
#[must_use]
pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// An insertion-ordered `dict[str, Any]` — what `_load()` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigFile {
    entries: Vec<(String, Value)>,
}

impl ConfigFile {
    /// `key in saved`.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(name, _)| name == key)
    }

    /// `saved[key]`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// `data[key] = value` — replaces **in place** when the key exists, which
    /// is how a Python dict behaves and what keeps `config.json`'s key order
    /// stable across repeated `cfg set` calls.
    pub fn insert(&mut self, key: &str, value: Value) {
        match self.entries.iter_mut().find(|(name, _)| name == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key.to_owned(), value)),
        }
    }

    /// `data.pop(key, None)`.
    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|(name, _)| name != key);
    }

    /// The file as the `dict` `json.dumps` renders.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.entries.clone())
    }
}

/// `settings._load()` — the file, or an empty dict on **any** failure.
///
/// Python catches `OSError` and `json.JSONDecodeError` only. A file holding
/// valid JSON that is not an object (`[]`, `3`) survives that `try` and is
/// returned as-is; every subsequent operation then behaves as if the key were
/// absent (`in` on a list is a membership test, `data[key] = …` raises). This
/// port folds a non-object into the empty dict — recorded as DIV-236, and
/// unreachable through any command in the tree, all of which write objects.
#[must_use]
pub fn load() -> ConfigFile {
    load_from(&config_path())
}

/// [`load`], with the path injected — the testable half.
#[must_use]
pub fn load_from(path: &std::path::Path) -> ConfigFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ConfigFile::default();
    };
    match pyjson::loads(&text) {
        Some(Value::Object(entries)) => ConfigFile { entries },
        _ => ConfigFile::default(),
    }
}

/// `settings._save(data)` — `mkdir(parents=True)` then
/// `json.dumps(data, indent=2)`, with **no** trailing newline.
///
/// # Errors
/// Any filesystem failure. Python lets those propagate too.
pub fn save(config: &ConfigFile) -> anyhow::Result<()> {
    let dir = app_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("config.json"),
        pyjson::dumps_indent2(&config.to_value()),
    )?;
    Ok(())
}

// ── resolution ───────────────────────────────────────────────────────────────

/// Where a resolved value came from — `cfg ls`'s third column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The environment variable is set (to anything, including `""`).
    Env,
    /// The key is present in `config.json`.
    File,
    /// Neither — the declared default.
    Default,
}

impl Source {
    /// The literal `cfg ls` prints inside the brackets.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::File => "file",
            Self::Default => "default",
        }
    }
}

/// The environment, injected — `set_var` is `unsafe` in Rust 2024 and the
/// workspace forbids `unsafe`, so every env read is a parameter (campaign
/// finding 5).
pub trait Env {
    /// `os.getenv(name)`.
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed table, for tests.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(pub Vec<(String, String)>);

impl Env for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

/// `_Opt.__get__` — env, then file, then a fresh copy of the default.
#[must_use]
pub fn resolve(spec: &Spec, config: &ConfigFile, env: &dyn Env) -> Value {
    if let Some(name) = spec.env
        && let Some(raw) = env.get(name)
    {
        return spec.default.cast(&raw);
    }
    if let Some(value) = config.get(spec.key) {
        // "Defensive: a corrupt config (wrong type) falls back to default."
        return match (spec.default, value) {
            (Default::Dict, Value::Object(_)) | (Default::IntList(_), Value::Array(_)) => {
                value.clone()
            }
            (Default::Dict | Default::IntList(_), _) => spec.default.value(),
            _ => value.clone(),
        };
    }
    spec.default.value()
}

/// Where [`resolve`] took the value from.
#[must_use]
pub fn source_of(spec: &Spec, config: &ConfigFile, env: &dyn Env) -> Source {
    // `if env_var and os.getenv(env_var) is not None` — the `and` is
    // truthiness on the *name*, so a hypothetical `env=""` would be skipped.
    if let Some(name) = spec.env
        && !name.is_empty()
        && env.get(name).is_some()
    {
        return Source::Env;
    }
    if config.contains(spec.key) {
        return Source::File;
    }
    Source::Default
}

/// `Settings.get_all()` — every key, in declaration order.
#[must_use]
pub fn get_all(config: &ConfigFile, env: &dyn Env) -> Vec<(&'static str, Value)> {
    SPECS
        .iter()
        .map(|spec| (spec.key, resolve(spec, config, env)))
        .collect()
}

/// `Settings.get(key)` for a key known to exist.
#[must_use]
pub fn get(key: &str, config: &ConfigFile, env: &dyn Env) -> Option<Value> {
    spec_of(key).map(|spec| resolve(spec, config, env))
}

/// `settings._validate_currency` — the one validator in the model.
///
/// # Errors
/// The verbatim `ValueError` message, which `cfg set` re-raises as a
/// `BadParameter` on `VALUE`.
pub fn validate_currency(value: &Value) -> Result<Value, String> {
    const MESSAGE: &str = "currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)";
    let Value::Str(text) = value else {
        return Err(MESSAGE.to_owned());
    };
    // `value.strip().upper()` then `^[A-Z]{3}$`. `str.upper()` is full Unicode
    // case mapping, so the ASCII-only regex rejects anything that is not three
    // ASCII letters after folding — `.to_uppercase()` is the faithful call and
    // the length test below is on the folded form, exactly as Python's is.
    let code: String = text.trim_matches(is_py_space).to_uppercase();
    let ascii_upper = code.len() == 3 && code.chars().all(|ch| ch.is_ascii_uppercase());
    if ascii_upper {
        Ok(Value::Str(code))
    } else {
        Err(MESSAGE.to_owned())
    }
}

/// `str.strip()`'s whitespace set.
fn is_py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{0b}' | '\u{0c}' | '\u{1c}'..='\u{1f}')
}

/// `Settings.persist(key, value)` — validate, load, set, save.
///
/// # Errors
/// `Err(Ok(message))` is the validator's `ValueError`; `Err(Err(error))` is a
/// filesystem failure.
pub fn persist(key: &str, value: Value) -> Result<(), PersistError> {
    let value = match spec_of(key) {
        Some(spec) if spec.validated => validate_currency(&value).map_err(PersistError::Value)?,
        _ => value,
    };
    let mut config = load();
    config.insert(key, value);
    save(&config).map_err(PersistError::Io)
}

/// What [`persist`] can fail with.
#[derive(Debug)]
pub enum PersistError {
    /// The validator raised — Python's `ValueError`, caught by `cfg set`.
    Value(String),
    /// The write failed. Python lets this propagate.
    Io(anyhow::Error),
}

/// `Settings.remove(key)` — pop and save, **unconditionally**.
///
/// Note the side effect the reference has and this reproduces: `_save` runs
/// even when the key was absent, so `cfg rm anything` *creates*
/// `config.json` (holding `{}`) on a home that had none.
///
/// # Errors
/// Any filesystem failure.
pub fn remove(key: &str) -> anyhow::Result<()> {
    let mut config = load();
    config.remove(key);
    save(&config)
}

// ── Python `str()` / `repr()` over a resolved value ──────────────────────────

/// `str(value)` — what an f-string interpolates.
///
/// `str` of a container uses `repr` for its elements, which is why this and
/// [`py_repr_value`] are mutually recursive.
#[must_use]
pub fn py_str_value(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        other => py_repr_value(other),
    }
}

/// `repr(value)`.
#[must_use]
pub fn py_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Int(number) => number.to_string(),
        Value::Float(number) => pyjson::repr_float(*number),
        Value::Str(text) => py_repr(text),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(py_repr_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(entries) => {
            if entries.is_empty() {
                return "{}".to_owned();
            }
            let rendered: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr(key), py_repr_value(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> ConfigFile {
        ConfigFile::default()
    }

    #[test]
    fn the_declaration_order_is_the_json_order() {
        let keys: Vec<&str> = SPECS.iter().map(|spec| spec.key).collect();
        assert_eq!(keys.first(), Some(&"port"));
        assert_eq!(keys.last(), Some(&"proactive_dismiss_suppress_after"));
        assert_eq!(keys.len(), 22);
        // …and it is NOT sorted — the text view sorts, the JSON view does not.
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_ne!(keys, sorted);
    }

    #[test]
    fn every_key_is_unique() {
        let mut keys = sorted_keys();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len());
    }

    #[test]
    fn defaults_round_trip_to_the_python_reprs() {
        let env = MapEnv::default();
        let config = empty();
        let all = get_all(&config, &env);
        let rendered: Vec<(&str, String)> = all
            .iter()
            .map(|(key, value)| (*key, py_str_value(value)))
            .collect();
        assert!(rendered.contains(&("port", "8081".to_owned())));
        assert!(rendered.contains(&("auto_browser", "True".to_owned())));
        assert!(rendered.contains(&("plan_name", "None".to_owned())));
        assert!(rendered.contains(&("plan_alert_thresholds", "[50, 75, 90]".to_owned())));
        assert!(rendered.contains(&("proactive_enabled", "False".to_owned())));
    }

    #[test]
    fn a_dict_default_renders_as_json_not_as_str() {
        // `cfg ls` uses `json.dumps(val)` for dicts and `str(val)` otherwise;
        // Python's `str({})` is also `{}`, but `str({'a': 'b'})` is
        // `{'a': 'b'}` where `json.dumps` gives `{"a": "b"}`. The command picks
        // json.dumps, so the caller must too.
        let value = Value::Object(vec![("a".into(), Value::Str("b".into()))]);
        assert_eq!(py_str_value(&value), "{'a': 'b'}");
        assert_eq!(pyjson::dumps_default(&value), "{\"a\": \"b\"}");
    }

    #[test]
    fn the_env_leg_casts_by_the_defaults_type() {
        let env = MapEnv(vec![
            ("PORT".into(), "9090".into()),
            ("AUTO_BROWSER".into(), "NO".into()),
            ("LOG_LEVEL".into(), "debug".into()),
        ]);
        let config = empty();
        assert_eq!(
            get("port", &config, &env),
            Some(Value::Int(9090)),
            "int() on the raw string"
        );
        assert_eq!(
            get("auto_browser", &config, &env),
            Some(Value::Bool(false)),
            "`no` is not in the truthy set"
        );
        assert_eq!(
            get("log_level", &config, &env),
            Some(Value::Str("debug".into())),
            "str settings are handed back raw, no case folding"
        );
    }

    #[test]
    fn a_bad_int_env_falls_back_to_the_default_silently() {
        let env = MapEnv(vec![("PORT".into(), "not-a-port".into())]);
        assert_eq!(get("port", &empty(), &env), Some(Value::Int(8081)));
    }

    #[test]
    fn an_empty_env_value_still_counts_as_set() {
        // `os.getenv(env) is not None` — `""` is not None, so the env leg wins
        // and `_cast("")` runs. For `host` that yields the empty string.
        let env = MapEnv(vec![("HOST".into(), String::new())]);
        assert_eq!(get("host", &empty(), &env), Some(Value::Str(String::new())));
        assert_eq!(
            source_of(spec_of("host").unwrap(), &empty(), &env),
            Source::Env
        );
    }

    #[test]
    fn int_env_values_use_cpython_int_not_rust_parse() {
        let env = MapEnv(vec![("MAX_DATE_RANGE_DAYS".into(), " +1_0 ".into())]);
        assert_eq!(
            get("max_date_range_days", &empty(), &env),
            Some(Value::Int(10)),
            "whitespace, a leading +, and _ separators are all int()-legal"
        );
    }

    #[test]
    fn the_file_leg_is_returned_verbatim_except_for_container_type_mismatch() {
        let mut config = empty();
        config.insert("port", Value::Str("nine thousand".into()));
        config.insert("model_aliases", Value::Str("not a dict".into()));
        config.insert("plan_alert_thresholds", Value::Int(5));
        let env = MapEnv::default();
        assert_eq!(
            get("port", &config, &env),
            Some(Value::Str("nine thousand".into())),
            "a scalar mismatch is NOT defended against — Python returns it raw"
        );
        assert_eq!(
            get("model_aliases", &config, &env),
            Some(Value::Object(Vec::new())),
            "a dict-typed setting falls back on a non-dict"
        );
        assert_eq!(
            get("plan_alert_thresholds", &config, &env),
            Some(Value::Array(vec![
                Value::Int(50),
                Value::Int(75),
                Value::Int(90)
            ])),
            "a list-typed setting falls back on a non-list"
        );
    }

    #[test]
    fn the_source_column_ranks_env_over_file_over_default() {
        let mut config = empty();
        config.insert("port", Value::Int(1));
        let with_env = MapEnv(vec![("PORT".into(), "2".into())]);
        let without = MapEnv::default();
        let port = spec_of("port").unwrap();
        assert_eq!(source_of(port, &config, &with_env), Source::Env);
        assert_eq!(source_of(port, &config, &without), Source::File);
        assert_eq!(source_of(port, &empty(), &without), Source::Default);
    }

    #[test]
    fn a_file_only_setting_never_reports_env() {
        // `model_aliases` has env=None; the probe must be skipped, not run
        // against a variable named after the key.
        let aliases = spec_of("model_aliases").unwrap();
        assert!(aliases.env.is_none());
        let env = MapEnv(vec![("model_aliases".into(), "x".into())]);
        assert_eq!(source_of(aliases, &empty(), &env), Source::Default);
    }

    #[test]
    fn currency_validation_matches_the_reference_message() {
        assert_eq!(
            validate_currency(&Value::Str("eur".into())),
            Ok(Value::Str("EUR".into()))
        );
        assert_eq!(
            validate_currency(&Value::Str("  gbp  ".into())),
            Ok(Value::Str("GBP".into()))
        );
        let expected =
            Err("currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)".to_owned());
        assert_eq!(validate_currency(&Value::Str("zz".into())), expected);
        assert_eq!(validate_currency(&Value::Str("EUROS".into())), expected);
        assert_eq!(validate_currency(&Value::Int(3)), expected);
    }

    #[test]
    fn insert_keeps_the_files_key_order() {
        let mut config = empty();
        config.insert("a", Value::Int(1));
        config.insert("b", Value::Int(2));
        config.insert("a", Value::Int(3));
        assert_eq!(
            pyjson::dumps_indent2(&config.to_value()),
            "{\n  \"a\": 3,\n  \"b\": 2\n}"
        );
    }

    #[test]
    fn a_corrupt_config_file_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("stax-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_from(&path), ConfigFile::default());
        std::fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(load_from(&path), ConfigFile::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repr_of_a_string_uses_pythons_quote_preference() {
        assert_eq!(py_repr_value(&Value::Str("x".into())), "'x'");
        assert_eq!(py_repr_value(&Value::Str("it's".into())), "\"it's\"");
        assert_eq!(py_str_value(&Value::Str("it's".into())), "it's");
    }
}

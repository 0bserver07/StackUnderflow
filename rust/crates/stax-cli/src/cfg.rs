//! `stax cfg` and `stax config` — `cli.py:413`–`:566` and `:783`–`:806`.
//!
//! Seven verbs over one JSON file, plus a hidden three-verb back-compat group
//! that `ctx.invoke`s straight into three of them. What makes them worth a
//! careful port rather than a transcription:
//!
//! * **Two key orders, both live.** `cfg ls` prints `sorted(data)`; `cfg ls
//!   --json` prints `json.dumps(get_all())`, which is *declaration* order. A
//!   port that stored settings in a map would silently pick one.
//! * **`cfg set` writes the string when the default is `None`.**
//!   `budget_monthly_usd` has no type to cast to, so `cfg set
//!   budget_monthly_usd 50` puts `"50"` — a JSON *string* — in `config.json`,
//!   and `cfg ls` then prints `50` because `str("50")` is `50`. Reproduced.
//! * **`cfg rm` always writes.** `Settings.remove` pops-with-default and then
//!   saves unconditionally, so removing a key that was never set *creates*
//!   `config.json`. The message is unconditional too.
//! * **The hints name `stackunderflow`.** `cfg set`'s three rejection messages
//!   embed the reference's own program name as a literal inside the message
//!   body. `parity-cli.sh`'s normalisation is scoped to `Usage:` / `Try '…'`
//!   lines and must stay that way, so these are emitted verbatim — DIV-237.

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::queries::paths::py_repr;
use stax_core::queries::pyjson::{self, Value};

use crate::click::{Output, PROGRAM, UsageError};
use crate::settings::{self, ConfigFile, Default as SettingDefault, Env, PersistError, ProcessEnv};

/// `stax cfg` — view or change persistent settings.
#[derive(Debug, Args)]
pub struct CfgArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: CfgVerb,
}

/// The `cfg` verbs.
#[derive(Debug, Subcommand)]
pub enum CfgVerb {
    /// Show all settings with their sources.
    Ls {
        /// JSON output
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Write KEY=VALUE to the config file.
    Set {
        /// The setting to write.
        key: String,
        /// The value to write.
        value: String,
    },
    /// Remove KEY from the config file.
    Rm {
        /// The setting to remove.
        key: String,
    },
    /// Manage model aliases (proxy → canonical model id).
    #[command(name = "model-alias", subcommand)]
    ModelAlias(ModelAliasVerb),
}

/// The `cfg model-alias` verbs.
#[derive(Debug, Subcommand)]
pub enum ModelAliasVerb {
    /// Map SOURCE (proxy id) → TARGET (canonical id) for cost lookup.
    Set {
        /// The proxy-rewritten model id.
        source: String,
        /// The canonical model id.
        target: String,
    },
    /// Remove SOURCE from the alias map.
    Rm {
        /// The proxy-rewritten model id.
        source: String,
    },
    /// List all configured model aliases.
    Ls {
        /// JSON output
        #[arg(long = "json")]
        as_json: bool,
    },
}

/// `stax config` — the hidden back-compat group.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: ConfigVerb,
}

/// The `config` verbs — each one `ctx.invoke`s a `cfg` verb.
#[derive(Debug, Subcommand)]
pub enum ConfigVerb {
    /// Show all settings with their sources.
    ///
    /// `about = ""`: `_cfg_show` has no docstring, so Click prints none.
    #[command(about = "", long_about = None)]
    Show {
        /// JSON output.
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Write KEY=VALUE to the config file.
    ///
    /// `about = ""`: `_cfg_set` has no docstring, so Click prints none.
    #[command(about = "", long_about = None)]
    Set {
        /// The setting to write.
        key: String,
        /// The value to write.
        value: String,
    },
    /// Remove KEY from the config file.
    ///
    /// `about = ""`: `_cfg_unset` has no docstring, so Click prints none.
    #[command(about = "", long_about = None)]
    Unset {
        /// The setting to remove.
        key: String,
    },
}

// ── entry points ─────────────────────────────────────────────────────────────

/// Run a `cfg` verb.
///
/// # Errors
/// Only a filesystem failure on the two writing verbs; Python lets those
/// propagate too.
pub fn run_cfg(args: &CfgArgs) -> Result<Output> {
    let env = ProcessEnv;
    match &args.verb {
        CfgVerb::Ls { as_json } => Ok(cfg_ls(&settings::load(), &env, *as_json)),
        CfgVerb::Set { key, value } => cfg_set(key, value, &env),
        CfgVerb::Rm { key } => cfg_rm(key),
        CfgVerb::ModelAlias(verb) => run_model_alias(verb, &env),
    }
}

/// Run a `config` verb — `ctx.invoke` into the `cfg` equivalent.
///
/// # Errors
/// As the target verb.
pub fn run_config(args: &ConfigArgs) -> Result<Output> {
    let env = ProcessEnv;
    match &args.verb {
        ConfigVerb::Show { as_json } => Ok(cfg_ls(&settings::load(), &env, *as_json)),
        ConfigVerb::Set { key, value } => cfg_set(key, value, &env),
        ConfigVerb::Unset { key } => cfg_rm(key),
    }
}

fn run_model_alias(verb: &ModelAliasVerb, env: &dyn Env) -> Result<Output> {
    match verb {
        ModelAliasVerb::Set { source, target } => model_alias_set(source, target, env),
        ModelAliasVerb::Rm { source } => model_alias_rm(source, env),
        ModelAliasVerb::Ls { as_json } => Ok(model_alias_ls(&settings::load(), env, *as_json)),
    }
}

// ── cfg ls ───────────────────────────────────────────────────────────────────

/// `cfg_ls` — the table, or `json.dumps(get_all(), indent=2)`.
#[must_use]
pub fn cfg_ls(config: &ConfigFile, env: &dyn Env, as_json: bool) -> Output {
    let data = settings::get_all(config, env);
    if as_json {
        let object = Value::Object(
            data.into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        );
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&object)));
    }
    let mut out = String::from("Settings:\n");
    for key in settings::sorted_keys() {
        let spec = settings::spec_of(key).expect("sorted_keys only yields declared keys");
        let value = settings::resolve(spec, config, env);
        let source = settings::source_of(spec, config, env);
        // `json.dumps(val) if isinstance(val, dict) else str(val)` — note that
        // it is `json.dumps` with DEFAULT separators, so a non-empty dict is
        // `{"a": "b"}` and not `{'a': 'b'}`.
        let rendered = match &value {
            Value::Object(_) => pyjson::dumps_default(&value),
            other => settings::py_str_value(other),
        };
        out.push_str(&format!(
            "  {}  {}  [{}]\n",
            pad(key, 34),
            pad(&rendered, 14),
            source.as_str()
        ));
    }
    Output::ok(out)
}

/// `f"{text:<width}"` — pad to `width` *characters*, never truncate.
pub(crate) fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + (width - len));
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', width - len));
    out
}

// ── cfg set ──────────────────────────────────────────────────────────────────

const SET_PATH: &str = "cfg set";
const SET_SPEC: &str = "[OPTIONS] KEY VALUE";

/// `cfg_set` — the three rejections, the two casts, and the read-back echo.
fn cfg_set(key: &str, value: &str, env: &dyn Env) -> Result<Output> {
    let Some(spec) = settings::spec_of(key) else {
        return Ok(Output::usage(
            &UsageError::bad_parameter(
                SET_PATH,
                SET_SPEC,
                "KEY",
                format!(
                    "Unknown key '{key}'. Valid: {}",
                    settings::sorted_keys().join(", ")
                ),
            ),
            PROGRAM,
        ));
    };
    if matches!(spec.default, SettingDefault::Dict) {
        return Ok(Output::usage(
            &UsageError::bad_parameter(
                SET_PATH,
                SET_SPEC,
                "KEY",
                format!(
                    "'{key}' is a structured setting; use a dedicated subcommand \
                     (e.g. ``stax cfg model-alias set FROM TO``)."
                ),
            ),
            PROGRAM,
        ));
    }
    if key.starts_with("plan_") {
        let hint = if key == "plan_alert_thresholds" {
            "stax plan thresholds set 50 75 90"
        } else {
            "stax plan set NAME [--monthly-usd N] [--reset-day D]"
        };
        return Ok(Output::usage(
            &UsageError::bad_parameter(
                SET_PATH,
                SET_SPEC,
                "KEY",
                format!(
                    "'{key}' is part of the plan-budget settings group; \
                     use ``{hint}`` instead."
                ),
            ),
            PROGRAM,
        ));
    }

    // `parsed: Any = value` — a string unless the default is a bool or an int.
    // `isinstance(ref, bool)` runs FIRST because `bool` is a subclass of `int`.
    let parsed = match spec.default {
        SettingDefault::Bool(_) => Value::Bool(matches!(
            value.to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        SettingDefault::Int(_) => match stax_core::queries::pyint::PyInt::parse(value) {
            Some(number) => Value::Int(number.saturating_i64()),
            // DIV-235: `int(value)` raises an *uncaught* `ValueError` here —
            // the `try` below wraps only `persist`. Python prints a traceback
            // and exits 1; a traceback is not portable, so the port prints one
            // line naming the same cause and exits 1 too. Same exit code, same
            // empty stdout, different stderr — and no case row, because a
            // divergence must not be dressed up as agreement.
            None => {
                anyhow::bail!("invalid literal for int() with base 10: {}", py_repr(value))
            }
        },
        _ => Value::Str(value.to_owned()),
    };

    match settings::persist(key, parsed) {
        Ok(()) => {}
        Err(PersistError::Value(message)) => {
            return Ok(Output::usage(
                &UsageError::bad_parameter(SET_PATH, SET_SPEC, "VALUE", message),
                PROGRAM,
            ));
        }
        Err(PersistError::Io(error)) => return Err(error),
    }

    // "Persist may normalise the value … read it back" — and the read-back goes
    // through the FULL chain, so a set environment variable wins over what was
    // just written to disk.
    let final_value = settings::get(key, &settings::load(), env).unwrap_or(Value::Null);
    Ok(Output::ok(format!(
        "  {key} = {}\n",
        settings::py_str_value(&final_value)
    )))
}

// ── cfg rm ───────────────────────────────────────────────────────────────────

fn cfg_rm(key: &str) -> Result<Output> {
    settings::remove(key)?;
    Ok(Output::ok(format!("  {key} removed\n")))
}

// ── cfg model-alias ──────────────────────────────────────────────────────────

/// `dict(s.get("model_aliases") or {})` — the `or` makes an empty dict and a
/// missing key the same thing.
fn alias_map(config: &ConfigFile, env: &dyn Env) -> Vec<(String, Value)> {
    match settings::get("model_aliases", config, env) {
        Some(Value::Object(entries)) => entries,
        _ => Vec::new(),
    }
}

fn model_alias_set(source: &str, target: &str, env: &dyn Env) -> Result<Output> {
    let mut aliases = alias_map(&settings::load(), env);
    match aliases.iter_mut().find(|(key, _)| key == source) {
        Some(slot) => slot.1 = Value::Str(target.to_owned()),
        None => aliases.push((source.to_owned(), Value::Str(target.to_owned()))),
    }
    persist_aliases(aliases)?;
    Ok(Output::ok(format!("  {source} -> {target}\n")))
}

fn model_alias_rm(source: &str, env: &dyn Env) -> Result<Output> {
    let mut aliases = alias_map(&settings::load(), env);
    if !aliases.iter().any(|(key, _)| key == source) {
        // Note: no write at all on a miss, and the id is `repr`-quoted.
        return Ok(Output::ok(format!("  no alias for {}\n", py_repr(source))));
    }
    aliases.retain(|(key, _)| key != source);
    persist_aliases(aliases)?;
    Ok(Output::ok(format!("  {source} removed\n")))
}

fn persist_aliases(aliases: Vec<(String, Value)>) -> Result<()> {
    match settings::persist("model_aliases", Value::Object(aliases)) {
        Ok(()) | Err(PersistError::Value(_)) => Ok(()),
        Err(PersistError::Io(error)) => Err(error),
    }
}

/// `cfg model-alias ls`.
#[must_use]
pub fn model_alias_ls(config: &ConfigFile, env: &dyn Env, as_json: bool) -> Output {
    let aliases = alias_map(config, env);
    if as_json {
        // `sort_keys=True` — the only sorted JSON writer in the group.
        let mut sorted = aliases;
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        return Output::ok(format!(
            "{}\n",
            pyjson::dumps_indent2(&Value::Object(sorted))
        ));
    }
    if aliases.is_empty() {
        return Output::ok("No model aliases configured.\n");
    }
    let width = aliases
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut keys: Vec<&String> = aliases.iter().map(|(key, _)| key).collect();
    keys.sort();
    let mut out = String::from("Model aliases:\n");
    for key in keys {
        let target = aliases
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| settings::py_str_value(value))
            .unwrap_or_default();
        out.push_str(&format!("  {}  ->  {}\n", pad(key, width), target));
    }
    Output::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MapEnv;

    fn env() -> MapEnv {
        MapEnv::default()
    }

    #[test]
    fn ls_text_is_sorted_and_ls_json_is_declaration_order() {
        let config = ConfigFile::default();
        let text = cfg_ls(&config, &env(), false).stdout;
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Settings:");
        assert!(lines[1].starts_with("  auto_browser"));
        assert!(lines[2].starts_with("  auto_reindex_on_ingest"));

        let json = cfg_ls(&config, &env(), true).stdout;
        let keys: Vec<&str> = json
            .lines()
            .filter_map(|line| line.trim().split('"').nth(1))
            .collect();
        assert_eq!(keys.first(), Some(&"port"), "declaration order, not sorted");
    }

    #[test]
    fn the_text_row_is_the_reference_f_string() {
        let text = cfg_ls(&ConfigFile::default(), &env(), false).stdout;
        assert!(
            text.contains("  port                                8081            [default]\n"),
            "{text}"
        );
        // The one row whose value overflows the 14-column field: the padding
        // must push, not truncate.
        assert!(
            text.contains(
                "  proactive_types                     command-cluster,file-risk  [default]\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_empty_dict_setting_renders_as_two_braces() {
        let text = cfg_ls(&ConfigFile::default(), &env(), false).stdout;
        assert!(text.contains("  model_aliases                       {}              [default]\n"));
    }

    #[test]
    fn the_source_column_follows_the_env() {
        let env = MapEnv(vec![("PORT".into(), "3000".into())]);
        let text = cfg_ls(&ConfigFile::default(), &env, false).stdout;
        assert!(text.contains("  port                                3000            [env]\n"));
    }

    #[test]
    fn an_unknown_key_lists_every_valid_one_sorted() {
        let output = cfg_set("bogus", "x", &env()).expect("no io");
        assert_eq!(output.code, 2);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.contains("Error: Invalid value for KEY: Unknown key 'bogus'. Valid: auto_browser, auto_reindex_on_ingest, budget_daily_usd,"));
        assert!(output.stderr.ends_with("proactive_types\n"));
    }

    #[test]
    fn a_dict_key_is_rejected_before_the_plan_check() {
        let output = cfg_set("model_aliases", "x", &env()).expect("no io");
        assert_eq!(output.code, 2);
        assert!(output.stderr.contains(
            "'model_aliases' is a structured setting; use a dedicated subcommand \
             (e.g. ``stax cfg model-alias set FROM TO``)."
        ));
    }

    #[test]
    fn the_plan_hints_split_on_the_thresholds_key() {
        let thresholds = cfg_set("plan_alert_thresholds", "x", &env()).expect("no io");
        assert!(
            thresholds
                .stderr
                .contains("use ``stax plan thresholds set 50 75 90`` instead.")
        );
        let name = cfg_set("plan_name", "x", &env()).expect("no io");
        assert!(
            name.stderr
                .contains("use ``stax plan set NAME [--monthly-usd N] [--reset-day D]`` instead.")
        );
        // …and the *reset-day* key takes the second hint too, not the first.
        let day = cfg_set("plan_reset_day", "3", &env()).expect("no io");
        assert!(day.stderr.contains("stax plan set NAME"));
    }

    #[test]
    fn a_bad_int_is_an_error_not_a_silent_default() {
        // DIV-235: Python's traceback, our one-line message. What must NOT
        // happen is a *successful* write of 8081 or of the string "abc".
        let error = cfg_set("port", "abc", &env()).expect_err("int() raises");
        assert!(format!("{error}").contains("invalid literal for int()"));
    }

    #[test]
    fn model_alias_ls_pads_to_the_widest_source() {
        let config = ConfigFile::default();
        let mut config = config;
        config.insert(
            "model_aliases",
            Value::Object(vec![
                ("zebra-long-name".into(), Value::Str("cc".into())),
                ("a".into(), Value::Str("b".into())),
            ]),
        );
        let text = model_alias_ls(&config, &env(), false).stdout;
        assert_eq!(
            text,
            "Model aliases:\n  a                ->  b\n  zebra-long-name  ->  cc\n"
        );
    }

    #[test]
    fn model_alias_ls_json_sorts_where_the_settings_writer_does_not() {
        let mut config = ConfigFile::default();
        config.insert(
            "model_aliases",
            Value::Object(vec![
                ("zebra".into(), Value::Str("cc".into())),
                ("a".into(), Value::Str("b".into())),
            ]),
        );
        assert_eq!(
            model_alias_ls(&config, &env(), true).stdout,
            "{\n  \"a\": \"b\",\n  \"zebra\": \"cc\"\n}\n"
        );
    }

    #[test]
    fn an_empty_alias_map_has_two_different_renderings() {
        let config = ConfigFile::default();
        assert_eq!(
            model_alias_ls(&config, &env(), false).stdout,
            "No model aliases configured.\n"
        );
        assert_eq!(model_alias_ls(&config, &env(), true).stdout, "{}\n");
    }

    #[test]
    fn padding_never_truncates() {
        assert_eq!(pad("abc", 5), "abc  ");
        assert_eq!(pad("abcdef", 3), "abcdef");
        assert_eq!(
            pad("café", 5),
            "café ",
            "width counts characters, not bytes"
        );
    }
}

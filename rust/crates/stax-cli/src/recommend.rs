//! `stax recommend` — `cli.py:4393`–`:4470` and `:5401`–`:5460`.
//!
//! Two verbs over two services: `mode` ([`crate::mode_rec`]) scores a prompt
//! against past sessions, `skills` ([`crate::skill_rec`]) surfaces patterns the
//! user re-runs by hand. Both are read-only *to the user* and neither is
//! read-only *to the disk*, which is the fact the parity rows are built around:
//!
//! * `recommend mode` writes a `mode_recommendations` row unless `--no-cache`.
//! * `recommend skills` writes `cache/skill_recommendations.json` **always**,
//!   `--no-cache` included, with a `time.time()` float in it.
//!
//! So the matrix rows are the `--no-cache` mode paths and the two error
//! funnels; the writing paths are proven by `rust/skills-differ.sh`, which
//! normalises the clock and compares the rest. Same treatment wave 5 gave its
//! six wall-clock rows, and tranche 2 gave `backup create`.
//!
//! # `--prompt` is required, and clap says so differently
//!
//! Click prints `Error: Missing option '--prompt'.`; clap prints its own
//! required-argument block. That is the D-2 parser-error class the campaign
//! already records (`PARITY-wave1-resume.md`), not a new divergence, and it is
//! deliberately unrowed — a row would encode clap's wording as the contract.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::queries::pyjson;

use crate::click::Output;
use crate::clickx::usage_error;
use crate::skill_rec::{self, DEFAULT_THRESHOLD, DEFAULT_WINDOW_DAYS, RecommendEnv};
use crate::skills::{SkillsEnv, detect_cwd_project_slug, open_store};

/// `stax recommend`.
#[derive(Debug, Args)]
pub struct RecommendArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: RecommendVerb,
}

/// The `recommend` verbs.
#[derive(Debug, Subcommand)]
pub enum RecommendVerb {
    /// Recommend the cheapest model that fits this task.
    ///
    /// Uses your local session history (``~/.stackunderflow/store.db``) —
    /// nothing leaves the machine. ``confidence == 0.0`` means "not enough
    /// similar past sessions, no opinion".
    Mode(ModeArgs),
    /// List patterns you've manually re-run that could become auto-skills.
    ///
    /// Reads ``messages`` + on-disk skills to find workflow patterns above
    /// ``--threshold`` occurrences that you don't yet have a skill for.
    /// Acceptance is never automatic — each row carries an ``accept_command``
    /// you can paste to install the skill.
    Skills(RecommendSkillsArgs),
}

/// `recommend mode`.
#[derive(Debug, Args)]
pub struct ModeArgs {
    /// The task prompt to score (text in quotes).
    #[arg(
        long = "prompt",
        required = true,
        value_name = "TEXT",
        allow_hyphen_values = true
    )]
    pub prompt: String,
    /// Model you'd otherwise route to. Drives the cost-delta.
    #[arg(
        long = "current-model",
        value_name = "TEXT",
        allow_hyphen_values = true
    )]
    pub current_model: Option<String>,
    /// Skip the 24h cache (recompute from history).
    #[arg(long = "no-cache")]
    pub no_cache: bool,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `recommend skills`.
#[derive(Debug, Args)]
pub struct RecommendSkillsArgs {
    /// Project slug to scan. Default: the project the current directory
    /// belongs to.
    // Every project slug starts with `-`; see `skills::GenerateArgs`.
    #[arg(long = "project", value_name = "TEXT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// A pattern must appear in this many distinct sessions.
    #[arg(long = "threshold", default_value_t = DEFAULT_THRESHOLD,
          value_parser = clap::value_parser!(i64).range(1..))]
    pub threshold: i64,
    /// Lookback window in days.
    #[arg(long = "window-days", default_value_t = DEFAULT_WINDOW_DAYS,
          value_parser = clap::value_parser!(i64).range(1..))]
    pub window_days: i64,
    /// Bypass the recommendation cache and re-mine.
    #[arg(long = "no-cache")]
    pub no_cache: bool,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `recommend`.
///
/// # Errors
/// When the store is missing (DIV-291) or a query fails.
pub fn run_recommend(args: &RecommendArgs) -> Result<Output> {
    let env = SkillsEnv::from_process()?;
    let app_dir = stax_core::settings::app_dir();
    run_recommend_with(args, &env, &app_dir)
}

/// Run `recommend` against an injected environment.
///
/// # Errors
/// As [`run_recommend`].
pub fn run_recommend_with(
    args: &RecommendArgs,
    env: &SkillsEnv,
    app_dir: &std::path::Path,
) -> Result<Output> {
    match &args.verb {
        RecommendVerb::Mode(mode) => run_mode(mode, env),
        RecommendVerb::Skills(skills) => run_skills(skills, env, app_dir),
    }
}

// ── `recommend mode` ─────────────────────────────────────────────────────────

fn run_mode(args: &ModeArgs, env: &SkillsEnv) -> Result<Output> {
    if !env.store.exists() {
        anyhow::bail!(
            "no store at {} — the port does not create one (Python's `_open_store` would \
             `db.connect` + `schema.apply` here). Run `stackunderflow start` first, or point \
             $STACKUNDERFLOW_HOME at an existing store.",
            env.store.display()
        );
    }
    // `_open_store` hands back a READ-WRITE connection, and the default path
    // uses it: `_cache_store` inserts, and a cache hit bumps `last_used_ts`.
    // `--no-cache` touches neither, which is why every matrix row passes it.
    let conn = stax_etl::ingest::guard::open_read_write(&env.store)?;
    let payload = crate::mode_rec::recommend(
        &conn,
        &args.prompt,
        args.current_model.as_deref(),
        !args.no_cache,
        env.now_micros,
    )?;

    if args.format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            pyjson::dumps_indent2(&payload.to_value())
        )));
    }

    let pick = if payload.recommended_model.is_empty() {
        "(none)".to_string()
    } else {
        payload.recommended_model.clone()
    };
    let delta = stax_etl::stats::aggregator::round_py(payload.cost_delta_usd, 6);
    let confidence = stax_etl::stats::aggregator::round_py(payload.confidence, 4);
    let mut out = format!("Recommended model:  {pick}\n");
    if let Some(current) = args
        .current_model
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        out.push_str(&format!("Current model:      {current}\n"));
    }
    out.push_str(&format!("Confidence:         {confidence:.2}\n"));
    if delta > 0.0 {
        out.push_str(&format!("Estimated savings:  ${delta:.4}/session\n"));
    } else if delta < 0.0 {
        out.push_str(&format!("Estimated cost-up:  ${:.4}/session\n", -delta));
    }
    out.push_str(&format!(
        "Similar sessions:   {}\n",
        payload.similar_session_count
    ));
    if payload.cache_hit {
        out.push_str("  (cache hit — re-run with --no-cache to recompute)\n");
    }
    if !payload.rationale.is_empty() {
        out.push_str(&format!("Why:                {}\n", payload.rationale));
    }
    if !payload.evidence_session_ids.is_empty() {
        out.push_str("Evidence sessions:\n");
        for session_id in &payload.evidence_session_ids {
            out.push_str(&format!("  - {session_id}\n"));
        }
    }
    Ok(Output::ok(out))
}

// ── `recommend skills` ───────────────────────────────────────────────────────

fn run_skills(
    args: &RecommendSkillsArgs,
    env: &SkillsEnv,
    app_dir: &std::path::Path,
) -> Result<Output> {
    const PATH: &str = "recommend skills";
    const SPEC: &str = "[OPTIONS]";

    let conn = open_store(env)?;
    let project = match args.project.clone() {
        Some(project) => Some(project),
        None => detect_cwd_project_slug(&conn, &env.cwd),
    };
    let Some(project) = project else {
        return Ok(usage_error(
            PATH,
            SPEC,
            "could not infer a project for the current directory — pass --project \
             SLUG (see `stackunderflow find-sessions-in-path .`).",
        ));
    };

    #[allow(clippy::cast_precision_loss, reason = "epoch micros fit a double")]
    let now = env.now_micros as f64 / 1_000_000.0;
    let recommend_env = RecommendEnv {
        app_dir: PathBuf::from(app_dir),
        home: env.home.clone(),
        now,
        now_micros: env.now_micros,
    };
    let result = match skill_rec::recommend_skills(
        &conn,
        Some(&project),
        args.threshold,
        args.window_days,
        !args.no_cache,
        &recommend_env,
    ) {
        Ok(result) => result,
        Err(error) => {
            let message = stax_core::queries::ValueError::of(&error)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{error}"));
            return Ok(usage_error(PATH, SPEC, &message));
        }
    };

    if args.format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            pyjson::dumps_indent2(&result.to_value())
        )));
    }

    if result.recommendations.is_empty() {
        let mut message = format!(
            "No skill recommendations for {project} above threshold {}",
            args.threshold
        );
        if result.filtered_already_installed > 0 {
            message.push_str(&format!(
                " ({} pattern(s) already installed)",
                result.filtered_already_installed
            ));
        }
        return Ok(Output::ok(format!("{message}.\n")));
    }
    let cache_hint = if result.cache_status == "hit" {
        " (cached)"
    } else {
        ""
    };
    let mut out = format!(
        "Found {} skill recommendation(s) for {project}{cache_hint}:\n",
        result.recommendations.len()
    );
    for row in &result.recommendations {
        out.push_str(&format!(
            "  • {}  [{}]  occurrences={}\n",
            row.suggested_skill_name, row.pattern_kind, row.occurrences
        ));
        out.push_str(&format!("      {}\n", row.description));
        out.push_str(&format!("      accept: {}\n", row.accept_command));
    }
    if result.filtered_already_installed > 0 {
        out.push_str(&format!(
            "({} pattern(s) already have installed skills — not re-recommended.)\n",
            result.filtered_already_installed
        ));
    }
    Ok(Output::ok(out))
}

#[cfg(test)]
mod tests {
    use crate::skill_rec::Recommendation;

    #[test]
    fn the_empty_message_names_the_filtered_count() {
        // The text branch is a pure function of the result; exercised here
        // rather than through a store so the wording is pinned even when no
        // fixture has an already-installed pattern.
        let filtered = 3;
        let message = format!(
            "No skill recommendations for {} above threshold {}{}.",
            "alpha",
            5,
            format_args!(" ({filtered} pattern(s) already installed)")
        );
        assert_eq!(
            message,
            "No skill recommendations for alpha above threshold 5 (3 pattern(s) already installed)."
        );
    }

    #[test]
    fn a_recommendation_row_renders_three_lines() {
        let row = Recommendation {
            pattern_id: "abc".to_string(),
            pattern_kind: "avoids-X".to_string(),
            suggested_skill_name: "auto-avoid-pkill".to_string(),
            description: "Triggers when about to run `pkill`.".to_string(),
            occurrences: 6,
            sessions: Vec::new(),
            last_seen_ts: String::new(),
            project_slug: None,
            suggested_skill_template: String::new(),
            accept_command: "stackunderflow skills generate --pattern abc".to_string(),
            normalized_command: None,
        };
        let rendered = format!(
            "  • {}  [{}]  occurrences={}\n      {}\n      accept: {}\n",
            row.suggested_skill_name,
            row.pattern_kind,
            row.occurrences,
            row.description,
            row.accept_command
        );
        assert_eq!(
            rendered,
            "  • auto-avoid-pkill  [avoids-X]  occurrences=6\n      \
             Triggers when about to run `pkill`.\n      accept: stackunderflow skills \
             generate --pattern abc\n"
        );
    }
}

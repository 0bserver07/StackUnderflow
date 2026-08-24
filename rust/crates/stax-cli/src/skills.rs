//! `stax skills` — `cli.py:4228`–`:4392` over [`crate::skill_synth`].
//!
//! Three verbs: `generate` mines the store and writes `SKILL.md` files, `list`
//! reports the generated ones already on disk, `clean` removes them. The
//! interesting parts are all in the seams:
//!
//! * **`clean` previews unless you say `--yes`.** `preview = dry_run or not
//!   assume_yes`, so the *default* invocation deletes nothing and prints
//!   "(preview — re-run with --yes to delete)" — but only when `--dry-run` was
//!   absent, because `--dry-run` gets its own wording (none). Two flags, four
//!   combinations, four different outputs; all four are rowed.
//! * **`--window all` means "no bound".** `("", "all", "none")` after a
//!   `strip().lower()` all map to `since=None`. Everything else goes through
//!   `parse_since`, whose `ValueError` becomes a `UsageError` (not a
//!   `BadParameter`) — a different rendering, and `S-skl-gen-window-bad` pins
//!   it.
//! * **`--out` is used verbatim.** No `resolve()`, no `expanduser()`: the
//!   printed directory is exactly what was typed, and a relative `--out` is
//!   relative to the process cwd.
//!
//! # The store is opened, not created (DIV-291)
//!
//! `cli.py`'s `_open_store` is `db.connect(...)` + `schema.apply(...)`: on a
//! machine with no store it *creates* one and migrates it. This port refuses,
//! exactly as [`crate::status`] does for the same reason (DIV-239): a CLI verb
//! that silently materialises a 500 KB database is a side effect a read verb
//! should not have, and hiding the difference behind a fabricated "No patterns"
//! line would make the divergence invisible to the harness. Every parity row
//! therefore runs on a seeded home whose store already exists.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use rusqlite::Connection;
use stax_core::queries::pyjson::{self, Value};
use stax_core::queries::pytime;

use crate::click::{Output, UsageError};
use crate::clickx::usage_error;
use crate::skill_synth::{
    self, ALL_PATTERN_KINDS, DEFAULT_MIN_OCCURRENCES, DEFAULT_WINDOW, SkillCandidate,
};

/// `stax skills` — generate / list / clean project-specific skills.
#[derive(Debug, Args)]
pub struct SkillsArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: SkillsVerb,
}

/// The `skills` verbs.
#[derive(Debug, Subcommand)]
pub enum SkillsVerb {
    /// Mine session patterns and emit auto-generated SKILL.md files.
    Generate(GenerateArgs),
    /// List the auto-generated skills present in the skills directory.
    List(ListArgs),
    /// Remove auto-generated skills (never touches hand-authored ones).
    Clean(CleanArgs),
}

/// `skills generate`.
#[derive(Debug, Args)]
pub struct GenerateArgs {
    /// Project slug to mine. Default: the project the current directory
    /// belongs to (for --scope project).
    // `allow_hyphen_values`: every project slug starts with `-` (it is the
    // absolute path with each non-alphanumeric character replaced), so without
    // this clap reads the slug as a flag and Click reads it as a value —
    // a divergence on the single most common way this verb is invoked.
    #[arg(long = "project", value_name = "TEXT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Comma-separated slugs for cross-project mining (required for --scope
    /// user when --project is not given).
    #[arg(long = "projects", value_name = "TEXT", allow_hyphen_values = true)]
    pub projects: Option<String>,
    /// project → ./.claude/skills/ ; user → ~/.claude/skills/ (global;
    /// requires explicit --project/--projects).
    #[arg(long = "scope", default_value = "project", value_parser = ["project", "user"])]
    pub scope: String,
    /// A pattern must appear in this many distinct sessions.
    #[arg(long = "min-occurrences", default_value_t = DEFAULT_MIN_OCCURRENCES,
          value_parser = clap::value_parser!(i64).range(1..))]
    pub min_occurrences: i64,
    /// Restrict to these pattern kinds (repeatable). Default: all.
    #[arg(long = "kind", value_name = "KIND", value_parser = ALL_PATTERN_KINDS)]
    pub kinds: Vec<String>,
    /// Only consider sessions newer than this ('90d'/'1w'/ISO; 'all' or empty
    /// for no bound).
    #[arg(long = "window", default_value = DEFAULT_WINDOW, allow_hyphen_values = true)]
    pub window: String,
    /// Output directory. Default depends on --scope.
    #[arg(long = "out", value_name = "DIRECTORY", allow_hyphen_values = true)]
    pub out_path: Option<PathBuf>,
    /// Show what would be generated; write nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `skills list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Where to look: ./.claude/skills/ or ~/.claude/skills/.
    #[arg(long = "scope", default_value = "project", value_parser = ["project", "user"])]
    pub scope: String,
    /// Skills directory to inspect. Default depends on --scope.
    #[arg(long = "out", value_name = "DIRECTORY", allow_hyphen_values = true)]
    pub out_path: Option<PathBuf>,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `skills clean`.
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Where to clean: ./.claude/skills/ or ~/.claude/skills/.
    #[arg(long = "scope", default_value = "project", value_parser = ["project", "user"])]
    pub scope: String,
    /// Skills directory to clean. Default depends on --scope.
    #[arg(long = "out", value_name = "DIRECTORY", allow_hyphen_values = true)]
    pub out_path: Option<PathBuf>,
    /// Only remove skills generated before this ('30d'/'2w'/ISO). Default:
    /// remove all auto-generated skills.
    #[arg(long = "older-than", value_name = "TEXT", allow_hyphen_values = true)]
    pub older_than: Option<String>,
    /// Show what would be removed; delete nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Actually delete. Without this, clean only previews.
    #[arg(long = "yes", short = 'y')]
    pub assume_yes: bool,
}

/// Everything the verbs read from the process, injected.
#[derive(Debug, Clone)]
pub struct SkillsEnv {
    /// `Path.cwd()`.
    pub cwd: PathBuf,
    /// `Path.home()`.
    pub home: Option<PathBuf>,
    /// `deps.store_path`.
    pub store: PathBuf,
    /// `datetime.now(UTC)` as epoch microseconds.
    pub now_micros: i64,
}

impl SkillsEnv {
    /// Resolve from the real process environment.
    ///
    /// # Errors
    /// When the working directory cannot be read.
    pub fn from_process() -> Result<Self> {
        Ok(Self {
            cwd: std::env::current_dir()?,
            home: stax_core::queries::paths::home_dir(),
            store: stax_core::settings::store_path(),
            now_micros: pytime::now_micros(),
        })
    }
}

/// Run `skills`.
///
/// # Errors
/// When the store is missing (DIV-291) or a filesystem write fails.
pub fn run_skills(args: &SkillsArgs) -> Result<Output> {
    run_skills_with(args, &SkillsEnv::from_process()?)
}

/// Run `skills` against an injected environment.
///
/// # Errors
/// As [`run_skills`].
pub fn run_skills_with(args: &SkillsArgs, env: &SkillsEnv) -> Result<Output> {
    match &args.verb {
        SkillsVerb::Generate(generate) => run_generate(generate, env),
        SkillsVerb::List(list) => Ok(run_list(list, env)),
        SkillsVerb::Clean(clean) => run_clean(clean, env),
    }
}

/// `_default_skills_out`.
#[must_use]
pub fn default_skills_out(scope: &str, env: &SkillsEnv) -> PathBuf {
    if scope == "user" {
        env.home
            .clone()
            .unwrap_or_default()
            .join(".claude")
            .join("skills")
    } else {
        env.cwd.join(".claude").join("skills")
    }
}

/// `_split_csv`.
#[must_use]
pub fn split_csv(value: Option<&str>) -> Option<Vec<String>> {
    let value = value.filter(|text| !text.is_empty())?;
    let out: Vec<String> = value
        .split(',')
        .map(|part| part.trim_matches(|ch: char| ch.is_whitespace()).to_string())
        .filter(|part| !part.is_empty())
        .collect();
    if out.is_empty() { None } else { Some(out) }
}

/// `_detect_cwd_project_slug`.
///
/// # Errors
/// Never — `except Exception: return None` covers the whole query, and so does
/// this.
#[must_use]
pub fn detect_cwd_project_slug(conn: &Connection, cwd: &Path) -> Option<String> {
    let cwd_text = cwd.to_string_lossy().to_string();
    let cwd_slug: String = cwd_text
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut statement = conn
        .prepare("SELECT DISTINCT slug, path FROM projects")
        .ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .ok()?
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let mut best_slug: Option<String> = None;
    let mut best_score: i64 = -1;
    for (slug, path) in rows {
        let Some(slug) = slug.filter(|value| !value.is_empty()) else {
            continue;
        };
        let (matched, score) = match path.filter(|value| !value.is_empty()) {
            Some(path) => {
                let anchor = path.trim_end_matches('/');
                (
                    cwd_text == anchor || cwd_text.starts_with(&format!("{anchor}/")),
                    i64::try_from(anchor.chars().count()).unwrap_or(i64::MAX),
                )
            }
            None => (
                cwd_slug == slug || cwd_slug.starts_with(&format!("{slug}-")),
                i64::try_from(slug.chars().count()).unwrap_or(i64::MAX),
            ),
        };
        if matched && score > best_score {
            best_score = score;
            best_slug = Some(slug);
        }
    }
    best_slug
}

/// `_open_store`, minus the create-and-migrate half (DIV-291).
///
/// # Errors
/// When no store exists at the resolved path, or SQLite refuses the file.
pub fn open_store(env: &SkillsEnv) -> Result<Connection> {
    if !env.store.exists() {
        anyhow::bail!(
            "no store at {} — the port does not create one (Python's `_open_store` would \
             `db.connect` + `schema.apply` here). Run `stax start` first, or point \
             $STACKUNDERFLOW_HOME at an existing store.",
            env.store.display()
        );
    }
    // READ-WRITE, as `_open_store` is. Not because these verbs write — they do
    // not — but because a read-only SQLite connection to a WAL database cannot
    // remove the `-shm` / `-wal` files it creates on open, so the case-home diff
    // saw two files Python never left behind. The guard refuses the live dataset
    // by path, which is the campaign's write-side rule (spec §5).
    stax_etl::ingest::guard::open_read_write(&env.store)
}

// ── `skills generate` ────────────────────────────────────────────────────────

fn run_generate(args: &GenerateArgs, env: &SkillsEnv) -> Result<Output> {
    const PATH: &str = "skills generate";
    const SPEC: &str = "[OPTIONS]";

    let projects_list = split_csv(args.projects.as_deref());
    if args.scope == "user" && args.project.is_none() && projects_list.is_none() {
        return Ok(usage_error(
            PATH,
            SPEC,
            "--scope user is global; pass --project SLUG or --projects A,B,C \
             explicitly. There is no implicit all-projects mode.",
        ));
    }
    let window = args
        .window
        .trim_matches(|ch: char| ch.is_whitespace())
        .to_lowercase();
    let window_arg = if matches!(window.as_str(), "" | "all" | "none") {
        None
    } else {
        Some(args.window.as_str())
    };

    let conn = open_store(env)?;
    let mut project = args.project.clone();
    if project.is_none() && projects_list.is_none() {
        project = detect_cwd_project_slug(&conn, &env.cwd);
        if project.is_none() {
            return Ok(usage_error(
                PATH,
                SPEC,
                "could not infer a project for the current directory — pass --project \
                 SLUG (see `stax find-sessions-in-path .`).",
            ));
        }
    }
    let candidates = match skill_synth::synthesize_skills(
        &conn,
        project.as_deref(),
        projects_list.as_deref(),
        args.min_occurrences,
        Some(&args.kinds),
        window_arg,
        env.home.as_deref(),
    ) {
        Ok(candidates) => candidates,
        // `except ValueError as exc: raise click.UsageError(str(exc))` — and
        // `parse_since`'s failure arrives on the same funnel.
        Err(error) => {
            let message = stax_core::queries::ValueError::of(&error)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{error}"));
            return Ok(usage_error(PATH, SPEC, &message));
        }
    };

    let out_dir = args
        .out_path
        .clone()
        .unwrap_or_else(|| default_skills_out(&args.scope, env));
    let results =
        skill_synth::write_skill_files(&candidates, &out_dir, env.now_micros, args.dry_run)?;

    if args.format == "json" {
        let payload = Value::Object(vec![
            ("scope".to_string(), Value::from(args.scope.as_str())),
            (
                "out_dir".to_string(),
                Value::Str(out_dir.to_string_lossy().to_string()),
            ),
            ("dry_run".to_string(), Value::Bool(args.dry_run)),
            (
                "candidates".to_string(),
                Value::Array(candidates.iter().map(SkillCandidate::to_dict).collect()),
            ),
            (
                "written".to_string(),
                Value::Array(
                    results
                        .iter()
                        .map(|result| {
                            Value::Object(vec![
                                ("name".to_string(), Value::from(&result.name)),
                                (
                                    "path".to_string(),
                                    Value::Str(result.path.to_string_lossy().to_string()),
                                ),
                                ("action".to_string(), Value::from(result.action)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        return Ok(Output::ok(format!("{}\n", pyjson::dumps_indent2(&payload))));
    }

    if candidates.is_empty() {
        return Ok(Output::ok(
            "No patterns met the threshold — nothing generated. \
             (Try a lower --min-occurrences or a wider --window.)\n",
        ));
    }
    let verb = if args.dry_run {
        "Would generate"
    } else {
        "Generated"
    };
    let mut out = format!(
        "{verb} {} skill(s) under {}:\n",
        candidates.len(),
        out_dir.display()
    );
    for result in &results {
        out.push_str(&format!(
            "  [{}] {}  ({})\n",
            result.action,
            result.name,
            result.path.display()
        ));
    }
    for candidate in &candidates {
        out.push_str(&format!(
            "    · {}: {}, {} sessions\n",
            candidate.name, candidate.pattern_kind, candidate.evidence_count
        ));
    }
    if args.dry_run {
        out.push_str("(dry run — nothing written)\n");
    }
    Ok(Output::ok(out))
}

// ── `skills list` ────────────────────────────────────────────────────────────

fn run_list(args: &ListArgs, env: &SkillsEnv) -> Output {
    let skills_dir = args
        .out_path
        .clone()
        .unwrap_or_else(|| default_skills_out(&args.scope, env));
    let items = skill_synth::list_generated_skills(&skills_dir);
    if args.format == "json" {
        let payload = Value::Object(vec![
            (
                "skills_dir".to_string(),
                Value::Str(skills_dir.to_string_lossy().to_string()),
            ),
            (
                "skills".to_string(),
                Value::Array(
                    items
                        .iter()
                        .map(skill_synth::GeneratedSkill::to_dict)
                        .collect(),
                ),
            ),
        ]);
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&payload)));
    }
    if items.is_empty() {
        return Output::ok(format!(
            "No auto-generated skills in {}.\n",
            skills_dir.display()
        ));
    }
    let mut out = format!(
        "Auto-generated skills in {}  ({}):\n",
        skills_dir.display(),
        items.len()
    );
    for item in &items {
        out.push_str(&format!(
            "  {}  [{}]  evidence={}  generated={}\n",
            item.name, item.pattern_kind, item.evidence_count, item.generated_at
        ));
        out.push_str(&format!("      {}\n", item.description));
    }
    Output::ok(out)
}

// ── `skills clean` ───────────────────────────────────────────────────────────

fn run_clean(args: &CleanArgs, env: &SkillsEnv) -> Result<Output> {
    let skills_dir = args
        .out_path
        .clone()
        .unwrap_or_else(|| default_skills_out(&args.scope, env));
    let preview = args.dry_run || !args.assume_yes;
    let removed =
        match skill_synth::clean_generated_skills(&skills_dir, args.older_than.as_deref(), preview)
        {
            Ok(removed) => removed,
            Err(error) => {
                // `raise click.BadParameter(str(exc), param_hint="--older-than")`.
                let Some(message) = stax_core::queries::ValueError::of(&error) else {
                    return Err(error);
                };
                return Ok(Output::usage(
                    &UsageError::bad_parameter(
                        "skills clean",
                        "[OPTIONS]",
                        "--older-than",
                        message.to_string(),
                    ),
                    crate::click::PROGRAM,
                ));
            }
        };
    if removed.is_empty() {
        return Ok(Output::ok(format!(
            "No auto-generated skills to remove in {}.\n",
            skills_dir.display()
        )));
    }
    let verb = if preview { "Would remove" } else { "Removed" };
    let mut out = format!(
        "{verb} {} auto-generated skill(s) from {}:\n",
        removed.len(),
        skills_dir.display()
    );
    for path in &removed {
        out.push_str(&format!(
            "  {}\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    if preview && !args.dry_run {
        out.push_str("(preview — re-run with --yes to delete)\n");
    }
    Ok(Output::ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(cwd: &Path) -> SkillsEnv {
        SkillsEnv {
            cwd: cwd.to_path_buf(),
            home: Some(PathBuf::from("/home/tester")),
            store: cwd.join("store.db"),
            now_micros: 1_800_000_000_000_000,
        }
    }

    #[test]
    fn the_default_out_dir_follows_the_scope() {
        let env = env(Path::new("/work/repo"));
        assert_eq!(
            default_skills_out("project", &env),
            Path::new("/work/repo/.claude/skills")
        );
        assert_eq!(
            default_skills_out("user", &env),
            Path::new("/home/tester/.claude/skills")
        );
    }

    #[test]
    fn split_csv_drops_blanks_and_trims() {
        assert_eq!(
            split_csv(Some(" a , b ,, c ")),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(split_csv(Some("")), None);
        assert_eq!(split_csv(Some(" , ")), None);
        assert_eq!(split_csv(None), None);
    }

    #[test]
    fn scope_user_without_a_project_is_a_usage_error() {
        let dir = tempdir();
        let args = SkillsArgs {
            verb: SkillsVerb::Generate(GenerateArgs {
                project: None,
                projects: None,
                scope: "user".to_string(),
                min_occurrences: 5,
                kinds: Vec::new(),
                window: DEFAULT_WINDOW.to_string(),
                out_path: None,
                dry_run: false,
                format: "text".to_string(),
            }),
        };
        let output = run_skills_with(&args, &env(&dir)).expect("no store touch");
        assert_eq!(output.code, 2);
        assert!(output.stderr.ends_with(
            "Error: --scope user is global; pass --project SLUG or --projects A,B,C \
             explicitly. There is no implicit all-projects mode.\n"
        ));
    }

    #[test]
    fn listing_an_absent_directory_says_so() {
        let dir = tempdir();
        let args = SkillsArgs {
            verb: SkillsVerb::List(ListArgs {
                scope: "project".to_string(),
                out_path: Some(dir.join("nope")),
                format: "text".to_string(),
            }),
        };
        let output = run_skills_with(&args, &env(&dir)).expect("read-only");
        assert_eq!(
            output.stdout,
            format!("No auto-generated skills in {}/nope.\n", dir.display())
        );
    }

    #[test]
    fn clean_previews_without_yes_and_deletes_with_it() {
        let dir = tempdir();
        let skills = dir.join("skills");
        let one = skills.join("auto-thing");
        std::fs::create_dir_all(&one).expect("mkdir");
        std::fs::write(
            one.join("SKILL.md"),
            "---\nname: auto-thing\nauto_generated: true\ngenerated_at: 2026-01-01T00:00:00+00:00\n---\n\nbody\n",
        )
        .expect("write");

        let preview = SkillsArgs {
            verb: SkillsVerb::Clean(CleanArgs {
                scope: "project".to_string(),
                out_path: Some(skills.clone()),
                older_than: None,
                dry_run: false,
                assume_yes: false,
            }),
        };
        let output = run_skills_with(&preview, &env(&dir)).expect("preview");
        assert!(
            output
                .stdout
                .starts_with("Would remove 1 auto-generated skill(s) from ")
        );
        assert!(
            output
                .stdout
                .ends_with("(preview — re-run with --yes to delete)\n")
        );
        assert!(one.exists(), "the preview must not delete");

        let deleting = SkillsArgs {
            verb: SkillsVerb::Clean(CleanArgs {
                scope: "project".to_string(),
                out_path: Some(skills),
                older_than: None,
                dry_run: false,
                assume_yes: true,
            }),
        };
        let output = run_skills_with(&deleting, &env(&dir)).expect("delete");
        assert!(
            output
                .stdout
                .starts_with("Removed 1 auto-generated skill(s) from ")
        );
        assert!(!output.stdout.contains("preview"));
        assert!(!one.exists(), "--yes deletes");
    }

    #[test]
    fn a_hand_authored_skill_is_never_removed() {
        let dir = tempdir();
        let skills = dir.join("skills");
        let mine = skills.join("auto-hand-written");
        std::fs::create_dir_all(&mine).expect("mkdir");
        std::fs::write(mine.join("SKILL.md"), "---\nname: mine\n---\n\nbody\n").expect("write");
        let args = SkillsArgs {
            verb: SkillsVerb::Clean(CleanArgs {
                scope: "project".to_string(),
                out_path: Some(skills),
                older_than: None,
                dry_run: false,
                assume_yes: true,
            }),
        };
        let output = run_skills_with(&args, &env(&dir)).expect("clean");
        assert!(
            output
                .stdout
                .starts_with("No auto-generated skills to remove in ")
        );
        assert!(mine.exists());
    }

    /// A scratch directory under the process temp dir, removed by the OS.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "stax-skills-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&base).expect("scratch dir");
        base
    }
}

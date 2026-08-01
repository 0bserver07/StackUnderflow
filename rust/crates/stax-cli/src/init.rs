//! `stax init` — `cli.py:328`–`:408`. Install the shipped skills, then `start`.
//!
//! The docstring calls it "an alias for `start`", and the inventory files it as
//! a separate node with seven of its own parameters; both are true. What it
//! actually is: an optional skills installer followed by `ctx.invoke(start_cmd,
//! …)` with four of `start`'s seven flags spelled differently
//! (`--no-browser` → `headless`, `--clear-cache` → `fresh`) and the other three
//! (`--no-watcher`, `--no-lock`, `--data-dir`) left at their defaults. Nothing
//! about `init` can *not* start the server: the reference's own comment says so
//! ("the user can pipe --no-browser or hit Ctrl-C if they only wanted the
//! install"), which is why this verb has no `parity-cli.sh` row that reaches
//! past the installer — a case row must terminate, and `init` does not.
//!
//! The installer half is proven instead by `rust/init-differ.sh`, which runs
//! both implementations against case-local homes, kills them once the boot line
//! lands, and diffs the destination tree.
//!
//! # The behaviour matrix, and why each leg matters
//!
//! | destination | bytes | `--skills-force` | outcome |
//! |---|---|---|---|
//! | missing | — | — | `created` |
//! | present | equal | — | `unchanged`, silently |
//! | present | differ | off | `skipped_modified` — **a warning on stderr** |
//! | present | differ | on | `overwritten` |
//!
//! The third row is the reason the command is safe to re-run: a user who edited
//! a `SKILL.md` keeps their edit and is told. Reproducing "skip on difference"
//! rather than "always copy" is the whole point of the port.
//!
//! The skill *set* is discovered from the packaged tree — any directory holding
//! a `SKILL.md` — so adding a skill is adding a folder in both implementations,
//! never editing a name list. A tree that is missing entirely degrades to one
//! `missing_source` sentinel named `skills/ tree` rather than crashing the
//! command (and the server start that follows it).

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;

use crate::click::{self, Output, UsageError};
use crate::start::{StartArgs, run_start_with};

/// `stax init [OPTIONS]`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Server port.
    #[arg(long)]
    pub port: Option<i64>,
    /// Bind address.
    #[arg(long)]
    pub host: Option<String>,
    /// Don't open the browser.
    #[arg(long = "no-browser")]
    pub no_browser: bool,
    /// Clear the disk cache first.
    #[arg(long = "clear-cache")]
    pub clear_cache: bool,
    /// Copy every shipped Claude Code skill (discovered from the packaged
    /// skills/ tree) into the skills destination (default ~/.claude/skills/)
    /// before starting the dashboard. Idempotent: byte-identical files are
    /// skipped silently.
    #[arg(long = "install-skills")]
    pub install_skills: bool,
    /// Destination directory for --install-skills. Defaults to
    /// ~/.claude/skills/. Useful for testing and advanced setups where Claude
    /// Code reads skills from a non-standard location.
    #[arg(long = "skills-dest", value_name = "DIRECTORY")]
    pub skills_dest: Option<PathBuf>,
    /// With --install-skills, overwrite destination SKILL.md files that differ
    /// from the shipped copy. Default behaviour preserves local edits — a
    /// modified destination is skipped with a warning.
    #[arg(long = "skills-force")]
    pub skills_force: bool,
}

/// What `_install_static_skills` returns: action → skill names, in the order the
/// reference's five loops print them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SkillsReport {
    /// Destination did not exist.
    pub created: Vec<String>,
    /// Destination differed and `--skills-force` was set.
    pub overwritten: Vec<String>,
    /// Destination matched the shipped bytes.
    pub unchanged: Vec<String>,
    /// Destination differed and `--skills-force` was not set.
    pub skipped_modified: Vec<String>,
    /// The shipped copy is not there — a packaging bug, or no tree at all.
    pub missing_source: Vec<String>,
}

/// Run `init`.
///
/// # Errors
/// A filesystem failure copying a skill, or whatever `start` returns.
pub fn run_init(args: &InitArgs) -> Result<Output> {
    let mut out = String::new();
    let mut err = String::new();

    if let Some(dest) = &args.skills_dest
        && dest.is_file()
    {
        // Click's `Path(file_okay=False)` conversion, at parse time — before the
        // installer and before the delegation.
        let error = UsageError::bad_parameter(
            "init",
            "[OPTIONS]",
            "'--skills-dest'",
            format!("Directory '{}' is a file.", dest.display()),
        );
        return Ok(Output::usage(&error, click::PROGRAM));
    }

    if args.install_skills {
        let dest = args.skills_dest.clone().unwrap_or_else(default_skills_dest);
        let report = install_static_skills(&shipped_skills_source_dir(), &dest, args.skills_force)?;
        let (stdout, stderr) = render_report(&report, &dest);
        out.push_str(&stdout);
        err.push_str(&stderr);
    }

    // `ctx.invoke(start_cmd, port=…, host=…, headless=no_browser, fresh=clear_cache)`
    // — the other three `start` flags keep their declared defaults, which is
    // what `ctx.invoke` does with parameters the caller does not name.
    let start = StartArgs {
        port: args.port,
        host: args.host.clone(),
        headless: args.no_browser,
        fresh: args.clear_cache,
        no_watcher: false,
        no_lock: false,
        data_dir: None,
    };
    if !err.is_empty() {
        // `click.secho(..., err=True)` fires as the loop runs, i.e. before the
        // server's banner. Flushed here for the same reason `start` flushes its
        // own: the boot blocks.
        eprint!("{err}");
    }
    run_start_with(&start, "init", out)
}

/// `~/.claude/skills` — the default `--skills-dest`.
#[must_use]
pub fn default_skills_dest() -> PathBuf {
    #[allow(
        deprecated,
        reason = "`Path.home()` is what the reference calls; \
        std::env::home_dir is its platform-correct equivalent on the 1.97.1 pin"
    )]
    let home = std::env::home_dir().unwrap_or_default();
    home.join(".claude").join("skills")
}

/// `_shipped_skills_source_dir()` — `importlib.resources.files("stackunderflow") / "skills"`.
///
/// Python resolves this through the package so it works from a wheel *and* from
/// an editable checkout. The port has no wheel yet, so it resolves the same
/// compile-time repo layout every other `stackunderflow/`-rooted lookup in this
/// workspace uses; packaging is a wave-10 decision, not an invention here.
#[must_use]
pub fn shipped_skills_source_dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../stackunderflow")
        .join("skills");
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// `_install_static_skills(dest_dir, force=…)`.
///
/// `dest_dir` is created (recursively — `mkdir(parents=True, exist_ok=True)`)
/// *before* the source tree is checked, so a missing packaged tree still leaves
/// the destination behind exactly as the reference does.
///
/// # Errors
/// Any filesystem failure other than the ones the reference tolerates.
pub fn install_static_skills(src_dir: &Path, dest_dir: &Path, force: bool) -> Result<SkillsReport> {
    let mut report = SkillsReport::default();
    std::fs::create_dir_all(dest_dir)?;

    if !src_dir.is_dir() {
        // Missing or non-filesystem skills tree: the names are unknowable, so
        // the reference degrades with a tree-level sentinel.
        report.missing_source.push("skills/ tree".to_owned());
        return Ok(report);
    }

    for name in shipped_names(src_dir)? {
        let src_file = src_dir.join(&name).join("SKILL.md");
        let dst_file = dest_dir.join(&name).join("SKILL.md");

        if !src_file.is_file() {
            report.missing_source.push(name);
            continue;
        }
        if let Some(parent) = dst_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !dst_file.exists() {
            std::fs::copy(&src_file, &dst_file)?;
            report.created.push(name);
            continue;
        }
        if std::fs::read(&src_file)? == std::fs::read(&dst_file)? {
            report.unchanged.push(name);
            continue;
        }
        if force {
            std::fs::copy(&src_file, &dst_file)?;
            report.overwritten.push(name);
        } else {
            report.skipped_modified.push(name);
        }
    }
    Ok(report)
}

/// The discovered skill set: every directory holding a `SKILL.md`, sorted.
fn shipped_names(src_dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        if entry.path().is_dir() && entry.path().join("SKILL.md").is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The five loops the command body runs over the report, in their order.
#[must_use]
pub fn render_report(report: &SkillsReport, dest: &Path) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    let target = |name: &str| dest.join(name).join("SKILL.md").display().to_string();

    for name in &report.created {
        out.push_str(&format!("  + installed skill: {name} → {}\n", target(name)));
    }
    for name in &report.overwritten {
        out.push_str(&format!(
            "  ~ overwrote skill (--skills-force): {name} → {}\n",
            target(name)
        ));
    }
    for name in &report.unchanged {
        out.push_str(&format!("  = skill already current: {name}\n"));
    }
    for name in &report.skipped_modified {
        err.push_str(&format!(
            "  ⚠  skill {name} differs from shipped copy; skipped. \
Re-run with --skills-force to overwrite.\n"
        ));
    }
    for name in &report.missing_source {
        err.push_str(&format!(
            "  ⚠  shipped skill source missing for {name}; this is a \
packaging bug — please file an issue.\n"
        ));
    }
    (out, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-init-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).expect("scratch");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn source_tree(root: &Path, skills: &[(&str, &str)]) -> PathBuf {
        let src = root.join("src");
        for (name, body) in skills {
            std::fs::create_dir_all(src.join(name)).expect("skill dir");
            std::fs::write(src.join(name).join("SKILL.md"), body).expect("skill");
        }
        std::fs::create_dir_all(&src).expect("src");
        src
    }

    #[test]
    fn the_shipped_tree_is_where_the_reference_says_it_is() {
        let src = shipped_skills_source_dir();
        assert!(src.is_dir(), "{} is not a directory", src.display());
        let names = shipped_names(&src).expect("discover");
        assert!(
            names.contains(&"recall-past-decisions".to_owned()),
            "discovered {names:?}"
        );
    }

    #[test]
    fn a_missing_destination_is_created_and_reported() {
        let scratch = Scratch::new("create");
        let src = source_tree(scratch.path(), &[("alpha", "A\n"), ("beta", "B\n")]);
        let dest = scratch.path().join("dest");

        let report = install_static_skills(&src, &dest, false).expect("install");
        assert_eq!(report.created, vec!["alpha", "beta"]);
        assert!(report.unchanged.is_empty());
        assert_eq!(
            std::fs::read_to_string(dest.join("alpha").join("SKILL.md")).expect("copied"),
            "A\n"
        );
    }

    #[test]
    fn a_second_run_is_silent_and_changes_nothing() {
        let scratch = Scratch::new("idempotent");
        let src = source_tree(scratch.path(), &[("alpha", "A\n")]);
        let dest = scratch.path().join("dest");

        install_static_skills(&src, &dest, false).expect("first");
        let report = install_static_skills(&src, &dest, false).expect("second");
        assert_eq!(report.unchanged, vec!["alpha"]);
        assert!(report.created.is_empty());
        let (out, err) = render_report(&report, &dest);
        assert_eq!(out, "  = skill already current: alpha\n");
        assert!(err.is_empty());
    }

    #[test]
    fn a_local_edit_survives_and_is_warned_about() {
        let scratch = Scratch::new("modified");
        let src = source_tree(scratch.path(), &[("alpha", "A\n")]);
        let dest = scratch.path().join("dest");
        install_static_skills(&src, &dest, false).expect("first");
        std::fs::write(dest.join("alpha").join("SKILL.md"), "MINE\n").expect("edit");

        let report = install_static_skills(&src, &dest, false).expect("second");
        assert_eq!(report.skipped_modified, vec!["alpha"]);
        assert_eq!(
            std::fs::read_to_string(dest.join("alpha").join("SKILL.md")).expect("kept"),
            "MINE\n",
            "the default must preserve a local edit"
        );
        let (out, err) = render_report(&report, &dest);
        assert!(out.is_empty());
        assert_eq!(
            err,
            "  ⚠  skill alpha differs from shipped copy; skipped. \
             Re-run with --skills-force to overwrite.\n"
                .replace("             ", "")
        );
    }

    #[test]
    fn force_overwrites_the_edit_and_says_so() {
        let scratch = Scratch::new("force");
        let src = source_tree(scratch.path(), &[("alpha", "A\n")]);
        let dest = scratch.path().join("dest");
        install_static_skills(&src, &dest, false).expect("first");
        std::fs::write(dest.join("alpha").join("SKILL.md"), "MINE\n").expect("edit");

        let report = install_static_skills(&src, &dest, true).expect("forced");
        assert_eq!(report.overwritten, vec!["alpha"]);
        assert_eq!(
            std::fs::read_to_string(dest.join("alpha").join("SKILL.md")).expect("copied"),
            "A\n"
        );
        let (out, _) = render_report(&report, &dest);
        assert!(out.starts_with("  ~ overwrote skill (--skills-force): alpha → "));
    }

    #[test]
    fn a_directory_without_a_skill_md_is_not_a_skill() {
        let scratch = Scratch::new("discovery");
        let src = source_tree(scratch.path(), &[("alpha", "A\n")]);
        std::fs::create_dir_all(src.join("not-a-skill")).expect("noise");
        std::fs::write(src.join("loose.md"), "x").expect("noise");

        let names = shipped_names(&src).expect("discover");
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn a_missing_source_tree_degrades_to_one_sentinel() {
        let scratch = Scratch::new("nosource");
        let dest = scratch.path().join("dest");
        let report =
            install_static_skills(&scratch.path().join("nope"), &dest, false).expect("degrade");
        assert_eq!(report.missing_source, vec!["skills/ tree"]);
        assert!(dest.is_dir(), "the destination is still created");
        let (out, err) = render_report(&report, &dest);
        assert!(out.is_empty());
        assert!(err.contains("shipped skill source missing for skills/ tree"));
        assert!(err.contains("packaging bug"));
    }

    #[test]
    fn the_report_prints_in_the_references_loop_order() {
        let report = SkillsReport {
            created: vec!["c".to_owned()],
            overwritten: vec!["o".to_owned()],
            unchanged: vec!["u".to_owned()],
            skipped_modified: vec!["s".to_owned()],
            missing_source: vec!["m".to_owned()],
        };
        let (out, err) = render_report(&report, Path::new("/d"));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "  + installed skill: c → /d/c/SKILL.md");
        assert_eq!(
            lines[1],
            "  ~ overwrote skill (--skills-force): o → /d/o/SKILL.md"
        );
        assert_eq!(lines[2], "  = skill already current: u");
        let errors: Vec<&str> = err.lines().collect();
        assert!(errors[0].starts_with("  ⚠  skill s differs"));
        assert!(errors[1].starts_with("  ⚠  shipped skill source missing for m"));
    }

    #[test]
    fn a_file_skills_dest_is_clicks_quoted_hint() {
        let scratch = Scratch::new("destfile");
        let file = scratch.path().join("afile");
        std::fs::write(&file, "x").expect("file");
        let args = InitArgs {
            port: None,
            host: None,
            no_browser: true,
            clear_cache: false,
            install_skills: false,
            skills_dest: Some(file.clone()),
            skills_force: false,
        };
        let output = run_init(&args).expect("usage error, not a boot");
        assert_eq!(output.code, 2);
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            format!(
                concat!(
                    "Usage: stax init [OPTIONS]\n",
                    "Try 'stax init --help' for help.\n",
                    "\n",
                    "Error: Invalid value for '--skills-dest': Directory '{}' is a file.\n",
                ),
                file.display()
            )
        );
    }
}

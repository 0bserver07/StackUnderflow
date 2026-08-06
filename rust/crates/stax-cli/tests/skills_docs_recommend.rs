//! Wave-8 tranche 4, end to end: the wiring the unit tests cannot see.
//!
//! The per-module tests pin the pure functions (shlex, the tie-breaks, the MD5,
//! the cache TTL). These run the *verbs* against the same synthetic store the
//! parity harness uses — `rust/parity/homes/skills-corpus`, built by
//! `rust/parity/build_skills_state.py` — so a regression in the seam between
//! the CLI shell, the miner and the filesystem is caught here rather than by
//! the differ two steps later.
//!
//! The seed is COPIED first, always: it is committed state, and
//! `skills::open_store` opens read-write (as `cli.py`'s `_open_store` does).

use std::path::{Path, PathBuf};

use stax_cli::{
    CleanArgs, DocsArgs, DocsVerb, GenerateArgs, ListArgs, SkillsArgs, SkillsEnv, SkillsVerb,
    run_docs_with, run_skills_with,
};

/// `rust/parity/homes/<name>` — the committed seed.
fn seed(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../parity/homes")
        .join(name)
}

/// The repository's `stackunderflow/adapters/capabilities.json`.
fn capabilities() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/adapters/capabilities.json")
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "stax-t4-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&base).expect("scratch dir");
    base
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read seed") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// A home seeded from `name`, with the env every verb reads.
fn home(tag: &str, name: &str) -> (PathBuf, SkillsEnv) {
    let dir = scratch(tag);
    copy_tree(&seed(name), &dir);
    let env = SkillsEnv {
        cwd: dir.clone(),
        home: Some(dir.clone()),
        store: dir.join("store.db"),
        // A pinned clock: `render_skill_md` stamps it into every file, and the
        // point of these tests is that nothing else moves.
        now_micros: 1_800_000_000_500_000,
    };
    (dir, env)
}

const PROJECT: &str = "-tmp-stax-skills-parity-proj";

fn generate(dry_run: bool, format: &str) -> SkillsArgs {
    SkillsArgs {
        verb: SkillsVerb::Generate(GenerateArgs {
            project: Some(PROJECT.to_string()),
            projects: None,
            scope: "project".to_string(),
            min_occurrences: 5,
            kinds: Vec::new(),
            window: "all".to_string(),
            out_path: None,
            dry_run,
            format: format.to_string(),
        }),
    }
}

#[test]
fn the_miner_finds_four_patterns_in_the_synthetic_corpus() {
    let (_dir, env) = home("mine", "skills-corpus");
    let output = run_skills_with(&generate(true, "text"), &env).expect("mines");
    assert_eq!(output.code, 0, "{}", output.stderr);
    assert!(
        output
            .stdout
            .starts_with("Would generate 4 skill(s) under "),
        "{}",
        output.stdout
    );
    for name in [
        "auto-canonical-test-command",
        "auto-run-ruff-check-fix-after-edits",
        "auto-avoid-pkill",
        "auto-never-touch-config-json",
    ] {
        assert!(
            output.stdout.contains(name),
            "missing {name}: {}",
            output.stdout
        );
    }
    // Every detector fired, and the merge collapsed the flag-combo candidate
    // into the canonical-test one (same normalized command, higher priority).
    assert!(
        !output.stdout.contains("auto-flags-pytest"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.ends_with("(dry run — nothing written)\n"));
}

#[test]
fn a_dry_run_writes_nothing_and_a_real_run_writes_once() {
    let (dir, env) = home("write", "skills-corpus");
    let skills = dir.join(".claude/skills");

    run_skills_with(&generate(true, "text"), &env).expect("dry run");
    assert!(!skills.exists(), "--dry-run created {}", skills.display());

    let output = run_skills_with(&generate(false, "text"), &env).expect("write");
    assert!(output.stdout.starts_with("Generated 4 skill(s) under "));
    let file = skills.join("auto-canonical-test-command/SKILL.md");
    let text = std::fs::read_to_string(&file).expect("the file");
    assert!(text.starts_with("---\nname: auto-canonical-test-command\n"));
    assert!(text.contains("auto_generated: true\n"));
    assert!(
        text.contains("generated_at: 2027-01-15T08:00:00+00:00\n"),
        "{text}"
    );
    assert!(text.contains(&format!("generated_from: 6 sessions in {PROJECT}\n")));
    assert!(text.contains("pattern_id: 34df997a879835bc\n"), "{text}");
    assert!(text.contains("```bash\npytest tests/ -q\n```"));

    // The second pass is `unchanged`: same bytes, no `.bak`, no rewrite.
    let output = run_skills_with(&generate(false, "text"), &env).expect("second write");
    assert!(
        output
            .stdout
            .contains("[unchanged] auto-canonical-test-command")
    );
    assert!(
        !skills
            .join("auto-canonical-test-command/SKILL.md.bak")
            .exists(),
        "an unchanged file must not be backed up"
    );
    assert_eq!(std::fs::read_to_string(&file).expect("still there"), text);
}

#[test]
fn a_hand_authored_skill_is_never_clobbered_and_a_collision_gets_a_suffix() {
    let (dir, env) = home("collide", "skills-both");
    let output = run_skills_with(&generate(false, "text"), &env).expect("write");
    let stdout = output.stdout;

    // `auto-never-touch-config-json` in the seed has no `auto_generated` flag.
    assert!(
        stdout.contains("[skipped-user-authored] auto-never-touch-config-json"),
        "{stdout}"
    );
    // `auto-avoid-pkill` in the seed IS ours, same pattern_id, different body.
    assert!(stdout.contains("[updated] auto-avoid-pkill"), "{stdout}");
    assert!(
        dir.join(".claude/skills/auto-avoid-pkill/SKILL.md.bak")
            .exists(),
        "an update must back the prior file up"
    );
    // `auto-canonical-test-command` in the seed is ours with a DIFFERENT
    // pattern_id, so the new one takes the `-<hash6>` suffix rather than
    // silently replacing a skill mined from another pattern.
    assert!(
        stdout.contains("auto-canonical-test-command-34df99"),
        "{stdout}"
    );
    assert!(
        std::fs::read_to_string(dir.join(".claude/skills/auto-never-touch-config-json/SKILL.md"))
            .expect("untouched")
            .contains("hand written")
    );
}

#[test]
fn list_reports_only_our_files_and_clean_removes_only_those() {
    let (dir, env) = home("listclean", "skills-installed");
    let list = SkillsArgs {
        verb: SkillsVerb::List(ListArgs {
            scope: "project".to_string(),
            out_path: None,
            format: "text".to_string(),
        }),
    };
    let output = run_skills_with(&list, &env).expect("list");
    assert!(output.stdout.starts_with("Auto-generated skills in "));
    for name in ["auto-old-thing", "auto-recent-thing", "auto-undated"] {
        assert!(output.stdout.contains(name), "{}", output.stdout);
    }
    // `auto-hand-written` has our prefix but not our marker; `handwritten` has
    // our marker but not our prefix. Neither is ours.
    assert!(!output.stdout.contains("auto-hand-written"));
    assert!(!output.stdout.contains("\n  handwritten"));

    let clean = |older: Option<&str>, yes: bool| SkillsArgs {
        verb: SkillsVerb::Clean(CleanArgs {
            scope: "project".to_string(),
            out_path: None,
            older_than: older.map(ToString::to_string),
            dry_run: false,
            assume_yes: yes,
        }),
    };
    let output = run_skills_with(&clean(None, false), &env).expect("preview");
    assert!(
        output
            .stdout
            .starts_with("Would remove 3 auto-generated skill(s) from ")
    );
    assert!(dir.join(".claude/skills/auto-old-thing").exists());

    // `--older-than` keeps the future-stamped one AND the undated one (the
    // conservative branch), so only the 2026 skill goes.
    let output = run_skills_with(&clean(Some("30d"), true), &env).expect("clean");
    assert!(
        output
            .stdout
            .starts_with("Removed 1 auto-generated skill(s) from ")
    );
    assert!(!dir.join(".claude/skills/auto-old-thing").exists());
    assert!(dir.join(".claude/skills/auto-recent-thing").exists());
    assert!(dir.join(".claude/skills/auto-undated").exists());
    assert!(dir.join(".claude/skills/auto-hand-written").exists());
}

#[test]
fn the_support_matrix_page_is_rendered_from_the_capability_table() {
    let args = DocsArgs {
        verb: DocsVerb::Show {
            topic: "support-matrix".to_string(),
            as_json: false,
        },
    };
    let output = run_docs_with(&args, &capabilities()).expect("renders");
    assert_eq!(output.code, 0);
    assert!(output.stdout.starts_with("# Adapter support matrix\n"));
    assert!(
        output
            .stdout
            .contains("| provider | status | content_text |")
    );
    assert!(output.stdout.contains("| `claude` | supported |"));
    assert!(output.stdout.contains("\n## Fields\n"));
    assert!(output.stdout.contains("\n## Notes\n"));
    assert!(output.stdout.ends_with('\n'));
    assert!(!output.stdout.ends_with("\n\n"));
}

#[test]
fn recommend_mode_picks_the_cheaper_model_from_the_corpus() {
    let (_dir, env) = home("mode", "skills-corpus");
    let conn = rusqlite::Connection::open(&env.store).expect("open");
    let answer = stax_cli::mode_rec::recommend(
        &conn,
        "fix the failing test in cost.py with pytest",
        Some("spendy-model"),
        false,
        env.now_micros,
    )
    .expect("recommends");
    assert_eq!(answer.recommended_model, "cheap-model");
    assert_eq!(answer.similar_session_count, 6);
    assert_eq!(answer.evidence_session_ids.len(), 3);
    assert!(!answer.cache_hit);
    assert!(
        (answer.cost_delta_usd - 0.99).abs() < 1e-9,
        "{}",
        answer.cost_delta_usd
    );
    assert!(answer.confidence > 0.8, "{}", answer.confidence);
    assert_eq!(answer.features.intent, "fix");
    assert_eq!(answer.features.languages, ["python"]);

    // `--no-cache` neither reads nor writes the table.
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM mode_recommendations", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(rows, 0, "--no-cache must not write a cache row");
}

#[test]
fn recommend_mode_caches_and_then_reports_the_hit() {
    let (_dir, env) = home("modecache", "skills-corpus");
    let conn = rusqlite::Connection::open(&env.store).expect("open");
    let first = stax_cli::mode_rec::recommend(
        &conn,
        "fix the failing test in cost.py with pytest",
        None,
        true,
        env.now_micros,
    )
    .expect("first");
    assert!(!first.cache_hit);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM mode_recommendations", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(rows, 1);

    let second = stax_cli::mode_rec::recommend(
        &conn,
        "fix the failing test in cost.py with pytest",
        None,
        true,
        env.now_micros + 1_000_000,
    )
    .expect("second");
    assert!(second.cache_hit);
    assert_eq!(second.recommended_model, first.recommended_model);
    assert!(
        second
            .rationale
            .starts_with("cached recommendation for 'fix'-band-'tiny'")
    );

    // 24 hours and one second later it is stale again.
    let third = stax_cli::mode_rec::recommend(
        &conn,
        "fix the failing test in cost.py with pytest",
        None,
        true,
        env.now_micros + 24 * 3_600 * 1_000_000 + 2_000_000,
    )
    .expect("third");
    assert!(!third.cache_hit);
}

#[test]
fn recommend_skills_filters_what_is_already_installed() {
    let (dir, env) = home("recskills", "skills-both");
    let conn = rusqlite::Connection::open(&env.store).expect("open");
    let recommend_env = stax_cli::skill_rec::RecommendEnv {
        app_dir: dir.clone(),
        home: Some(dir.clone()),
        now: 1_800_000_000.5,
        now_micros: env.now_micros,
    };
    let result =
        stax_cli::skill_rec::recommend_skills(&conn, Some(PROJECT), 5, 3650, false, &recommend_env)
            .expect("recommends");
    assert_eq!(result.cache_status, "bypassed");
    // The seed ships `auto-avoid-pkill` with the mined pattern's id.
    assert_eq!(result.filtered_already_installed, 1);
    assert_eq!(result.recommendations.len(), 3);
    assert!(
        result
            .recommendations
            .iter()
            .all(|row| row.suggested_skill_name != "auto-avoid-pkill")
    );
    let first = &result.recommendations[0];
    assert!(
        first
            .accept_command
            .ends_with(&format!("--pattern {}", first.pattern_id))
    );
    assert!(first.suggested_skill_template.starts_with("---\nname: "));

    // Bypassed still WRITES, which is what makes the second call a hit.
    let cache = dir.join("cache/skill_recommendations.json");
    assert!(
        cache.is_file(),
        "the cache is written even under --no-cache"
    );
    let again =
        stax_cli::skill_rec::recommend_skills(&conn, Some(PROJECT), 5, 3650, true, &recommend_env)
            .expect("cached");
    assert_eq!(again.cache_status, "hit");
    assert_eq!(again.recommendations.len(), 3);
}

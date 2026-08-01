//! `backup auto --enable` on Darwin, diffed against the reference's own bytes.
//!
//! The launchd branch cannot execute on this host and, per the tranche-2 brief,
//! must never execute on any host: no `launchctl`, no `~/Library/LaunchAgents`.
//! What is provable is the *generated file*, so `rust/backup-differ.sh` runs the
//! real `cli.py` with `platform.system()` faked to `"Darwin"`, `Path.home()`
//! pointed at a scratch directory and `subprocess.run` stubbed, captures the
//! plist it writes, and hands the path to this test through
//! `STAX_PLIST_GOLDEN`. The comparison is byte for byte.
//!
//! Without those variables the test still runs and still asserts the structural
//! invariants — a golden that is merely absent must not look like a pass, so
//! `rust/backup-differ.sh` fails if this test is skipped rather than run.

use stax_cli::{cron_line, darwin_plist, launchctl_argv};

#[test]
fn the_plist_matches_the_reference_byte_for_byte() {
    let (Ok(golden), Ok(su_bin), Ok(state_dir)) = (
        std::env::var("STAX_PLIST_GOLDEN"),
        std::env::var("STAX_PLIST_BIN"),
        std::env::var("STAX_PLIST_STATE"),
    ) else {
        eprintln!("plist_golden: no STAX_PLIST_* environment — structural checks only");
        return;
    };
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|err| panic!("reading the golden plist {golden}: {err}"));
    let actual = darwin_plist(&su_bin, &state_dir);
    assert_eq!(
        actual, expected,
        "the generated launchd plist differs from the reference's"
    );
}

#[test]
fn the_plist_names_the_label_the_program_and_the_log_twice() {
    let plist = darwin_plist("/usr/local/bin/stackunderflow", "/home/u/.stackunderflow");
    assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
    assert!(
        plist.ends_with("</plist>"),
        "no trailing newline in the source f-string"
    );
    assert!(plist.contains("<string>com.stackunderflow.backup</string>"));
    assert!(plist.contains("<string>/usr/local/bin/stackunderflow</string>"));
    assert_eq!(
        plist.matches("/home/u/.stackunderflow/backup.log").count(),
        2,
        "StandardOutPath and StandardErrorPath both point at the log"
    );
    // The schedule is 03:00 and the retention is 10 — the two numbers the
    // printed line promises ("Daily backup enabled (3:00 AM). Keeps last 10.").
    assert!(plist.contains("<key>Hour</key>\n        <integer>3</integer>"));
    assert!(plist.contains("<string>--keep</string>\n        <string>10</string>"));
}

#[test]
fn the_cron_line_carries_the_same_schedule_and_retention() {
    assert_eq!(
        cron_line("/usr/local/bin/stackunderflow"),
        "0 3 * * * /usr/local/bin/stackunderflow backup create --label auto --keep 10"
    );
}

#[test]
fn the_launchctl_argv_is_the_three_words_the_reference_passes() {
    assert_eq!(
        launchctl_argv(
            "load",
            std::path::Path::new("/x/com.stackunderflow.backup.plist")
        ),
        vec![
            "launchctl".to_owned(),
            "load".to_owned(),
            "/x/com.stackunderflow.backup.plist".to_owned()
        ]
    );
    assert_eq!(
        launchctl_argv("unload", std::path::Path::new("/x/y"))[1],
        "unload"
    );
}

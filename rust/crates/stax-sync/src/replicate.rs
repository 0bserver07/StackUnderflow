//! `cli.py::_replicate_backup` — the rsync-over-ssh transport, argv only.
//!
//! This is *replication*, not sync: one direction, whole artifacts, no merge.
//! It lives in this crate because it is the second consumer of
//! [`crate::ssh_store::parse_ssh_url`] and because it is the other thing the
//! wave-6 brief means by "transport orchestration" — but it belongs to
//! `backup create --to`, not to `stax sync`.
//!
//! # Why the argv is a value and not a side effect
//!
//! The brief forbids running ssh at a remote host. Everything interesting about
//! this function is therefore the two argvs it builds — and they are genuinely
//! interesting: the `-e` string is a *shell word list* ssh parses itself (so the
//! port must not shell-quote it), and `--link-dest` is resolved **on the remote
//! side**, so it names the previous generation sitting there rather than the
//! local one. Get either wrong and the failure is silent extra disk usage on
//! someone else's machine.
//!
//! Note also what is NOT quoted: `_replicate_backup` passes `mkdir -p
//! {shlex.quote(root)}` through `target.ssh_argv()` (quoted, correct) but hands
//! rsync the destination as a bare `f"{host}:{dir}/"` — rsync's own remote-shell
//! quoting applies there, and a root with a space would break. Ported
//! bug-for-bug; recorded as DIV-215.

use crate::ssh_store::{SSHTarget, shlex_quote};

/// The two invocations `_replicate_backup` builds, in the order it builds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationPlan {
    /// The remote directory the backup lands in — `{root}/{dest.name}`.
    pub remote_dir: String,
    /// `[*target.ssh_argv(), "mkdir -p {shlex.quote(root)}"]`, timeout 60 s.
    pub mkdir_argv: Vec<String>,
    /// `["rsync", "-a", "-e", ssh_cmd, (--link-dest)?, "{dest}/", "{host}:{remote_dir}/"]`,
    /// timeout 3600 s.
    pub rsync_argv: Vec<String>,
    /// The `-e` value, exposed because it is assembled by string concatenation
    /// and NOT from `_SSH_BASE_OPTS` — it drops `ConnectTimeout=10`.
    pub ssh_cmd: String,
}

/// `_replicate_backup(dest, to_url, previous)` — everything up to the spawn.
///
/// `dest` is the finished local backup directory and `previous` the prior
/// generation's, both used only for their **final path component**.
///
/// # Errors
/// [`crate::ssh_store::parse_ssh_url`]'s message, which `cli.py` prints as
/// `Invalid --to destination: {exc}`.
pub fn plan(
    dest_name: &str,
    dest_path: &str,
    to_url: &str,
    previous_name: Option<&str>,
) -> Result<ReplicationPlan, String> {
    let target = crate::ssh_store::parse_ssh_url(to_url)?;
    Ok(plan_for(&target, dest_name, dest_path, previous_name))
}

/// The plan for an already-parsed target.
#[must_use]
pub fn plan_for(
    target: &SSHTarget,
    dest_name: &str,
    dest_path: &str,
    previous_name: Option<&str>,
) -> ReplicationPlan {
    // NOTE: this is NOT `_SSH_BASE_OPTS`. `cli.py` hand-writes two of the three
    // options and omits `ConnectTimeout=10`, so a replication to an unreachable
    // host hangs where a `sync push` to the same host would fail in ten
    // seconds. Reproduced exactly; recorded as DIV-215.
    let mut ssh_cmd = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new".to_owned();
    if let Some(port) = target.port {
        ssh_cmd.push_str(&format!(" -p {port}"));
    }

    let remote_dir = format!("{}/{}", target.root, dest_name);

    let mut mkdir_argv = target.ssh_argv();
    mkdir_argv.push(format!("mkdir -p {}", shlex_quote(&target.root)));

    let mut rsync_argv = vec![
        "rsync".to_owned(),
        "-a".to_owned(),
        "-e".to_owned(),
        ssh_cmd.clone(),
    ];
    if let Some(previous) = previous_name {
        // Interpreted on the REMOTE side, so it points at the previous
        // generation already sitting there — not at the local one.
        rsync_argv.push(format!("--link-dest={}/{previous}", target.root));
    }
    rsync_argv.push(format!("{dest_path}/"));
    rsync_argv.push(format!("{}:{remote_dir}/", target.host));

    ReplicationPlan {
        remote_dir,
        mkdir_argv,
        rsync_argv,
        ssh_cmd,
    }
}

/// `_RSYNC_VANISHED` — "some files vanished before they could be transferred".
pub const RSYNC_VANISHED: i32 = 24;

/// `_RSYNC_PARTIAL` — "partial transfer due to error".
pub const RSYNC_PARTIAL: i32 = 23;

/// `_rsync_reported(stderr, limit=6)` — condense rsync's per-file complaints.
///
/// rsync's last stderr line is always the generic `rsync error: …` recap; the
/// useful part is the per-path lines above it. Capped so one pathological run
/// cannot dump thousands of lines into a backup log.
#[must_use]
pub fn rsync_reported(stderr: &str, limit: usize) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("rsync error:"))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut shown = lines
        .iter()
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    if lines.len() > limit {
        shown.push_str(&format!("; (+{} more)", lines.len() - limit));
    }
    shown
}

/// `_rsync_outcome(returncode, stderr, what=…)` — `(ok, message)`.
///
/// Exit 0 is silent success; 24 is success-with-note; 23 is
/// success-with-warning naming what rsync could not transfer; every other
/// non-zero code is a failure and the message is the raw stderr.
///
/// The trees being mirrored are LIVE — a running agent rotates shell snapshots
/// and appends session JSONL while rsync walks them — so treating 24/23 as
/// fatal meant a machine that is actually being used could never finish a
/// backup, which is precisely when backups matter.
#[must_use]
pub fn rsync_outcome(returncode: i32, stderr: &str, what: &str) -> (bool, String) {
    if returncode == 0 {
        return (true, String::new());
    }
    let detail = rsync_reported(stderr, 6);
    if returncode == RSYNC_VANISHED {
        let msg = format!(
            "  Note: source files vanished mid-copy while backing up {what} (rsync 24) — \
             normal on a live tree; everything still present was copied."
        );
        return if detail.is_empty() {
            (true, msg)
        } else {
            (true, format!("{msg}\n    {detail}"))
        };
    }
    if returncode == RSYNC_PARTIAL {
        let msg = format!(
            "  Warning: partial transfer backing up {what} (rsync 23) — \
             kept what copied; rsync reported:"
        );
        let detail = if detail.is_empty() {
            "(no detail on stderr)".to_owned()
        } else {
            detail
        };
        return (true, format!("{msg}\n    {detail}"));
    }
    (false, stderr.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_generation_has_no_link_dest() {
        let plan = plan(
            "2026-07-31T120000",
            "/home/yad/.stackunderflow/backups/2026-07-31T120000",
            "ssh://yad@box/srv/backups",
            None,
        )
        .expect("plan");
        assert_eq!(plan.remote_dir, "/srv/backups/2026-07-31T120000");
        assert_eq!(
            plan.mkdir_argv,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "yad@box",
                "mkdir -p /srv/backups"
            ]
        );
        assert_eq!(
            plan.rsync_argv,
            vec![
                "rsync",
                "-a",
                "-e",
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
                "/home/yad/.stackunderflow/backups/2026-07-31T120000/",
                "yad@box:/srv/backups/2026-07-31T120000/",
            ]
        );
    }

    #[test]
    fn link_dest_names_the_remote_previous_generation_not_the_local_one() {
        let plan = plan(
            "gen-2",
            "/local/backups/gen-2",
            "ssh://box:2222/srv/backups",
            Some("gen-1"),
        )
        .expect("plan");
        assert!(
            plan.rsync_argv
                .contains(&"--link-dest=/srv/backups/gen-1".to_owned()),
            "{:?}",
            plan.rsync_argv
        );
        assert!(
            !plan
                .rsync_argv
                .iter()
                .any(|arg| arg.contains("/local/backups/gen-1")),
            "the LOCAL previous path must never appear"
        );
        // The port rides in the `-e` string, not as an rsync flag.
        assert_eq!(
            plan.ssh_cmd,
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -p 2222"
        );
        // …and the mkdir leg still uses the full `_SSH_BASE_OPTS` with `-p`.
        assert!(plan.mkdir_argv.contains(&"ConnectTimeout=10".to_owned()));
        assert!(plan.mkdir_argv.contains(&"2222".to_owned()));
    }

    #[test]
    fn the_e_string_omits_connecttimeout_and_that_is_the_references_bug() {
        // DIV-215. `sync push` to an unreachable host fails in ten seconds;
        // `backup create --to` the same host hangs on the rsync leg, because
        // the hand-written `-e` string drops the option the shared constant
        // carries. Pinned so a "helpful" fix is a deliberate change.
        let plan = plan("g", "/l/g", "ssh://box/srv", None).expect("plan");
        assert!(!plan.ssh_cmd.contains("ConnectTimeout"));
        assert!(plan.mkdir_argv.contains(&"ConnectTimeout=10".to_owned()));
    }

    #[test]
    fn an_invalid_destination_reports_parse_ssh_urls_words() {
        assert_eq!(
            plan("g", "/l/g", "ssh://box", None).expect_err("no path"),
            "ssh URL needs an absolute remote directory, e.g. \
             ssh://host/srv/stackunderflow-sync (got 'ssh://box')"
        );
    }

    #[test]
    fn a_root_with_a_space_is_quoted_for_mkdir_and_bare_for_rsync() {
        // DIV-215's second half — the asymmetry is the reference's.
        let plan = plan("g", "/l/g", "ssh://box/srv/my backups", None).expect("plan");
        assert_eq!(
            plan.mkdir_argv.last().expect("mkdir"),
            "mkdir -p '/srv/my backups'"
        );
        assert_eq!(
            plan.rsync_argv.last().expect("dest"),
            "box:/srv/my backups/g/"
        );
    }

    #[test]
    fn rsync_reported_drops_the_recap_and_caps_the_list() {
        assert_eq!(rsync_reported("", 6), "");
        assert_eq!(
            rsync_reported(
                "rsync error: some files could not be transferred (code 23)",
                6
            ),
            ""
        );
        assert_eq!(
            rsync_reported(
                "file has vanished: a\n  link_stat b failed\nrsync error: x",
                6
            ),
            "file has vanished: a; link_stat b failed"
        );
        let many = (1..=9)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rsync_reported(&many, 6),
            "line 1; line 2; line 3; line 4; line 5; line 6; (+3 more)"
        );
    }

    #[test]
    fn twenty_four_is_a_note_and_twenty_three_a_warning_and_both_are_ok() {
        assert_eq!(
            rsync_outcome(0, "anything", "~/.claude"),
            (true, String::new())
        );

        let (ok, msg) = rsync_outcome(24, "file has vanished: x\nrsync error: y", "~/.claude");
        assert!(ok);
        assert!(msg.starts_with(
            "  Note: source files vanished mid-copy while backing up ~/.claude (rsync 24)"
        ));
        assert!(msg.ends_with("\n    file has vanished: x"));

        let (ok, msg) = rsync_outcome(23, "", "~/.codex");
        assert!(ok);
        assert!(msg.contains("Warning: partial transfer backing up ~/.codex (rsync 23)"));
        assert!(msg.ends_with("(no detail on stderr)"));

        let (ok, msg) = rsync_outcome(12, "  protocol mismatch  ", "~/.claude");
        assert!(!ok);
        assert_eq!(msg, "protocol mismatch");
    }

    #[test]
    fn a_bare_note_with_no_detail_has_no_trailing_indent_block() {
        let (ok, msg) = rsync_outcome(24, "rsync error: only the recap", "~/.claude");
        assert!(ok);
        assert!(!msg.contains('\n'), "{msg}");
    }
}

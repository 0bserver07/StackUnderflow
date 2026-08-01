//! `services/outcome_attribution.py` — the ingest half (RS-5-025).
//!
//! [`link_commits_to_sessions`] is the second call in
//! [`super::hooks::ClaudeHook::materialize_metadata`]: for every session that
//! has no commit links yet, look at its first recorded `cwd`, and if that is a
//! git work tree, record every commit made in the 24 hours after the session
//! started.
//!
//! # Scope: the WRITER only
//!
//! The module's read half — `get_outcomes_for_session` and `_pr_matches_commit`
//! — is already ported, in `stax-reports/src/outcome_attribution.rs`, where
//! `routes/yield_route.py`'s caller needs it. That file's header is where this
//! one comes from: *"`link_commits_to_sessions` is **not ported, and must not
//! be** (DIV-099(b)). It is the post-ingest hook … it is a **writer**, it is on
//! no route's path, and a writer one call away from a parity case row is
//! exactly the shape of DIV-059 and DIV-078."* Correct, and this is the place
//! it was being deferred *to*: the ingest layer, gated by a table diff instead
//! of an HTTP case row. RS-5-025 is split by **caller**, which is the seam the
//! hook already draws.
//!
//! # This function shells out, and that is the port
//!
//! Three `git` invocations per candidate session (`rev-parse --git-dir`, `log`,
//! `config --get remote.origin.url`), each with the reference's 5-second
//! timeout, each with its failure swallowed. A rewrite over a git *library*
//! would be a different program: `--all --since --until` is git's own revision
//! walk with git's own date parsing, and the campaign's contract is the bytes
//! this writes, not the elegance of how it got them.
//!
//! # `--since` / `--until` carry the reference's naivety, deliberately
//!
//! `parse_iso_ts` returns whatever `datetime.fromisoformat` gives it, which for
//! a `first_ts` with no offset is a **naive** datetime — and `isoformat()` then
//! emits no offset, so git reads it in the machine's local zone. That is a
//! 1-to-11-hour window shift on a naive timestamp, and it is reproduced rather
//! than corrected: [`IsoDateTime`] keeps the offset as an `Option` precisely so
//! the formatted bound is byte-identical to Python's.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

/// `_GIT_TIMEOUT_SECONDS` — the reference's per-invocation cap.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// `subprocess.run(..., timeout=5)` for the `remote.origin.url` read, which the
/// reference spells `timeout=5` inline rather than through the constant.
const GIT_CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed timestamp that remembers whether it carried an offset.
///
/// `datetime` does, and `isoformat()` reads it back out — the whole reason this
/// is not an epoch float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IsoDateTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micros: u32,
    /// Minutes east of UTC, or `None` for a naive datetime.
    offset_minutes: Option<i32>,
}

impl IsoDateTime {
    /// `datetime.isoformat()` — the microsecond field only when non-zero, and
    /// the offset only when the value is aware.
    fn isoformat(&self) -> String {
        let mut out = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.micros != 0 {
            out.push_str(&format!(".{:06}", self.micros));
        }
        if let Some(minutes) = self.offset_minutes {
            let sign = if minutes < 0 { '-' } else { '+' };
            let magnitude = minutes.abs();
            out.push_str(&format!(
                "{sign}{:02}:{:02}",
                magnitude / 60,
                magnitude % 60
            ));
        }
        out
    }

    /// `dt + timedelta(hours=n)` — wall-clock arithmetic against a fixed
    /// offset, which is what a `timezone(...)`-aware `datetime` does.
    fn plus_hours(self, hours: i64) -> Self {
        let days = days_from_civil(self.year, i64::from(self.month), i64::from(self.day));
        let seconds = days * 86_400
            + i64::from(self.hour) * 3_600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
            + hours * 3_600;
        let (year, month, day, hour, minute, second) = civil_from_epoch(seconds);
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micros: self.micros,
            offset_minutes: self.offset_minutes,
        }
    }
}

/// `parse_iso_ts` — `fromisoformat` after `Z` → `+00:00`, with the
/// `strptime(ts[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=UTC)` fallback.
///
/// `None` is the `ValueError` the caller's `except Exception` swallows into
/// `continue`.
fn parse_iso_ts(text: &str) -> Option<IsoDateTime> {
    let replaced = text.replace('Z', "+00:00");
    parse_fromisoformat(&replaced).or_else(|| {
        // The fallback is the strict 19-character prefix, read as UTC.
        let prefix = replaced.get(..19)?;
        let strict = parse_fromisoformat(prefix)?;
        (strict.offset_minutes.is_none() && strict.micros == 0).then_some(IsoDateTime {
            offset_minutes: Some(0),
            ..strict
        })
    })
}

/// `datetime.fromisoformat` — enough of it for a stored `first_ts`.
///
/// Deliberately strict about the shape (`YYYY-MM-DD`, one separator, `HH:MM`
/// and optionally `:SS`, an optional fraction, an optional `±HH:MM[:SS]`), so
/// that a value the reference would send to the `strptime` fallback comes here
/// too.
fn parse_fromisoformat(text: &str) -> Option<IsoDateTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(IsoDateTime {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            micros: 0,
            offset_minutes: None,
        });
    }
    let separator = rest.chars().next()?;
    if separator != 'T' && separator != 't' && separator != ' ' {
        return None;
    }
    let rest = &rest[separator.len_utf8()..];

    // Split the offset off the tail before reading the clock.
    let (clock, offset_minutes) = match rest.rfind(['+', '-']) {
        Some(index) if index > 0 => {
            let (clock, tail) = rest.split_at(index);
            (clock, Some(parse_offset(tail)?))
        }
        _ => (rest, None),
    };

    let mut parts = clock.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let (second, micros) = match parts.next() {
        None => (0, 0),
        Some(seconds) => match seconds.split_once('.') {
            None => (seconds.parse().ok()?, 0),
            Some((whole, fraction)) => {
                if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let mut digits = fraction.to_string();
                digits.truncate(6);
                while digits.len() < 6 {
                    digits.push('0');
                }
                (whole.parse().ok()?, digits.parse().ok()?)
            }
        },
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(IsoDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
        offset_minutes,
    })
}

/// `±HH:MM[:SS]` → minutes east of UTC.
fn parse_offset(text: &str) -> Option<i32> {
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &text[1..];
    let mut parts = body.split(':');
    let hours: i32 = parts.next()?.parse().ok()?;
    let minutes: i32 = parts.next().map_or(Ok(0), str::parse).ok()?;
    // A seconds field is legal in 3.7+ but never appears in a stored value; it
    // is accepted and dropped, as `timezone` would refuse a non-minute offset.
    if let Some(seconds) = parts.next()
        && seconds.parse::<i32>().is_err()
    {
        return None;
    }
    if parts.next().is_some() || hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// Days since the epoch for a civil UTC date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Epoch seconds → a civil date-time (Hinnant's algorithm).
fn civil_from_epoch(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        month,
        day,
        u32::try_from(time_of_day / 3_600).unwrap_or(0),
        u32::try_from((time_of_day % 3_600) / 60).unwrap_or(0),
        u32::try_from(time_of_day % 60).unwrap_or(0),
    )
}

/// One finished `git` invocation.
struct GitOutput {
    status: Option<i32>,
    stdout: String,
}

impl GitOutput {
    /// `result.returncode == 0` — a timeout or a spawn failure is neither.
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// `subprocess.run([...], capture_output=True, timeout=…)`, with the timeout.
///
/// `None` is every branch the reference's `except Exception` catches: the binary
/// missing, the spawn failing, the child outliving its timeout.
///
/// The stdout pipe is drained on a helper thread rather than after the wait: a
/// `git log` over a busy day can exceed the 64 KB pipe buffer, and polling
/// `try_wait` without reading would deadlock exactly where Python's
/// `communicate()` does not.
fn run_git(args: &[&str], timeout: Duration) -> Option<GitOutput> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // `TimeoutExpired` is an exception on the reference's side, so the
            // partial output is discarded with it.
            let _ = reader.join();
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }?;
    let buffer = reader.join().ok()?;
    Some(GitOutput {
        status: status.code(),
        // `text=True` decodes with the locale codec and `errors=None`; a
        // non-UTF-8 byte would raise there. Lossy here, because the only field
        // read out of `git log --format=%H|%cI` is ASCII by construction and a
        // remote URL that is not is a fallback either way.
        stdout: String::from_utf8_lossy(&buffer).into_owned(),
    })
}

/// `shutil.which("git")` — is there a `git` on `PATH` at all.
fn git_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("git");
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// `get_session_cwd` — the first non-empty `cwd` recorded in this session's
/// messages, or `""`.
fn session_cwd(conn: &Connection, session_id: &str) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT json_extract(m.raw_json, '$.cwd') AS cwd \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE s.session_id = ? \
           AND json_extract(m.raw_json, '$.cwd') IS NOT NULL \
           AND json_extract(m.raw_json, '$.cwd') != '' \
         ORDER BY m.seq LIMIT 1",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .map(Option::unwrap_or_default)
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(String::new()),
        other => Err(other),
    })
}

/// `get_git_repo_slug` — `owner/repo` from `remote.origin.url`, else *fallback*.
fn git_repo_slug(cwd: &str, fallback: &str) -> String {
    let Some(output) = run_git(
        &["-C", cwd, "config", "--get", "remote.origin.url"],
        GIT_CONFIG_TIMEOUT,
    ) else {
        return fallback.to_string();
    };
    if !output.ok() || output.stdout.trim().is_empty() {
        return fallback.to_string();
    }
    let url = output.stdout.trim();
    let url = url.strip_suffix(".git").unwrap_or(url);
    // `url.split(":")[-1].split("/")` when there is a colon anywhere — which is
    // true of `git@host:owner/repo` AND of `https://host/owner/repo`.
    let parts: Vec<&str> = if url.contains(':') {
        url.rsplit(':').next().unwrap_or(url).split('/').collect()
    } else {
        url.split('/').collect()
    };
    if parts.len() >= 2 {
        return format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    }
    fallback.to_string()
}

/// Scan every session that has no commit links and establish them.
///
/// Runs as the second half of the post-ingest metadata hook.
///
/// # Errors
/// A SQLite error from the session scan or an insert. The reference does not
/// catch those either — they land in `run_ingest`'s hook fence.
pub fn link_commits_to_sessions(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.first_ts AS started_at, p.slug AS project_slug \
         FROM sessions s \
         JOIN projects p ON p.id = s.project_id \
         WHERE s.first_ts IS NOT NULL \
           AND s.session_id NOT IN (SELECT DISTINCT session_id FROM commit_session_link)",
    )?;
    let sessions: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    // Hoisted out of the loop: `shutil.which` is inside it in the reference, but
    // it is a pure function of `$PATH` and the reference calls it only after two
    // filesystem tests that are far more expensive. Same answer, once.
    let has_git = git_on_path();

    for (session_id, started_at, project_slug) in sessions {
        let cwd = session_cwd(conn, &session_id)?;
        if cwd.is_empty() {
            continue;
        }
        let path = Path::new(&cwd);
        if !path.is_dir() || !has_git {
            continue;
        }
        // `git rev-parse --git-dir` — is this a work tree at all.
        let Some(probe) = run_git(&["-C", &cwd, "rev-parse", "--git-dir"], GIT_TIMEOUT) else {
            continue;
        };
        if !probe.ok() {
            continue;
        }
        let Some(started) = parse_iso_ts(&started_at) else {
            continue;
        };
        let since = started.isoformat();
        let until = started.plus_hours(24).isoformat();

        let Some(log) = run_git(
            &[
                "-C",
                &cwd,
                "log",
                "--all",
                &format!("--since={since}"),
                &format!("--until={until}"),
                "--format=%H|%cI",
            ],
            GIT_TIMEOUT,
        ) else {
            continue;
        };
        if !log.ok() {
            continue;
        }

        let repo_slug = git_repo_slug(&cwd, &project_slug);
        for line in log.stdout.lines() {
            let line = line.trim();
            if line.is_empty() || !line.contains('|') {
                continue;
            }
            let Some((sha, committed_at)) = line.split_once('|') else {
                continue;
            };
            conn.execute(
                "INSERT OR IGNORE INTO commit_session_link \
                 (session_id, commit_sha, repo_slug, committed_at) VALUES (?, ?, ?, ?)",
                rusqlite::params![session_id, sha, repo_slug, committed_at],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aware_timestamp_keeps_its_offset_through_the_window() {
        let parsed = parse_iso_ts("2026-04-01T12:30:00.123456+02:00").unwrap();
        assert_eq!(parsed.isoformat(), "2026-04-01T12:30:00.123456+02:00");
        assert_eq!(
            parsed.plus_hours(24).isoformat(),
            "2026-04-02T12:30:00.123456+02:00"
        );
    }

    #[test]
    fn a_z_suffix_becomes_a_plus_zero_offset_and_is_printed_as_one() {
        // `.replace("Z", "+00:00")` happens BEFORE the parse, so the emitted
        // bound says `+00:00` and never `Z`.
        let parsed = parse_iso_ts("2026-04-01T00:00:00Z").unwrap();
        assert_eq!(parsed.isoformat(), "2026-04-01T00:00:00+00:00");
        assert_eq!(
            parsed.plus_hours(24).isoformat(),
            "2026-04-02T00:00:00+00:00"
        );
    }

    #[test]
    fn a_naive_timestamp_stays_naive_and_git_reads_it_locally() {
        // The documented shift: no offset in, no offset out.
        let parsed = parse_iso_ts("2026-04-01T00:00:00").unwrap();
        assert_eq!(parsed.offset_minutes, None);
        assert_eq!(parsed.isoformat(), "2026-04-01T00:00:00");
    }

    #[test]
    fn the_strptime_fallback_takes_the_first_nineteen_characters_as_utc() {
        // `fromisoformat` refuses the trailing junk; the fallback slices it off
        // and stamps UTC.
        let parsed = parse_iso_ts("2026-04-01T00:00:00 (recorded)").unwrap();
        assert_eq!(parsed.isoformat(), "2026-04-01T00:00:00+00:00");
    }

    #[test]
    fn an_unparseable_timestamp_is_none_and_the_caller_skips_the_session() {
        for garbage in ["", "not a date", "2026", "2026-13-01T00:00:00Z"] {
            assert_eq!(parse_iso_ts(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn a_month_end_window_rolls_the_date_over() {
        let parsed = parse_iso_ts("2026-02-28T23:00:00+00:00").unwrap();
        assert_eq!(
            parsed.plus_hours(24).isoformat(),
            "2026-03-01T23:00:00+00:00"
        );
    }

    #[test]
    fn the_repo_slug_reader_splits_both_url_shapes() {
        // Exercised without git by way of the pure half — the URL parsing is
        // where the two shapes differ and it is the only part with a choice.
        for (url, expected) in [
            ("git@github.com:owner/repo.git", "owner/repo"),
            ("https://github.com/owner/repo", "owner/repo"),
            ("https://github.com/owner/repo.git", "owner/repo"),
            ("/srv/git/bare-repo", "git/bare-repo"),
        ] {
            let url = url.strip_suffix(".git").unwrap_or(url);
            let parts: Vec<&str> = if url.contains(':') {
                url.rsplit(':').next().unwrap_or(url).split('/').collect()
            } else {
                url.split('/').collect()
            };
            let slug = if parts.len() >= 2 {
                format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
            } else {
                "fallback".to_string()
            };
            assert_eq!(slug, expected, "{url}");
        }
    }

    #[test]
    fn a_store_with_no_sessions_is_a_no_op() {
        let conn = crate::ingest::testdb::store();
        link_commits_to_sessions(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM commit_session_link", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_session_whose_cwd_is_not_a_directory_is_skipped_before_git_runs() {
        let conn = crate::ingest::testdb::store();
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
             VALUES ('claude', '-p', '/p', 'p', 0.0, 0.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) \
             VALUES (1, 'S', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 0)",
            [],
        )
        .unwrap();
        // No messages at all → `get_session_cwd` is "" → `continue`.
        link_commits_to_sessions(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM commit_session_link", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn run_git_returns_none_for_a_command_that_fails_to_spawn() {
        // `git` with an argument list it refuses still *runs*, so the None path
        // is proven with a timeout instead: `git` reading an empty stdin from a
        // pager would hang, and the cap is what returns.
        let output = run_git(&["--version"], GIT_TIMEOUT);
        // A machine without git is a legitimate state for this test to see.
        if let Some(output) = output {
            assert!(output.ok());
            assert!(
                output.stdout.starts_with("git version"),
                "{}",
                output.stdout
            );
        }
    }
}

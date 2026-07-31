//! Port of `stackunderflow/etl/marts/command.py` — the
//! `(day, project_id, command_name)` rollup **and** `command_day_mart`.
//!
//! A "command" is a slash command (`/init`, `/review`, …) plus a synthetic
//! `freeform` bucket. User messages are not in `usage_events` — only billable
//! assistant rows are — so each event is attributed back to the most recent
//! preceding `role='user'` row in the same session by `(session_fk, seq)`, and
//! grouped by that message's parsed command. An event with no preceding prompt
//! lands in `__no_prompt__` so cost still conserves.
//!
//! # One builder, two tables, two different grains
//!
//! `command_mart` is event-attributed. `command_day_mart` (v025) is a per-day
//! count of *real user turns* — the same tally `project_mart.total_commands`
//! materialises — and it is recomputed from raw `messages`, not from the event
//! buckets, because a command can predate the window's events, produce many, or
//! produce none. `rebuild_from_scratch` therefore clears both.
//!
//! A consequence worth stating plainly, because the wave-3 diff will show it:
//! the per-day pass runs only for projects that appear in *this window's
//! events*, while the rebuild deletes every row of `command_day_mart` first. A
//! project with messages but no billable events loses its `command_day_mart`
//! rows on a full rebuild and does not get them back. That is Python's
//! behaviour, and the port reproduces it.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use super::{MartBuilder, max_event_id};
use crate::stats::classifier::{INTERRUPT_API, INTERRUPT_PREFIX, determine_kind};
use crate::stats::enricher::{has_result_block_of, text_from};
use crate::stats::pytext::py_lstrip;

/// Synthetic bucket for prompts that do not start with a slash command.
pub const FREEFORM: &str = "freeform";

/// Synthetic bucket for assistant events with no preceding user message.
const NO_PROMPT: &str = "__no_prompt__";

/// Per-`(day, project_id, command_name)` cost + token rollup.
pub struct CommandMartBuilder;

#[derive(Default, Clone, Copy)]
struct Bucket {
    event_count: i64,
    cost_usd: f64,
    tokens_in: i64,
    tokens_out: i64,
}

type Key = (String, i64, String);

struct WindowRow {
    day: String,
    project_id: Option<i64>,
    cost_usd: f64,
    input_tokens: i64,
    output_tokens: i64,
    session_fk: Option<i64>,
    event_seq: Option<i64>,
}

impl MartBuilder for CommandMartBuilder {
    fn name(&self) -> &'static str {
        "command"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        let rows = fetch_window(conn, since_event_id, max_id)?;

        // `sorted({int(r["project_id"]) for r in rows if r["project_id"] is not None})`
        let mut affected_projects: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.project_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        affected_projects.sort_unstable();

        let mut prompt_cache: HashMap<(i64, i64), String> = HashMap::new();
        let mut order: Vec<Key> = Vec::new();
        let mut buckets: HashMap<Key, Bucket> = HashMap::new();

        for r in &rows {
            let command_name = match (r.session_fk, r.event_seq) {
                (Some(fk), Some(seq)) => match prompt_cache.get(&(fk, seq)) {
                    Some(c) => c.clone(),
                    None => {
                        let c = find_command_for(conn, fk, seq)?;
                        prompt_cache.insert((fk, seq), c.clone());
                        c
                    }
                },
                _ => NO_PROMPT.to_string(),
            };

            // Python indexes `int(r["project_id"])` unconditionally here — a
            // NULL project_id would raise. The column is NOT NULL.
            let agg_key = (
                r.day.clone(),
                r.project_id.unwrap_or_default(),
                command_name,
            );
            if !buckets.contains_key(&agg_key) {
                order.push(agg_key.clone());
                buckets.insert(agg_key.clone(), Bucket::default());
            }
            let bucket = buckets.get_mut(&agg_key).expect("just inserted");
            bucket.event_count += 1;
            bucket.cost_usd += r.cost_usd;
            bucket.tokens_in += r.input_tokens;
            bucket.tokens_out += r.output_tokens;
        }

        if !order.is_empty() {
            let mut stmt = conn.prepare(
                r"
                INSERT INTO command_mart (
                    day, project_id, command_name,
                    event_count, cost_usd, tokens_in, tokens_out,
                    session_count
                ) VALUES (?, ?, ?, ?, ?, ?, ?, 0)
                ON CONFLICT (day, project_id, command_name) DO UPDATE SET
                    event_count = event_count + excluded.event_count,
                    cost_usd    = cost_usd    + excluded.cost_usd,
                    tokens_in   = tokens_in   + excluded.tokens_in,
                    tokens_out  = tokens_out  + excluded.tokens_out
                ",
            )?;
            for key in &order {
                let v = &buckets[key];
                stmt.execute(rusqlite::params![
                    key.0,
                    key.1,
                    key.2,
                    v.event_count,
                    v.cost_usd,
                    v.tokens_in,
                    v.tokens_out,
                ])?;
            }
            drop(stmt);

            recompute_session_counts(conn, &order)?;
        }

        if !affected_projects.is_empty() && command_day_table_exists(conn)? {
            refresh_command_day_mart(conn, &affected_projects)?;
        }

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        // The single "command" mart owns both tables.
        conn.execute("DELETE FROM command_mart", [])?;
        if command_day_table_exists(conn)? {
            conn.execute("DELETE FROM command_day_mart", [])?;
        }
        self.refresh(conn, 0)?;
        Ok(())
    }
}

/// `command._fetch_window`.
fn fetch_window(conn: &Connection, since_event_id: i64, max_id: i64) -> Result<Vec<WindowRow>> {
    let mut stmt = conn.prepare(
        r"
        SELECT e.id            AS event_id,
               e.day           AS day,
               e.project_id    AS project_id,
               e.session_id    AS session_id,
               e.cost_usd      AS cost_usd,
               e.input_tokens  AS input_tokens,
               e.output_tokens AS output_tokens,
               m.session_fk    AS session_fk,
               m.seq           AS event_seq
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
        ",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![since_event_id, max_id], |r| {
            Ok(WindowRow {
                day: r.get::<_, Option<String>>("day")?.unwrap_or_default(),
                project_id: r.get::<_, Option<i64>>("project_id")?,
                cost_usd: r.get::<_, Option<f64>>("cost_usd")?.unwrap_or(0.0),
                input_tokens: r.get::<_, Option<i64>>("input_tokens")?.unwrap_or(0),
                output_tokens: r.get::<_, Option<i64>>("output_tokens")?.unwrap_or(0),
                session_fk: r.get::<_, Option<i64>>("session_fk")?,
                event_seq: r.get::<_, Option<i64>>("event_seq")?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `command._find_command_for` — walk back to the most recent user prompt.
///
/// "No preceding user message" is `__no_prompt__`; a SQL error is an error.
fn find_command_for(conn: &Connection, session_fk: i64, event_seq: i64) -> Result<String> {
    let text: Option<Option<String>> = conn
        .query_row(
            r"
            SELECT content_text
              FROM messages
             WHERE session_fk = ?
               AND role = 'user'
               AND seq < ?
             ORDER BY seq DESC
             LIMIT 1
            ",
            rusqlite::params![session_fk, event_seq],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    match text {
        None => Ok(NO_PROMPT.to_string()),
        Some(t) => Ok(parse_command_name(t.as_deref().unwrap_or(""))),
    }
}

/// `command.parse_command_name` — the slash-command name, or [`FREEFORM`].
///
/// Python: `_SLASH_RE = re.compile(r"^/([A-Za-z][A-Za-z0-9_-]{0,63})\b")`
/// applied to `content_text.lstrip()`.
///
/// Two things make this more than a `starts_with`:
///
/// * `lstrip()` is CPython's whitespace set, not Unicode's — see
///   [`crate::stats::pytext::is_py_space`].
/// * `\b` after a *greedy* class that includes `-` (a non-word character)
///   forces backtracking. `"/foo- bar"` matches `/foo`, not `/foo-`, because a
///   boundary needs exactly one side to be a word character. The loop below
///   walks candidate lengths longest-first, which is the order the regex engine
///   tries them.
///
/// `\b`'s "word character" is Unicode-aware for `str` patterns (`\w` is
/// alphanumeric plus underscore over the whole repertoire), so the *following*
/// character is tested with `char::is_alphanumeric`, not an ASCII class.
#[must_use]
pub fn parse_command_name(content_text: &str) -> String {
    if content_text.is_empty() {
        return FREEFORM.to_string();
    }
    let stripped = py_lstrip(content_text);
    let Some(body) = stripped.strip_prefix('/') else {
        return FREEFORM.to_string();
    };
    let bytes = body.as_bytes();
    // `[A-Za-z]` for the first character.
    if bytes.first().is_none_or(|b| !b.is_ascii_alphabetic()) {
        return FREEFORM.to_string();
    }
    // The maximal run of `[A-Za-z0-9_-]`, capped at 1 + 63 characters.
    let run = bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-')
        .count();
    let max_len = run.min(64);

    for len in (1..=max_len).rev() {
        let last = bytes[len - 1];
        let last_is_word = last != b'-';
        let next_is_word = body[len..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if last_is_word != next_is_word {
            return format!("/{}", &body[..len]);
        }
    }
    FREEFORM.to_string()
}

/// `command._recompute_session_counts` — one scan per `(day, project_id)` group.
fn recompute_session_counts(conn: &Connection, keys: &[Key]) -> Result<()> {
    let mut group_order: Vec<(String, i64)> = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    for k in keys {
        let g = (k.0.clone(), k.1);
        if seen.insert(g.clone()) {
            group_order.push(g);
        }
    }

    let mut scan = conn.prepare(
        r"
        SELECT e.session_id    AS session_id,
               m.session_fk    AS session_fk,
               m.seq           AS event_seq
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.day = ? AND e.project_id = ?
        ",
    )?;
    let mut update = conn.prepare(
        r"
        UPDATE command_mart
           SET session_count = ?
         WHERE day = ? AND project_id = ? AND command_name = ?
        ",
    )?;

    for (day, project_id) in &group_order {
        let rows: Vec<(String, Option<i64>, Option<i64>)> = scan
            .query_map(rusqlite::params![day, project_id], |r| {
                Ok((
                    r.get::<_, Option<String>>("session_id")?
                        .unwrap_or_default(),
                    r.get::<_, Option<i64>>("session_fk")?,
                    r.get::<_, Option<i64>>("event_seq")?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut per_command_order: Vec<String> = Vec::new();
        let mut per_command_sessions: HashMap<String, HashSet<String>> = HashMap::new();
        let mut local_cache: HashMap<(i64, i64), String> = HashMap::new();
        for (session_id, session_fk, seq) in rows {
            let cmd = match (session_fk, seq) {
                (Some(fk), Some(s)) => match local_cache.get(&(fk, s)) {
                    Some(c) => c.clone(),
                    None => {
                        let c = find_command_for(conn, fk, s)?;
                        local_cache.insert((fk, s), c.clone());
                        c
                    }
                },
                _ => NO_PROMPT.to_string(),
            };
            if let Some(set) = per_command_sessions.get_mut(&cmd) {
                set.insert(session_id);
            } else {
                per_command_order.push(cmd.clone());
                per_command_sessions.insert(cmd, HashSet::from([session_id]));
            }
        }

        for cmd in &per_command_order {
            #[allow(clippy::cast_possible_wrap)]
            let n = per_command_sessions[cmd].len() as i64;
            update.execute(rusqlite::params![n, day, project_id, cmd])?;
        }
    }
    Ok(())
}

/// `command._command_day_table_exists` — guards a store mid-migration.
fn command_day_table_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='command_day_mart'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// `command._refresh_command_day_mart` — replace-from-scratch per project.
///
/// A "command" here is the SAME real user turn `project_mart.total_commands`
/// counts, bucketed by the leading 10 characters of the message timestamp.
fn refresh_command_day_mart(conn: &Connection, project_ids: &[i64]) -> Result<()> {
    let mut scan = conn.prepare(
        "SELECT m.raw_json AS raw_json, \
                substr(m.timestamp, 1, 10) AS day \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE s.project_id = ?",
    )?;
    let mut del = conn.prepare("DELETE FROM command_day_mart WHERE project_id = ?")?;
    let mut ins = conn.prepare(
        "INSERT INTO command_day_mart (day, project_id, command_count) VALUES (?, ?, ?)",
    )?;

    for pid in project_ids {
        let rows: Vec<(Option<String>, Option<String>)> = scan
            .query_map([pid], |r| {
                Ok((
                    r.get::<_, Option<String>>("raw_json")?,
                    r.get::<_, Option<String>>("day")?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut day_order: Vec<String> = Vec::new();
        let mut per_day: HashMap<String, i64> = HashMap::new();
        for (raw_json, day) in rows {
            // `if not day: continue` — NULL and empty string both skip.
            let Some(day) = day.filter(|d| !d.is_empty()) else {
                continue;
            };
            if is_user_command(raw_json.as_deref()) {
                if let Some(n) = per_day.get_mut(&day) {
                    *n += 1;
                } else {
                    day_order.push(day.clone());
                    per_day.insert(day, 1);
                }
            }
        }

        del.execute([pid])?;
        for day in &day_order {
            ins.execute(rusqlite::params![day, pid, per_day[day]])?;
        }
    }
    Ok(())
}

/// `command._is_user_command` — a real user command turn.
///
/// Mirrors `project.py`'s `_count_message_dims` command rule exactly: kind
/// `user`, no `tool_result` block, and text that does not open with an
/// interruption marker. That equality is why `command_day_mart.command_count`
/// summed over a project equals `project_mart.total_commands`.
#[must_use]
pub fn is_user_command(raw_json: Option<&str>) -> bool {
    let Some(payload) = super::json::loads(raw_json) else {
        return false;
    };
    if !payload.is_object() {
        return false;
    }
    if determine_kind(&payload) != "user" {
        return false;
    }
    if has_result_block_of(&payload) {
        return false;
    }
    let text = text_from(&payload);
    !(text.starts_with(INTERRUPT_PREFIX) || text.starts_with(INTERRUPT_API))
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    /// Every right-hand side here is CPython's answer, produced by running the
    /// case through `etl.marts.command.parse_command_name` itself. The command
    /// name is a `command_mart` **primary-key column**, so a port that is
    /// merely reasonable here silently re-buckets a project's costs.
    const PYTHON_ORACLE: &[(&str, &str)] = &[
        ("", "freeform"),
        ("/init args", "/init"),
        ("   /review-pr", "/review-pr"),
        ("hello", "freeform"),
        ("// comment", "freeform"),
        ("/abs/path", "/abs"),
        ("/9lives", "freeform"),
        ("/_x", "freeform"),
        ("/foo- bar", "/foo"),
        ("/foo-", "/foo"),
        ("/foo-bar baz", "/foo-bar"),
        ("/foo--bar", "/foo--bar"),
        ("/foo--", "/foo"),
        ("/a_b", "/a_b"),
        ("/init.", "/init"),
        ("/init\u{e9}", "freeform"),
        ("/i", "/i"),
        ("/i-", "/i"),
        ("/i_", "/i_"),
        ("/init\tx", "/init"),
        ("/init\n", "/init"),
        ("/init\u{b}\u{c}", "/init"),
        ("\u{3000}/init", "/init"),
        ("\u{200b}/init", "freeform"),
        ("\u{a0}/init", "/init"),
        ("\u{1c}\u{1f}/init", "/init"),
        ("\t\n\r /init", "/init"),
        ("/", "freeform"),
        ("/-", "freeform"),
        ("/1", "freeform"),
        ("/init2fa", "/init2fa"),
        ("/INIT", "/INIT"),
        ("/Init-PR_v2", "/Init-PR_v2"),
        ("/x-y-z", "/x-y-z"),
        ("/x-y-", "/x-y"),
        ("/x--y", "/x--y"),
        ("/x_-y", "/x_-y"),
        ("/x-_y", "/x-_y"),
        ("  \t /deploy --now", "/deploy"),
        ("/deploy next", "/deploy"),
        ("/a----------", "/a"),
        ("/a----------b", "/a----------b"),
        ("/init(", "/init"),
        ("/init)", "/init"),
        ("/init/", "/init"),
        ("/init:", "/init"),
        ("/init0", "/init0"),
        // U+0301 is a combining mark: not alphanumeric, so `\b` holds.
        ("/init\u{301}", "/init"),
        // U+0660 is an Arabic-Indic digit: alphanumeric, so `\b` does not.
        ("/init\u{660}", "freeform"),
        ("/init\u{df}", "freeform"),
    ];

    #[test]
    fn command_names_agree_with_cpython_case_for_case() {
        for (text, expected) in PYTHON_ORACLE {
            assert_eq!(&parse_command_name(text), expected, "input {text:?}");
        }
    }

    #[test]
    fn the_length_cap_agrees_with_cpython_at_the_boundary() {
        let a = |n: usize| "a".repeat(n);
        for (input, expected) in [
            (format!("/{}", a(63)), format!("/{}", a(63))),
            (format!("/{}", a(64)), format!("/{}", a(64))),
            (format!("/{}", a(65)), "freeform".to_string()),
            (format!("/{} rest", a(64)), format!("/{}", a(64))),
            (format!("/{} rest", a(65)), "freeform".to_string()),
            (format!("/{}- rest", a(63)), format!("/{}", a(63))),
            // 64 characters then a dash: the class would take the dash as the
            // 65th, which the `{0,63}` cap forbids, so the 64-character
            // candidate stands and `\b` holds against the dash.
            (format!("/{}- rest", a(64)), format!("/{}", a(64))),
        ] {
            assert_eq!(parse_command_name(&input), expected, "input {input:?}");
        }
    }

    #[test]
    fn slash_commands_parse_the_way_the_regex_does() {
        assert_eq!(parse_command_name("/init do the thing"), "/init");
        assert_eq!(parse_command_name("   /review-pr"), "/review-pr");
        assert_eq!(parse_command_name("hello"), FREEFORM);
        assert_eq!(parse_command_name(""), FREEFORM);
        assert_eq!(parse_command_name("// comment"), FREEFORM);
        assert_eq!(parse_command_name("/abs/path"), "/abs");
        assert_eq!(parse_command_name("/9lives"), FREEFORM);
        assert_eq!(parse_command_name("/_x"), FREEFORM);
    }

    #[test]
    fn the_word_boundary_backtracks_off_a_trailing_dash() {
        // `[A-Za-z0-9_-]{0,63}` is greedy, `-` is not a word character, and
        // `\b` needs exactly one side to be one — so the engine gives the dash
        // back. A `starts_with`-shaped port returns "/foo-" here.
        assert_eq!(parse_command_name("/foo- bar"), "/foo");
        assert_eq!(parse_command_name("/foo-"), "/foo");
        assert_eq!(parse_command_name("/foo-bar baz"), "/foo-bar");
        assert_eq!(parse_command_name("/foo--bar"), "/foo--bar");
        assert_eq!(parse_command_name("/foo--"), "/foo");
        assert_eq!(parse_command_name("/a_b"), "/a_b");
    }

    #[test]
    fn the_length_cap_is_sixty_four_characters() {
        let long = format!("/{} rest", "a".repeat(80));
        // The class matches 1 + 63; the 65th character is a word character, so
        // `\b` fails there and every shorter candidate fails the same way.
        assert_eq!(parse_command_name(&long), FREEFORM);
        let exact = format!("/{} rest", "a".repeat(64));
        assert_eq!(parse_command_name(&exact), format!("/{}", "a".repeat(64)));
    }

    #[test]
    fn lstrip_uses_cpythons_whitespace_set() {
        // U+001C is whitespace to `str.lstrip()` and not to `trim_start()`.
        assert_eq!(parse_command_name("\u{1c}\u{1f}/init"), "/init");
        assert_eq!(parse_command_name("\u{3000}/init"), "/init");
        // …and U+200B is whitespace to neither.
        assert_eq!(parse_command_name("\u{200b}/init"), FREEFORM);
    }

    #[test]
    fn a_unicode_letter_after_the_run_blocks_the_boundary() {
        // `\b` is Unicode-aware for str patterns: `é` is a word character.
        assert_eq!(parse_command_name("/init\u{e9}"), FREEFORM);
        assert_eq!(parse_command_name("/init."), "/init");
    }

    fn seed(c: &Connection) {
        testdb::project(c, 1, "p", "claude");
        testdb::session(c, 1, 1, "s1");
    }

    #[test]
    fn events_attribute_to_the_most_recent_preceding_user_prompt() {
        let c = testdb::conn();
        seed(&c);
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            "/init go",
            "[]",
            "{}",
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "assistant",
            "ok",
            "[]",
            "{}",
        );
        testdb::message(
            &c,
            3,
            1,
            2,
            "2026-01-01T00:02:00Z",
            "user",
            "plain words",
            "[]",
            "{}",
        );
        testdb::message(
            &c,
            4,
            1,
            3,
            "2026-01-01T00:03:00Z",
            "assistant",
            "ok",
            "[]",
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(2),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (10, 1, 0, 0),
            1.0,
        );
        testdb::event(
            &c,
            2,
            Some(4),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (20, 2, 0, 0),
            2.0,
        );
        CommandMartBuilder.refresh(&c, 0).unwrap();

        let mut stmt = c
            .prepare(
                "SELECT command_name, event_count, cost_usd, tokens_in, session_count \
                 FROM command_mart ORDER BY command_name",
            )
            .unwrap();
        let rows: Vec<(String, i64, f64, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "/init");
        assert_eq!((rows[0].1, rows[0].3, rows[0].4), (1, 10, 1));
        assert_eq!(rows[1].0, FREEFORM);
        assert_eq!((rows[1].1, rows[1].3), (1, 20));
    }

    #[test]
    fn an_orphaned_assistant_event_lands_in_no_prompt() {
        let c = testdb::conn();
        seed(&c);
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "hi",
            "[]",
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            3.0,
        );
        // …and an event whose source message is gone entirely.
        testdb::event(
            &c,
            2,
            Some(99),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            4.0,
        );
        CommandMartBuilder.refresh(&c, 0).unwrap();
        let (name, n, cost): (String, i64, f64) = c
            .query_row(
                "SELECT command_name, event_count, cost_usd FROM command_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, NO_PROMPT);
        assert_eq!(n, 2);
        assert!(
            (cost - 7.0).abs() < 1e-12,
            "cost must still conserve: {cost}"
        );
    }

    #[test]
    fn command_day_counts_user_turns_not_events() {
        let c = testdb::conn();
        seed(&c);
        let user = |text: &str| {
            format!(r#"{{"type":"human","message":{{"role":"user","content":"{text}"}}}}"#)
        };
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            "a",
            "[]",
            &user("a"),
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "user",
            "b",
            "[]",
            &user("b"),
        );
        // A tool_result turn is a user row but NOT a command.
        testdb::message(
            &c,
            3,
            1,
            2,
            "2026-01-01T00:02:00Z",
            "user",
            "r",
            "[]",
            r#"{"type":"human","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
        );
        // An interruption is a user turn but NOT a command.
        testdb::message(
            &c,
            4,
            1,
            3,
            "2026-01-02T00:00:00Z",
            "user",
            "i",
            "[]",
            &user("[Request interrupted by user for tool use]"),
        );
        testdb::message(
            &c,
            5,
            1,
            4,
            "2026-01-02T00:01:00Z",
            "assistant",
            "ok",
            "[]",
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(5),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-02",
            (1, 1, 0, 0),
            0.0,
        );
        CommandMartBuilder.refresh(&c, 0).unwrap();

        let mut stmt = c
            .prepare("SELECT day, command_count FROM command_day_mart ORDER BY day")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows, vec![("2026-01-01".to_string(), 2)]);
    }

    #[test]
    fn is_user_command_is_defensive_about_poison_rows() {
        assert!(!is_user_command(None));
        assert!(!is_user_command(Some("{not json")));
        assert!(!is_user_command(Some("[1,2]")));
        assert!(is_user_command(Some(
            r#"{"type":"human","message":{"content":"hi"}}"#
        )));
        // DIV-002: no type, no role → "assistant" → not a command.
        assert!(!is_user_command(Some(r#"{"message":{"content":"hi"}}"#)));
    }
}

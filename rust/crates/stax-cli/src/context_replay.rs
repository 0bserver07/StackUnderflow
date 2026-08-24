//! `stax context-replay` — `cli.py:2961`–`:3059` (+ `_emit_context_replay_text`).
//!
//! `stax_reports::context_replay::reconstruct_context` is the whole engine and
//! it is already ported; this module is the verb around it — the `--project`
//! fence, the `--limit` head, the `--context-budget` greedy pack, the envelope
//! and the text renderer.
//!
//! # The two truncation passes are ordered, and both set the same flag
//!
//! `--limit` cuts first (a plain head, `truncated = True` when it dropped
//! anything), then the budget packs what survived. So a run that hits both
//! reports one `truncated: true`, and a budget that would have kept more than
//! `--limit` events still cannot — the head already happened. Written in that
//! order rather than combined, because combining them changes which events a
//! `--limit 5 --context-budget 100000` call returns.
//!
//! # The pack keeps at least one event, always
//!
//! `if packed and used + est > budget:` — the guard tests `packed` first, so
//! the FIRST event goes in whatever it costs. A single 50,000-token turn under
//! `--context-budget 10` comes back as one row, not zero.
//!
//! # `_resolve_context_budget` here is NOT `MemoryEnv`
//!
//! The memory verbs resolve their budget through [`crate::memory::MemoryEnv`],
//! which also opens the FTS sidecar — and `SearchService`'s constructor
//! *creates* `search_index.db`. `context_replay_cmd` calls only
//! `Settings().discovery_budget_tokens`, so this module reads the setting
//! directly. Routing it through `MemoryEnv` would have made a read-only verb
//! materialise a database, which is the DIV-291 shape in reverse.

use anyhow::Result;
use clap::Args;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_core::queries::pyint::PyInt;
use stax_memory::envelope::{MemoryCommand, build_envelope, render_line};
use stax_memory::pyjson as mempyjson;
use stax_reports::context_replay::{empty_context, reconstruct_context};

use crate::click::Output;
use crate::reports::open_store;

/// `stax context-replay`.
#[derive(Debug, Args)]
pub struct ContextReplayArgs {
    /// The session to reconstruct.
    #[arg(value_name = "SESSION_ID", allow_hyphen_values = true)]
    pub session_id: String,
    /// seq cutoff (inclusive). Omit for the whole session's context.
    ///
    /// [`PyInt`] and `allow_hyphen_values`, as `memory`'s numeric options carry:
    /// Click's `type=int` is `int(value)`, which takes `' 5'`, `1_000`, `٧` and
    /// a bignum, and its parser pops the next token whatever it looks like — so
    /// `--at -1` is an exit-0 invocation on the reference and was an exit-2
    /// parse rejection here until this attribute landed.
    #[arg(long = "at", value_name = "AT", value_parser = crate::memory::py_int,
          allow_hyphen_values = true)]
    pub at_seq: Option<PyInt>,
    /// Project slug to fence to. A session in another project is treated as
    /// out-of-scope (empty-but-valid).
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Cap on the number of events returned (earliest first, in seq order).
    #[arg(long = "limit", value_name = "LIMIT", default_value_t = PyInt::from(100),
          value_parser = crate::memory::py_int, allow_hyphen_values = true)]
    pub limit: PyInt,
    /// Token budget for --json results: events are kept in order until ~this
    /// many estimated tokens are used. Pass 0 to disable.
    #[arg(long = "context-budget", value_name = "CONTEXT_BUDGET",
          value_parser = crate::memory::py_int, allow_hyphen_values = true)]
    pub context_budget: Option<PyInt>,
    /// Shortcut for --format json.
    #[arg(long = "json", action = clap::ArgAction::SetTrue)]
    pub as_json: bool,
    /// Output format. 'json' emits the stable agent-output envelope.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `context-replay`.
///
/// # Errors
/// A store that cannot be opened or migrated. An unknown session is NOT an
/// error — `reconstruct_context` answers an empty-but-valid result, which the
/// reference's docstring states outright.
pub fn run_context_replay(args: &ContextReplayArgs) -> Result<Output> {
    let conn = open_store()?;
    let at_seq = args.at_seq.as_ref().map(PyInt::saturating_i64);
    let mut result = reconstruct_context(&conn, &args.session_id, at_seq);
    // `if project and not _session_in_project(...)` — Python truthiness, so
    // `--project ''` skips the fence entirely (the twice-proven `--project ''`
    // class, DIV-236's shape).
    if let Some(project) = args.project.as_deref().filter(|slug| !slug.is_empty())
        && !session_in_project(&conn, &args.session_id, project)
    {
        result = empty_context(
            &args.session_id,
            at_seq,
            &[format!(
                "session {} is outside project {}",
                args.session_id,
                py_repr(project)
            )],
        );
    }
    drop(conn);

    let budget = resolve_context_budget(args.context_budget.as_ref());
    Ok(emit(args, &result, budget))
}

/// The pure half: pack, then render. Testable without a store.
#[must_use]
pub fn emit(args: &ContextReplayArgs, result: &Value, budget: i64) -> Output {
    let mut events: Vec<Value> = result
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut truncated = false;

    // `if limit and limit > 0` — a 0 or negative `--limit` disables the head.
    let limit = args.limit.saturating_i64();
    if limit > 0 && events.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
        events.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        truncated = true;
    }

    if budget > 0 {
        let mut packed: Vec<Value> = Vec::new();
        let mut used: i64 = 0;
        for event in events {
            let est = i64::try_from(mempyjson::estimate_tokens(&event)).unwrap_or(i64::MAX);
            // `if packed and used + est > budget` — the emptiness test comes
            // FIRST, so one event always survives.
            if !packed.is_empty() && used.saturating_add(est) > budget {
                truncated = true;
                break;
            }
            packed.push(event);
            used = used.saturating_add(est);
        }
        events = packed;
    }

    let json_mode = args.as_json || args.format == "json";
    if json_mode {
        let mut query = Map::new();
        query.insert(
            "session_id".to_owned(),
            Value::String(args.session_id.clone()),
        );
        query.insert("at".to_owned(), int_or_null(args.at_seq.as_ref()));
        query.insert(
            "project".to_owned(),
            args.project.clone().map_or(Value::Null, Value::String),
        );
        query.insert("limit".to_owned(), Value::from(limit));

        let mut extra = Map::new();
        // `result.get("session_id", session_id)` — the RESULT's id wins, and it
        // is the same string in every reachable case; kept as the fallback the
        // reference wrote.
        extra.insert(
            "session_id".to_owned(),
            result
                .get("session_id")
                .cloned()
                .unwrap_or_else(|| Value::String(args.session_id.clone())),
        );
        extra.insert(
            "at_seq".to_owned(),
            result.get("at_seq").cloned().unwrap_or(Value::Null),
        );
        extra.insert(
            "total_tokens".to_owned(),
            result
                .get("total_tokens")
                .cloned()
                .unwrap_or(Value::from(0)),
        );
        extra.insert(
            "message_count".to_owned(),
            result
                .get("message_count")
                .cloned()
                .unwrap_or(Value::from(0)),
        );
        extra.insert(
            "warnings".to_owned(),
            // `result.get("warnings") or []` — null AND an empty list are `[]`.
            match result.get("warnings") {
                Some(Value::Array(items)) if !items.is_empty() => Value::Array(items.clone()),
                _ => Value::Array(Vec::new()),
            },
        );

        let envelope = build_envelope(
            MemoryCommand::ContextReplay,
            query,
            events,
            budget,
            truncated,
            extra,
        );
        return Output::ok(render_line(&envelope));
    }

    Output::ok(render_text(result, &events))
}

/// `_emit_context_replay_text(result, events)`.
#[must_use]
pub fn render_text(result: &Value, events: &[Value]) -> String {
    let sid = result
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let at_seq = result.get("at_seq").and_then(Value::as_i64);
    // `"up to seq {at}" if at_seq is not None else "full session"` — a JSON
    // null is `None`, so an unfenced replay says "full session".
    let scope = at_seq.map_or_else(
        || "full session".to_owned(),
        |seq| format!("up to seq {seq}"),
    );
    let mut out = format!("Context replay for {sid} ({scope})\n");
    out.push_str(&format!(
        "  messages: {}   est. context tokens: {}\n",
        result
            .get("message_count")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        result
            .get("total_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    ));
    for warning in result
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("  ! {}\n", warning.as_str().unwrap_or_default()));
    }
    if events.is_empty() {
        // An early return: no blank line, no event block.
        out.push_str("  (no messages in range)\n");
        return out;
    }
    out.push('\n');
    for event in events {
        out.push_str(&format!(
            "  #{:>4} [{:>9}]  {:>8} tok\n",
            event.get("seq").and_then(Value::as_i64).unwrap_or(0),
            event
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            event
                .get("cumulative_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ));
        // `if e.get("tool_calls"):` — truthiness, so an EMPTY list prints
        // nothing at all rather than a bare `tools: `.
        if let Some(tools) = event.get("tool_calls").and_then(Value::as_array)
            && !tools.is_empty()
        {
            let joined: Vec<&str> = tools
                .iter()
                .map(|tool| tool.as_str().unwrap_or_default())
                .collect();
            out.push_str(&format!("        tools: {}\n", joined.join(", ")));
        }
        let preview = event
            .get("content_preview")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // `(… or "").splitlines()` — an empty string yields an EMPTY list, so
        // `if preview:` is false and no line is printed at all. A string of
        // only a newline yields `[""]`, which IS truthy and prints a blank.
        if let Some(first) = py_splitlines_first(preview) {
            out.push_str(&format!(
                "        {}\n",
                stax_etl::stats::pytext::py_char_prefix(first, 100)
            ));
        }
    }
    out
}

/// `str.splitlines()[0]`, or `None` when `splitlines()` is empty.
///
/// CPython breaks on eight boundaries beyond `\n` — `\r`, `\v`, `\f`, `\x1c`,
/// `\x1d`, `\x1e`, `\x85`, `\u2028`, `\u2029` — and `"".splitlines()` is `[]`
/// while `"\n".splitlines()` is `[""]`. Both edges are reachable:
/// `content_preview` is raw message text.
#[must_use]
pub fn py_splitlines_first(text: &str) -> Option<&str> {
    if text.is_empty() {
        return None;
    }
    let end = text
        .char_indices()
        .find(|(_, ch)| is_line_boundary(*ch))
        .map_or(text.len(), |(offset, _)| offset);
    Some(&text[..end])
}

const fn is_line_boundary(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r'
            | '\u{b}'
            | '\u{c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `_session_in_project` — one indexed probe, and any failure is `False`.
#[must_use]
pub fn session_in_project(conn: &Connection, session_id: &str, project_slug: &str) -> bool {
    // `except Exception: return False` — a store missing the join is advisory,
    // never fatal. The `?`-free shape is the reference's bare `except`.
    conn.query_row(
        "SELECT 1 FROM sessions s JOIN projects p ON p.id = s.project_id \
         WHERE s.session_id = ? AND p.slug = ? LIMIT 1",
        rusqlite::params![session_id, project_slug],
        |_| Ok(()),
    )
    .is_ok()
}

/// `_resolve_context_budget(context_budget)`.
#[must_use]
pub fn resolve_context_budget(flag: Option<&PyInt>) -> i64 {
    flag.map_or_else(
        || {
            let config =
                std::fs::read_to_string(stax_core::settings::app_dir().join("config.json"))
                    .ok()
                    .and_then(|raw| stax_core::queries::pyjson::loads(&raw));
            crate::memory::resolve_budget_default(
                stax_core::settings::env_var("DISCOVERY_BUDGET_TOKENS")
                    .ok_or(())
                    .ok()
                    .as_deref(),
                config.as_ref(),
            )
        },
        PyInt::saturating_i64,
    )
}

/// `f"{value!r}"` for the one shape the warning uses — a `str`.
fn py_repr(text: &str) -> String {
    stax_core::queries::paths::py_repr(text)
}

fn int_or_null(value: Option<&PyInt>) -> Value {
    value.map_or(Value::Null, |value| Value::from(value.saturating_i64()))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        args: ContextReplayArgs,
    }

    fn parse(argv: &[&str]) -> ContextReplayArgs {
        let mut all = vec!["x"];
        all.extend_from_slice(argv);
        Wrap::try_parse_from(all).expect("parse").args
    }

    fn result_with(count: usize) -> Value {
        let events: Vec<Value> = (1..=count)
            .map(|seq| {
                json!({
                    "seq": seq,
                    "role": if seq % 2 == 1 { "user" } else { "assistant" },
                    "content_preview": format!("line {seq}\nsecond line"),
                    "tokens": 3,
                    "cumulative_tokens": seq * 3,
                    "tool_calls": if seq == 2 { vec!["Edit a.py", "Read b.py"] } else { vec![] },
                })
            })
            .collect();
        json!({
            "session_id": "sess-a",
            "at_seq": Value::Null,
            "message_count": count,
            "total_tokens": count * 3,
            "events": events,
            "warnings": [],
        })
    }

    #[test]
    fn the_defaults_are_the_decorators() {
        let args = parse(&["sess-a"]);
        assert_eq!(args.session_id, "sess-a");
        assert!(args.at_seq.is_none());
        assert_eq!(args.limit.saturating_i64(), 100);
        assert!(args.context_budget.is_none());
        assert_eq!(args.format, "text");
        assert!(!args.as_json);
    }

    #[test]
    fn the_text_render_is_the_reference_f_strings() {
        let result = result_with(2);
        let events: Vec<Value> = result["events"].as_array().unwrap().clone();
        assert_eq!(
            render_text(&result, &events),
            concat!(
                "Context replay for sess-a (full session)\n",
                "  messages: 2   est. context tokens: 6\n",
                "\n",
                "  #   1 [     user]         3 tok\n",
                "        line 1\n",
                "  #   2 [assistant]         6 tok\n",
                "        tools: Edit a.py, Read b.py\n",
                "        line 2\n",
            ),
            "`:>4`, `:>9`, `:>8`, the eight-space indents and the splitlines head"
        );
    }

    #[test]
    fn an_at_seq_changes_the_scope_word() {
        let mut result = result_with(1);
        result["at_seq"] = json!(7);
        let events: Vec<Value> = result["events"].as_array().unwrap().clone();
        assert!(
            render_text(&result, &events).starts_with("Context replay for sess-a (up to seq 7)\n"),
            "`at_seq is not None`, and JSON null is None"
        );
    }

    #[test]
    fn no_events_prints_the_marker_and_no_blank_line() {
        let result = json!({
            "session_id": "nope", "at_seq": Value::Null,
            "message_count": 0, "total_tokens": 0, "events": [],
            "warnings": ["session nope is outside project 'p'"],
        });
        assert_eq!(
            render_text(&result, &[]),
            concat!(
                "Context replay for nope (full session)\n",
                "  messages: 0   est. context tokens: 0\n",
                "  ! session nope is outside project 'p'\n",
                "  (no messages in range)\n",
            )
        );
    }

    #[test]
    fn the_limit_heads_and_sets_truncated() {
        let result = result_with(5);
        let args = parse(&["sess-a", "--limit", "2", "--json", "--context-budget", "0"]);
        let out = emit(&args, &result, 0).stdout;
        assert!(out.contains("\"result_count\": 2"), "{out}");
        assert!(out.contains("\"truncated\": true"), "{out}");
    }

    #[test]
    fn a_zero_or_negative_limit_disables_the_head() {
        let result = result_with(5);
        for limit in ["0", "-1"] {
            let args = parse(&[
                "sess-a",
                "--limit",
                limit,
                "--json",
                "--context-budget",
                "0",
            ]);
            let out = emit(&args, &result, 0).stdout;
            assert!(out.contains("\"result_count\": 5"), "limit={limit}\n{out}");
        }
    }

    #[test]
    fn the_budget_always_keeps_the_first_event() {
        let result = result_with(5);
        let args = parse(&["sess-a", "--json", "--context-budget", "1"]);
        let out = emit(&args, &result, 1).stdout;
        assert!(
            out.contains("\"result_count\": 1"),
            "`if packed and …` tests emptiness first:\n{out}"
        );
        assert!(out.contains("\"truncated\": true"));
    }

    #[test]
    fn the_envelope_carries_the_five_extras_after_the_core_eight() {
        let result = result_with(1);
        let args = parse(&["sess-a", "--format", "json", "--context-budget", "0"]);
        let out = emit(&args, &result, 0).stdout;
        let core = out.find("\"truncated\"").expect("core field");
        for key in [
            "\"at_seq\"",
            "\"total_tokens\"",
            "\"message_count\"",
            "\"warnings\"",
        ] {
            assert!(
                out[core..].contains(key),
                "{key} missing after core:\n{out}"
            );
        }
        assert!(out.starts_with("{\n  \"schema\": \"staxtrace.memory/1\","));
        assert!(out.contains("\"command\": \"context-replay\""));
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn the_json_flag_and_the_format_option_are_the_same_switch() {
        let result = result_with(1);
        let a = emit(
            &parse(&["s", "--json", "--context-budget", "0"]),
            &result,
            0,
        );
        let b = emit(
            &parse(&["s", "--format", "json", "--context-budget", "0"]),
            &result,
            0,
        );
        assert_eq!(a.stdout, b.stdout);
    }

    #[test]
    fn splitlines_first_matches_cpythons_boundaries() {
        assert_eq!(py_splitlines_first(""), None, "\"\".splitlines() == []");
        assert_eq!(py_splitlines_first("\n"), Some(""), "\"\\n\" -> [\"\"]");
        assert_eq!(py_splitlines_first("a\nb"), Some("a"));
        assert_eq!(py_splitlines_first("a\rb"), Some("a"), "\\r is a boundary");
        assert_eq!(py_splitlines_first("a\u{b}b"), Some("a"), "\\v is one too");
        assert_eq!(py_splitlines_first("a\u{2028}b"), Some("a"), "U+2028");
        assert_eq!(py_splitlines_first("no breaks"), Some("no breaks"));
    }

    #[test]
    fn the_preview_is_sliced_by_characters_not_bytes() {
        let long = "é".repeat(150);
        let mut result = result_with(1);
        result["events"][0]["content_preview"] = json!(long);
        let events: Vec<Value> = result["events"].as_array().unwrap().clone();
        let text = render_text(&result, &events);
        let line = text
            .lines()
            .find(|line| line.starts_with("        é"))
            .expect("preview line");
        assert_eq!(
            line.chars().count(),
            8 + 100,
            "`[:100]` counts CHARACTERS: {line}"
        );
    }
}

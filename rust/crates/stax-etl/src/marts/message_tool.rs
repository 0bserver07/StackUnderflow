//! Port of `stackunderflow/etl/marts/message_tool.py` — one row per
//! `(message, tool_name, call_index)`.
//!
//! The first per-message-grain mart: it keeps the row-per-tool-call detail
//! `reports/optimize.py`'s detectors need without re-parsing `raw_json` on
//! every request — a scan that fans out across every monthly partition since
//! v008 turned `messages` into a UNION-ALL view.
//!
//! * `file_path` — `input.file_path` / `input.path` / `input.notebook_path`,
//!   or for `Task` the `subagent_type` (so the ghost-agent detector reads
//!   invoked agents straight off the mart).
//! * `byte_count` — for write-family tools the payload written; for
//!   output-producing tools the size of the *result*, matched on `tool_use_id`
//!   against the immediately-following message's `tool_result` blocks.
//! * `call_index` — 0-based **per tool name within the message**. Read, Edit,
//!   Read produces Read#0, Edit#0, Read#1, and `UNIQUE(message_id, tool_name,
//!   call_index)` is what makes `INSERT OR IGNORE` idempotent.
//!
//! Staleness caveat, carried over: a message re-parsed with a different tool
//! shape leaves its old rows behind, because `INSERT OR IGNORE` only adds.
//! `rebuild_from_scratch` clears the table first, so a full rebuild self-heals.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use super::{MartBuilder, max_event_id};

/// Tools whose `file_path` slot is the subagent being spawned, not a path.
const TASK_TOOLS: &[&str] = &["Task"];

/// Input keys that carry a filesystem path, in priority order.
const FILE_PATH_KEYS: &[&str] = &["file_path", "path", "notebook_path"];

/// One parsed `tool_use` block, normalised to the mart's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The block's `name`.
    pub tool_name: String,
    /// A path, a subagent name, or `None`.
    pub file_path: Option<String>,
    /// Payload or result size in bytes, or `None` when unsizeable.
    pub byte_count: Option<i64>,
    /// 0-based index of this call within the message, per tool name.
    pub call_index: i64,
}

/// Per-`(message, tool_name, call_index)` detail rows for usage events.
pub struct MessageToolMartBuilder;

struct WindowRow {
    message_id: Option<i64>,
    project_id: i64,
    session_id: String,
    ts: String,
    day: String,
    raw_json: Option<String>,
    next_raw_json: Option<String>,
}

impl MartBuilder for MessageToolMartBuilder {
    fn name(&self) -> &'static str {
        "message_tool"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        let rows = fetch_window(conn, since_event_id, max_id)?;

        let mut stmt = conn.prepare(
            r"
            INSERT OR IGNORE INTO message_tool_mart (
                message_id, project_id, session_id, ts, day,
                tool_name, file_path, byte_count, call_index
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )?;

        for r in &rows {
            let result_sizes = result_sizes(r.next_raw_json.as_deref());
            for tc in parse_tool_calls(r.raw_json.as_deref(), &result_sizes) {
                stmt.execute(rusqlite::params![
                    r.message_id,
                    r.project_id,
                    r.session_id,
                    r.ts,
                    r.day,
                    tc.tool_name,
                    tc.file_path,
                    tc.byte_count,
                    tc.call_index,
                ])?;
            }
        }

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM message_tool_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

/// `message_tool._fetch_window`.
///
/// `next_raw_json` is the correlated lookup for the message that immediately
/// follows the source message by `seq` — almost always the `tool_result` turn.
/// The subquery shape is §6b territory: each UNION-ALL branch of the `messages`
/// view uses its `(session_fk, seq)` index, and a "cleaner" join would not.
fn fetch_window(conn: &Connection, since_event_id: i64, max_id: i64) -> Result<Vec<WindowRow>> {
    let mut stmt = conn.prepare(
        r"
        SELECT e.source_message_fk AS message_id,
               e.project_id        AS project_id,
               e.session_id        AS session_id,
               e.ts                AS ts,
               e.day               AS day,
               m.raw_json          AS raw_json,
               (
                   SELECT m2.raw_json
                     FROM messages m2
                    WHERE m2.session_fk = m.session_fk
                      AND m2.seq > m.seq
                    ORDER BY m2.seq
                    LIMIT 1
               )                   AS next_raw_json
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
        ",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![since_event_id, max_id], |r| {
            Ok(WindowRow {
                message_id: r.get::<_, Option<i64>>("message_id")?,
                project_id: r.get::<_, Option<i64>>("project_id")?.unwrap_or_default(),
                session_id: r
                    .get::<_, Option<String>>("session_id")?
                    .unwrap_or_default(),
                ts: r.get::<_, Option<String>>("ts")?.unwrap_or_default(),
                day: r.get::<_, Option<String>>("day")?.unwrap_or_default(),
                raw_json: r.get::<_, Option<String>>("raw_json")?,
                next_raw_json: r.get::<_, Option<String>>("next_raw_json")?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `message_tool._parse_tool_calls` — pure function over one message's JSON.
#[must_use]
pub fn parse_tool_calls(
    raw_json: Option<&str>,
    sizes: &std::collections::HashMap<String, i64>,
) -> Vec<ToolCall> {
    let blocks = tool_use_blocks(raw_json);
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut per_tool: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut out: Vec<ToolCall> = Vec::new();
    for blk in &blocks {
        // `if not isinstance(name, str) or not name: continue`
        let Some(name) = blk
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let empty = serde_json::Map::new();
        let inp = blk
            .get("input")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let result_size = blk
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| sizes.get(id).copied());
        let idx = per_tool.get(name).copied().unwrap_or(0);
        per_tool.insert(name.to_string(), idx + 1);
        out.push(ToolCall {
            tool_name: name.to_string(),
            file_path: extract_file_path(name, inp),
            byte_count: extract_byte_count(name, inp, result_size),
            call_index: idx,
        });
    }
    out
}

/// `message_tool._tool_use_blocks`.
fn tool_use_blocks(raw_json: Option<&str>) -> Vec<serde_json::Map<String, Value>> {
    let Some(obj) = super::json::loads(raw_json) else {
        return Vec::new();
    };
    let Some(obj) = obj.as_object() else {
        return Vec::new();
    };
    let Some(msg) = obj.get("message").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(content) = msg.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(Value::as_object)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .cloned()
        .collect()
}

/// `message_tool._extract_file_path`.
fn extract_file_path(tool_name: &str, inp: &serde_json::Map<String, Value>) -> Option<String> {
    if TASK_TOOLS.contains(&tool_name) {
        for key in ["subagent_type", "agent"] {
            if let Some(v) = inp.get(key).and_then(Value::as_str)
                && !v.is_empty()
            {
                return Some(v.to_string());
            }
        }
        return None;
    }
    for key in FILE_PATH_KEYS {
        if let Some(v) = inp.get(*key).and_then(Value::as_str)
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

/// `message_tool._extract_byte_count`.
fn extract_byte_count(
    tool_name: &str,
    inp: &serde_json::Map<String, Value>,
    result_size: Option<i64>,
) -> Option<i64> {
    match tool_name {
        "Write" => byte_len(inp.get("content")),
        "Edit" => byte_len(inp.get("new_string")),
        "NotebookEdit" => byte_len(inp.get("new_source")),
        "MultiEdit" => {
            let edits = inp.get("edits").and_then(Value::as_array)?;
            let mut total = 0_i64;
            let mut seen = false;
            for e in edits {
                let Some(e) = e.as_object() else { continue };
                if let Some(n) = byte_len(e.get("new_string")) {
                    total += n;
                    seen = true;
                }
            }
            seen.then_some(total)
        }
        _ => result_size,
    }
}

/// `message_tool._byte_len` — UTF-8 byte length of a string value.
fn byte_len(value: Option<&Value>) -> Option<i64> {
    #[allow(clippy::cast_possible_wrap)]
    value.and_then(Value::as_str).map(|s| s.len() as i64)
}

/// `message_tool._result_sizes` — `{tool_use_id: result_byte_size}`.
#[must_use]
pub fn result_sizes(next_raw_json: Option<&str>) -> std::collections::HashMap<String, i64> {
    let mut out = std::collections::HashMap::new();
    let Some(obj) = super::json::loads(next_raw_json) else {
        return out;
    };
    let Some(obj) = obj.as_object() else {
        return out;
    };
    let Some(msg) = obj.get("message").and_then(Value::as_object) else {
        return out;
    };
    let Some(content) = msg.get("content").and_then(Value::as_array) else {
        return out;
    };
    for blk in content {
        let Some(b) = blk.as_object() else { continue };
        if b.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = b
            .get("tool_use_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        #[allow(clippy::cast_possible_wrap)]
        let n = render_result_content(b.get("content")).len() as i64;
        out.insert(id.to_string(), n);
    }
    out
}

/// `message_tool._render_result_content`.
fn render_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|p| p.as_object())
            .filter_map(|o| o.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn call_index_is_per_tool_name_within_the_message() {
        let raw = r#"{"message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"/a"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/b","new_string":"xy"}},
            {"type":"tool_use","name":"Read","input":{"file_path":"/c"}}
        ]}}"#;
        let calls = parse_tool_calls(Some(raw), &HashMap::new());
        let idx: Vec<(&str, i64)> = calls
            .iter()
            .map(|c| (c.tool_name.as_str(), c.call_index))
            .collect();
        assert_eq!(idx, vec![("Read", 0), ("Edit", 0), ("Read", 1)]);
        assert_eq!(calls[1].byte_count, Some(2));
        assert_eq!(calls[0].file_path.as_deref(), Some("/a"));
    }

    #[test]
    fn task_tools_record_the_subagent_not_a_path() {
        let raw = r#"{"message":{"content":[
            {"type":"tool_use","name":"Task","input":{"subagent_type":"Explore","file_path":"/x"}}
        ]}}"#;
        let calls = parse_tool_calls(Some(raw), &HashMap::new());
        assert_eq!(calls[0].file_path.as_deref(), Some("Explore"));
    }

    #[test]
    fn write_family_byte_counts_measure_the_payload_in_utf8() {
        let mk = |name: &str, input: &str| {
            let raw = format!(
                r#"{{"message":{{"content":[{{"type":"tool_use","name":"{name}","input":{input}}}]}}}}"#
            );
            parse_tool_calls(Some(&raw), &HashMap::new())[0].byte_count
        };
        assert_eq!(mk("Write", r#"{"content":"héllo"}"#), Some(6));
        assert_eq!(mk("Edit", r#"{"new_string":"abc"}"#), Some(3));
        assert_eq!(mk("NotebookEdit", r#"{"new_source":"ab"}"#), Some(2));
        assert_eq!(
            mk(
                "MultiEdit",
                r#"{"edits":[{"new_string":"ab"},{"new_string":"c"}]}"#
            ),
            Some(3)
        );
        // MultiEdit with nothing sizeable is NULL, not 0.
        assert_eq!(mk("MultiEdit", r#"{"edits":[{"old_string":"x"}]}"#), None);
        assert_eq!(mk("MultiEdit", r#"{"edits":"nope"}"#), None);
        // Output tools with no matched result are NULL.
        assert_eq!(mk("Bash", r#"{"command":"ls"}"#), None);
    }

    #[test]
    fn output_tools_take_their_size_from_the_matched_tool_result() {
        let next = r#"{"message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"12345"},
            {"type":"tool_result","tool_use_id":"t2","content":[{"text":"ab"},{"text":"c"}]}
        ]}}"#;
        let sizes = result_sizes(Some(next));
        assert_eq!(sizes.get("t1"), Some(&5));
        assert_eq!(sizes.get("t2"), Some(&3));

        let raw = r#"{"message":{"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{}},
            {"type":"tool_use","id":"zz","name":"Grep","input":{}}
        ]}}"#;
        let calls = parse_tool_calls(Some(raw), &sizes);
        assert_eq!(calls[0].byte_count, Some(5));
        assert_eq!(calls[1].byte_count, None);
    }

    #[test]
    fn malformed_shapes_yield_no_rows() {
        for raw in [
            None,
            Some("not json"),
            Some("[1,2]"),
            Some(r#"{"message":"str"}"#),
            Some(r#"{"message":{"content":"str"}}"#),
            Some(r#"{"message":{"content":[{"type":"text","text":"x"}]}}"#),
            Some(r#"{"message":{"content":[{"type":"tool_use"}]}}"#),
            Some(r#"{"message":{"content":[{"type":"tool_use","name":""}]}}"#),
            Some(r#"{"message":{"content":[{"type":"tool_use","name":5}]}}"#),
        ] {
            assert!(parse_tool_calls(raw, &HashMap::new()).is_empty(), "{raw:?}");
        }
        assert!(result_sizes(Some("not json")).is_empty());
    }

    #[test]
    fn re_running_a_window_adds_nothing() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        testdb::session(&c, 1, 1, "s1");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            r#"["Read"]"#,
            r#"{"message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}]}}"#,
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "user",
            "",
            "[]",
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"1234"}]}}"#,
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
            0.0,
        );

        MessageToolMartBuilder.refresh(&c, 0).unwrap();
        MessageToolMartBuilder.refresh(&c, 0).unwrap();
        let (n, bytes): (i64, i64) = c
            .query_row(
                "SELECT COUNT(*), SUM(byte_count) FROM message_tool_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "INSERT OR IGNORE must make a re-run a no-op");
        assert_eq!(bytes, 4, "byte_count comes from the FOLLOWING message");
    }
}

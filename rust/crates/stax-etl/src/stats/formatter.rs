//! Port of `python-legacy: stats/formatter.py` (RS-3-065) — enriched records
//! back to the dict shape the REST API serves.
//!
//! Fifteen keys per message, in `_record_to_dict`'s literal order, plus three
//! more stamped onto the records that begin an interaction. `json.dumps` writes
//! a dict in insertion order, so the order below is the wire contract and not a
//! style choice.
//!
//! # `id(rec)` is an index here
//!
//! Python builds `{id(ix.command): ix}` and looks each record up by object
//! identity, which is exactly "is this record the one this interaction was
//! opened on". `Interaction::command` is the index of that record in
//! `EnrichedDataset::records`, so the lookup is the same partition with no
//! hashing — and, unlike `id()`, it cannot be confused by a record that was
//! cloned.

use serde_json::{Map, Value};

use super::enricher::{EnrichedDataset, Record};

/// `formatter.to_dicts` — the message list, sorted by timestamp.
///
/// `limit` is applied AFTER the sort, exactly as Python applies it.
#[must_use]
pub fn to_dicts(ds: &EnrichedDataset, limit: Option<usize>) -> Vec<Value> {
    // `ix_by_cmd[id(ix.command)] = ix` — last interaction wins for a repeated
    // command index, which post-dedup cannot happen.
    let mut ix_by_cmd: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(ds.interactions.len());
    for (i, ix) in ds.interactions.iter().enumerate() {
        ix_by_cmd.insert(ix.command, i);
    }

    let mut dicts: Vec<(String, Value)> = Vec::with_capacity(ds.records.len());
    for (i, rec) in ds.records.iter().enumerate() {
        let mut d = record_to_dict(rec);
        if let Some(&ix_idx) = ix_by_cmd.get(&i) {
            let ix = &ds.interactions[ix_idx];
            #[allow(clippy::cast_possible_wrap)]
            d.insert(
                "interaction_tool_count".into(),
                (ix.tool_count as i64).into(),
            );
            d.insert("interaction_model".into(), ix.model.clone());
            #[allow(clippy::cast_possible_wrap)]
            d.insert(
                "interaction_assistant_steps".into(),
                (ix.assistant_steps as i64).into(),
            );
        }
        // `key=lambda m: m["timestamp"] if m["timestamp"] else ""` — a falsy
        // timestamp sorts as the empty string, which for our `String` field is
        // the same value.
        dicts.push((rec.timestamp.clone(), Value::Object(d)));
    }

    // `list.sort` is stable, so equal timestamps keep their record order.
    dicts.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<Value> = dicts.into_iter().map(|(_, v)| v).collect();
    if let Some(limit) = limit {
        out.truncate(limit);
    }
    out
}

/// `formatter._record_to_dict`.
fn record_to_dict(rec: &Record) -> Map<String, Value> {
    let mut d = Map::new();
    d.insert("session_id".into(), Value::String(rec.session_id.clone()));
    d.insert("type".into(), Value::String(rec.kind.clone()));
    d.insert("timestamp".into(), Value::String(rec.timestamp.clone()));
    d.insert("model".into(), rec.model.clone());
    d.insert("content".into(), Value::String(rec.content.clone()));
    d.insert("tools".into(), tools_to_json(rec));
    d.insert("tokens".into(), rec.tokens.to_json());
    d.insert("cwd".into(), rec.cwd.clone());
    d.insert("uuid".into(), rec.uuid.clone());
    d.insert("parent_uuid".into(), rec.parent_uuid.clone());
    d.insert("is_sidechain".into(), rec.is_sidechain.clone());
    d.insert("has_tool_result".into(), Value::Bool(rec.has_tool_result));
    d.insert("error".into(), Value::Bool(rec.is_error));
    d.insert("is_interruption".into(), Value::Bool(rec.is_interruption));
    d.insert("message_id".into(), rec.message_id.clone());
    d
}

/// `rec.tools` — the `{"name", "id", "input"}` dicts, in `_tools_from`'s key
/// order.
///
/// A [`super::enricher::Detail::Lean`] record has no blocks, and the list comes
/// out empty rather than as a run of placeholders. Nothing calls the formatter
/// on a lean dataset (`get_project_stats` builds a detailed one); the branch
/// exists so a future caller gets an empty list instead of a panic.
fn tools_to_json(rec: &Record) -> Value {
    Value::Array(
        rec.tools
            .iter()
            .filter_map(|t| t.block.as_ref())
            .map(|b| {
                let mut m = Map::new();
                m.insert("name".into(), b.name.clone());
                m.insert("id".into(), b.id.clone());
                m.insert("input".into(), b.input.clone());
                Value::Object(m)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classifier::{RawEntry, tag};
    use crate::stats::enricher::build_detailed;
    use serde_json::json;

    fn entry(payload: Value) -> RawEntry {
        RawEntry {
            payload,
            session_id: "s".into(),
            provider: "anthropic".into(),
        }
    }

    #[test]
    fn fifteen_keys_in_the_python_order_plus_three_on_a_command() {
        let ds = build_detailed(tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "uuid": "u1",
                         "cwd": "/w", "message": {"content": "ask"}})),
            entry(json!({"type": "assistant", "timestamp": "t2", "uuid": "u2",
                         "parentUuid": "u1", "isSidechain": true,
                         "message": {"id": "m2", "model": "claude-x",
                                     "content": [{"type": "tool_use", "id": "a",
                                                  "name": "Read", "input": {"p": 1}}]}})),
        ]));
        let msgs = to_dicts(&ds, None);
        assert_eq!(msgs.len(), 2);

        let cmd = msgs[0].as_object().expect("object");
        assert_eq!(
            cmd.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "session_id",
                "type",
                "timestamp",
                "model",
                "content",
                "tools",
                "tokens",
                "cwd",
                "uuid",
                "parent_uuid",
                "is_sidechain",
                "has_tool_result",
                "error",
                "is_interruption",
                "message_id",
                "interaction_tool_count",
                "interaction_model",
                "interaction_assistant_steps",
            ]
        );
        assert_eq!(cmd["interaction_tool_count"], json!(1));
        assert_eq!(cmd["interaction_assistant_steps"], json!(1));
        assert_eq!(cmd["interaction_model"], json!("claude-x"));
        assert_eq!(cmd["model"], json!("N/A"));
        assert_eq!(cmd["parent_uuid"], Value::Null);
        assert_eq!(cmd["is_sidechain"], json!(false));

        // A non-command record gets exactly fifteen keys.
        let asst = msgs[1].as_object().expect("object");
        assert_eq!(asst.len(), 15);
        assert_eq!(
            asst["tools"],
            json!([{"name": "Read", "id": "a", "input": {"p": 1}}])
        );
        assert_eq!(asst["is_sidechain"], json!(true));
        assert_eq!(asst["message_id"], json!("m2"));
    }

    #[test]
    fn the_sort_is_stable_on_equal_timestamps() {
        let ds = build_detailed(tag(vec![
            entry(json!({"type": "assistant", "timestamp": "t", "uuid": "b"})),
            entry(json!({"type": "assistant", "timestamp": "", "uuid": "a"})),
            entry(json!({"type": "assistant", "timestamp": "t", "uuid": "c"})),
        ]));
        let msgs = to_dicts(&ds, None);
        let uuids: Vec<&str> = msgs
            .iter()
            .map(|m| m["uuid"].as_str().unwrap_or(""))
            .collect();
        // The empty timestamp sorts first; the two "t" records keep input order.
        assert_eq!(uuids, vec!["a", "b", "c"]);
    }

    #[test]
    fn limit_is_applied_after_the_sort() {
        let ds = build_detailed(tag(vec![
            entry(json!({"type": "assistant", "timestamp": "t9", "uuid": "late"})),
            entry(json!({"type": "assistant", "timestamp": "t1", "uuid": "early"})),
        ]));
        let msgs = to_dicts(&ds, Some(1));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["uuid"], json!("early"));
    }
}

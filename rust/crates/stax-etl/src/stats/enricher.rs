//! Port of `stackunderflow/stats/enricher.py` — the mart-path subset.
//!
//! `project_mart`'s second pass calls `enricher.build(classifier.tag(...), "")`
//! and reads the resulting records and interactions, so the builder's five
//! steps are ported in order and with their ordering semantics intact.
//!
//! # What is ported, and what is deliberately not
//!
//! `Record` in Python carries 19 fields. The mart path reads eight of them
//! (`session_id`, `kind`, `timestamp`, `content`, `tokens["cache_read"]`,
//! `tools`, `is_error`/`error_category`, `has_tool_result`) plus one predicate
//! over `model`. Carrying the rest — `uuid`, `parent_uuid`, `is_sidechain`,
//! `message_id`, `cwd`, `raw_data`, `provider`, `speed`, `reasoning` tokens —
//! would mean copying every `raw_json` blob of a 383K-message store into a
//! struct nothing reads. They are listed here rather than silently dropped:
//! whoever finishes RS-3-064 (the full enricher, for the aggregator) adds them
//! back, and the extraction helpers they need (`_usage_from`, `_reasoning_from`,
//! `_speed_from`) are the ones already transcribed in the pricing port.
//!
//! Step 5 (`scan_sessions`) is skipped for the same reason: `EnrichedDataset.
//! sessions` has no reader on the mart path. It is a pure fold over
//! `records` and can be added without touching anything here.
//!
//! # Ordering is the contract
//!
//! `group_interactions` sorts by `timestamp or ""` with Python's *stable*
//! `sorted`, and the interaction chain it builds depends on the order of
//! equal-timestamp records. `slice::sort_by_key` is stable too, and the input
//! order is the SQL row order — which is why `_refresh_message_dims`' query
//! keeps its `ORDER BY m.timestamp` exactly as Python wrote it.

use serde_json::Value;

use super::classifier::TaggedEntry;
use super::pytext::{py_char_prefix, py_str, py_truthy};

/// One tool-use block, reduced to the field the interaction graph keys on.
///
/// Python keeps `{"name", "id", "input"}`; dedup (`finalise_tools`,
/// `_absorb_tools`) reads only `id`, and the mart path never reads `name` or
/// `input`. `id_key` is `None` when Python's `t.get("id", "")` would be falsy —
/// those blocks are never deduplicated and every occurrence counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    /// A key that compares equal exactly when Python's set membership would.
    ///
    /// Tagged by JSON type so a string `"5"` and a number `5` stay distinct the
    /// way two Python dict keys would.
    pub id_key: Option<String>,
}

/// One fully-parsed log entry (`enricher.Record`, mart-path subset).
#[derive(Debug, Clone)]
pub struct Record {
    /// `te.session_id`.
    pub session_id: String,
    /// `te.kind`.
    pub kind: String,
    /// `raw.get("timestamp", "")`.
    pub timestamp: String,
    /// `_text_from(raw)` — the flattening extraction, not the classifier's.
    pub content: String,
    /// Whether `tokens["cache_read"]` is truthy — the `cache.hit_rate` numerator.
    pub cache_read_truthy: bool,
    /// `_tools_from(msg)`.
    pub tools: Vec<ToolRef>,
    /// `te.is_error`.
    pub is_error: bool,
    /// `te.error_category`.
    pub error_category: Option<String>,
    /// `_has_result_block(msg)`.
    pub has_tool_result: bool,
    /// Python's `rec.model and rec.model != "N/A"`, precomputed.
    ///
    /// The model *string* has no reader on the mart path (it reaches only
    /// `command_details[].model` and the model distribution, neither of which
    /// is materialised), so only the predicate is carried.
    pub model_named: bool,
}

/// A user prompt and everything that followed until the next prompt
/// (`enricher.Interaction`, mart-path subset).
#[derive(Debug, Clone)]
pub struct Interaction {
    /// `f"{command.timestamp}|{command.content[:64]}"` — the *material* Python
    /// hashes into `interaction_id`, used directly as the identity.
    ///
    /// See [`build`] for why the hash itself is not computed.
    pub key: String,
    /// `len(responses)` — becomes `assistant_steps` in `finalise_tools`.
    pub responses: usize,
    /// Accumulated `tools_used`, deduplicated by [`finalise_tools`].
    pub tools_used: Vec<ToolRef>,
    /// `len(tools_used)` after dedup.
    pub tool_count: usize,
}

/// `enricher.EnrichedDataset` (mart-path subset — no `sessions`).
#[derive(Debug, Default)]
pub struct EnrichedDataset {
    /// Every record, in input order.
    pub records: Vec<Record>,
    /// The deduplicated interaction chains.
    pub interactions: Vec<Interaction>,
}

/// `enricher.build` — steps 1, 2, 3 and 4 (see the module docs for 5).
///
/// # The one deliberate substitution
///
/// Python identifies an interaction by
/// `sha256(f"{timestamp}|{content[:64]}").hexdigest()[:16]`, then
/// `_command_analysis` looks interactions up by the *unhashed* material. This
/// port keys on the material throughout, which is the same partition of the
/// input unless two distinct materials collide in 64 bits of SHA-256 — at which
/// point Python merges two unrelated interactions and this port does not. The
/// wave-3 full-row diff over the live store is the evidence that no such
/// collision exists there; the alternative was a hash dependency in the shared
/// workspace lock to reproduce a truncation that can only ever lose
/// information.
#[must_use]
pub fn build(tagged: Vec<TaggedEntry>) -> EnrichedDataset {
    // step 1 — extract_records
    let records: Vec<Record> = tagged.iter().map(parse_entry).collect();

    // step 2 — group_interactions
    let mut order: Vec<usize> = (0..records.len()).collect();
    // Python: `sorted(self.records, key=lambda r: r.timestamp or "")`, stable.
    order.sort_by(|&a, &b| records[a].timestamp.cmp(&records[b].timestamp));

    let mut interactions: Vec<Interaction> = Vec::new();
    let mut active: Option<Interaction> = None;
    for &i in &order {
        let rec = &records[i];
        if rec.kind == "summary" || rec.kind == "compact_summary" || rec.kind == "task" {
            continue;
        }
        let is_user_command = rec.kind == "user" && !rec.has_tool_result;
        if is_user_command {
            if let Some(prev) = active.take() {
                interactions.push(prev);
            }
            active = Some(Interaction {
                key: interaction_key(rec),
                responses: 0,
                tools_used: Vec::new(),
                tool_count: 0,
            });
            continue;
        }
        let Some(act) = active.as_mut() else { continue };
        if rec.kind == "assistant" {
            act.responses += 1;
            act.tools_used.extend(rec.tools.iter().cloned());
        }
        // `elif rec.has_tool_result:` appends to `tool_results`, which the mart
        // path does not read.
    }
    if let Some(last) = active.take() {
        interactions.push(last);
    }

    // step 3 — deduplicate_interactions (insertion-ordered, first-wins on ties)
    let mut keys: Vec<String> = Vec::new();
    let mut best: std::collections::HashMap<String, Interaction> = std::collections::HashMap::new();
    for ix in interactions {
        match best.remove(&ix.key) {
            None => {
                keys.push(ix.key.clone());
                best.insert(ix.key.clone(), ix);
            }
            Some(prev) => {
                // `(ix, prev) if len(ix.responses) > len(prev.responses) else (prev, ix)`
                let (mut winner, loser) = if ix.responses > prev.responses {
                    (ix, prev)
                } else {
                    (prev, ix)
                };
                absorb_tools(&mut winner, &loser);
                best.insert(winner.key.clone(), winner);
            }
        }
    }
    let mut interactions: Vec<Interaction> =
        keys.into_iter().filter_map(|k| best.remove(&k)).collect();

    // step 4 — finalise_tools
    for ix in &mut interactions {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut deduped: Vec<ToolRef> = Vec::with_capacity(ix.tools_used.len());
        for t in ix.tools_used.drain(..) {
            if let Some(id) = &t.id_key {
                if seen.contains(id) {
                    continue;
                }
                seen.insert(id.clone());
            }
            deduped.push(t);
        }
        ix.tool_count = deduped.len();
        ix.tools_used = deduped;
    }

    EnrichedDataset {
        records,
        interactions,
    }
}

/// `f"{rec.timestamp}|{rec.content[:64]}"` — the material behind `_make_id`
/// and the literal key `_command_analysis` builds its lookup table on.
#[must_use]
pub fn interaction_key(rec: &Record) -> String {
    format!("{}|{}", rec.timestamp, py_char_prefix(&rec.content, 64))
}

/// `enricher._absorb_tools`.
fn absorb_tools(winner: &mut Interaction, loser: &Interaction) {
    let mut existing: std::collections::HashSet<String> = winner
        .tools_used
        .iter()
        .filter_map(|t| t.id_key.clone())
        .collect();
    for t in &loser.tools_used {
        if let Some(id) = &t.id_key
            && !existing.contains(id)
        {
            winner.tools_used.push(t.clone());
            existing.insert(id.clone());
        }
    }
}

/// `enricher._parse_entry`.
fn parse_entry(te: &TaggedEntry) -> Record {
    let raw = &te.payload;
    // `msg = raw.get("message") if isinstance(raw.get("message"), dict) else {}`
    let msg = raw.get("message").and_then(Value::as_object);

    // `_usage_from(msg)["cache_read"]`, then `if rec.tokens.get("cache_read", 0)`.
    // `usage.get(k, 0) or 0` is truthy exactly when the wire value is truthy.
    let cache_read_truthy = msg
        .and_then(|m| m.get("usage"))
        .and_then(Value::as_object)
        .and_then(|u| u.get("cache_read_input_tokens"))
        .is_some_and(py_truthy);

    // `msg.get("model", "N/A") if msg else "N/A"` then `model and model != "N/A"`.
    // `if msg` is truthiness: an empty dict takes the "N/A" branch.
    let model_named = match msg {
        Some(m) if !m.is_empty() => match m.get("model") {
            None => false, // defaults to "N/A"
            Some(v) => py_truthy(v) && v.as_str() != Some("N/A"),
        },
        _ => false,
    };

    Record {
        session_id: te.session_id.clone(),
        kind: te.kind.clone(),
        timestamp: match raw.get("timestamp") {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            // Python keeps the raw value; a non-string one makes the
            // `sorted(..., key=lambda r: r.timestamp or "")` in step 2 raise
            // `TypeError` as soon as a string timestamp is present too, so
            // Python cannot complete a refresh that reaches this branch on a
            // mixed project. Coercing keeps the port total; the gate counts
            // occurrences (zero on the live store).
            Some(other) => py_str(other),
        },
        content: text_from(raw),
        cache_read_truthy,
        tools: tools_from(msg),
        is_error: te.is_error,
        error_category: te.error_category.clone(),
        has_tool_result: has_result_block(msg),
        model_named,
    }
}

/// `enricher._text_from` — readable text from a JSONL entry.
///
/// Distinct from `classifier._surface_text`: this one renders `tool_use` as
/// `[Tool: name]` and recurses into `tool_result` content. Both exist in
/// Python; this is the one that becomes `Record.content`, and therefore the one
/// the interruption tally and the interaction identity are computed from.
#[must_use]
pub fn text_from(raw: &Value) -> String {
    if let Some(s) = raw.get("summary").and_then(Value::as_str) {
        return s.to_string();
    }
    let Some(msg) = raw.get("message").and_then(Value::as_object) else {
        return String::new();
    };
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => flatten_content_blocks(items).join("\n"),
        _ => String::new(),
    }
}

/// `enricher._flatten_content_blocks`.
fn flatten_content_blocks(blocks: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for blk in blocks {
        match blk {
            Value::String(s) => {
                out.push(s.clone());
                continue;
            }
            Value::Object(o) => {
                let bt = o.get("type").and_then(Value::as_str).unwrap_or("");
                match bt {
                    "text" => out.push(
                        o.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    "tool_use" => {
                        // f"[Tool: {blk.get('name', '?')}]" — `name` need not be
                        // a string, and Python's f-string calls `str()` on it.
                        let name = match o.get("name") {
                            None => "?".to_string(),
                            Some(v) => py_str(v),
                        };
                        out.push(format!("[Tool: {name}]"));
                    }
                    "tool_result" => match o.get("content") {
                        Some(Value::String(s)) => out.push(s.clone()),
                        Some(Value::Array(inner)) => out.extend(flatten_content_blocks(inner)),
                        _ => {}
                    },
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

/// `enricher._tools_from`.
fn tools_from(msg: Option<&serde_json::Map<String, Value>>) -> Vec<ToolRef> {
    let Some(msg) = msg else { return Vec::new() };
    let Some(body) = msg.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    body.iter()
        .filter_map(Value::as_object)
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|o| ToolRef {
            // `blk.get("id", "")` then `if tid:` — falsy ids never dedup.
            id_key: match o.get("id") {
                Some(v) if py_truthy(v) => Some(match v {
                    Value::String(s) => format!("s:{s}"),
                    other => format!("v:{other}"),
                }),
                _ => None,
            },
        })
        .collect()
}

/// `enricher._has_result_block`.
#[must_use]
pub fn has_result_block(msg: Option<&serde_json::Map<String, Value>>) -> bool {
    msg.and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|body| {
            body.iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// `_has_result_block` over a raw payload's `message`, the shape both
/// `_count_message_dims` and `command.py::_is_user_command` use.
#[must_use]
pub fn has_result_block_of(raw: &Value) -> bool {
    has_result_block(raw.get("message").and_then(Value::as_object))
}

/// `_tools_from(msg)` over a raw payload — non-empty iff the turn used tools.
#[must_use]
pub fn tools_from_raw(raw: &Value) -> Vec<ToolRef> {
    tools_from(raw.get("message").and_then(Value::as_object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classifier::{RawEntry, tag};
    use serde_json::json;

    fn entry(payload: Value) -> RawEntry {
        RawEntry {
            payload,
            session_id: "s".into(),
            provider: "anthropic".into(),
        }
    }

    #[test]
    fn text_from_flattens_tool_blocks_and_joins_with_newlines() {
        let raw = json!({"message": {"content": [
            {"type": "text", "text": "one"},
            {"type": "tool_use", "name": "Read"},
            {"type": "tool_result", "content": [{"type": "text", "text": "two"}]},
            "bare",
        ]}});
        assert_eq!(text_from(&raw), "one\n[Tool: Read]\ntwo\nbare");
        assert_eq!(text_from(&json!({"summary": "s"})), "s");
        assert_eq!(text_from(&json!({"message": {"content": "flat"}})), "flat");
        assert_eq!(text_from(&json!({})), "");
        // missing name renders the literal '?'
        assert_eq!(
            text_from(&json!({"message": {"content": [{"type": "tool_use"}]}})),
            "[Tool: ?]"
        );
    }

    #[test]
    fn interactions_chain_from_user_turns_and_count_assistant_steps() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(json!({"type": "assistant", "timestamp": "t2", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t3", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
            entry(json!({"type": "human", "timestamp": "t4", "message": {"content": "again"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.records.len(), 4);
        assert_eq!(ds.interactions.len(), 2);
        assert_eq!(ds.interactions[0].responses, 2);
        assert_eq!(ds.interactions[0].tool_count, 2);
        assert_eq!(ds.interactions[1].responses, 0);
    }

    #[test]
    fn tool_result_turns_do_not_start_an_interaction() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(json!({"type": "human", "timestamp": "t2", "message": {
                "content": [{"type": "tool_result", "content": "out"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t3", "message": {"content": "ok"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        assert_eq!(ds.interactions[0].responses, 1);
    }

    #[test]
    fn duplicate_interactions_merge_with_the_longer_response_chain_winning() {
        // Same timestamp + same first 64 chars = same identity. Every record
        // shares a timestamp so the stable sort leaves them in input order and
        // both chains are non-empty — the shape the merge actually has to
        // decide between.
        let dup = json!({"type": "human", "timestamp": "t", "message": {"content": "ask"}});
        let tagged = tag(vec![
            entry(dup.clone()),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(dup),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {"content": "more"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        // Chain 1 has 1 response, chain 2 has 2 — strictly greater, so chain 2
        // wins. (`>` not `>=`: a tie keeps the FIRST.)
        assert_eq!(ds.interactions[0].responses, 2);
        // …and the loser's tool is absorbed rather than dropped.
        assert_eq!(ds.interactions[0].tool_count, 2);
    }

    #[test]
    fn a_tie_on_response_count_keeps_the_first_chain() {
        let dup = json!({"type": "human", "timestamp": "t", "message": {"content": "ask"}});
        let tagged = tag(vec![
            entry(dup.clone()),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(dup),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        assert_eq!(ds.interactions[0].responses, 1);
        assert_eq!(ds.interactions[0].tool_count, 2);
    }

    #[test]
    fn tools_without_an_id_are_never_deduplicated() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(
                json!({"type": "assistant", "timestamp": "t2", "message": {"content": [
                    {"type": "tool_use", "name": "Read"},
                    {"type": "tool_use", "name": "Read"},
                    {"type": "tool_use", "id": "x", "name": "Edit"},
                    {"type": "tool_use", "id": "x", "name": "Edit"},
                ]}}),
            ),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions[0].tool_count, 3);
    }

    #[test]
    fn cache_read_truthiness_follows_the_wire_value() {
        let mk = |usage: Value| {
            let ds = build(tag(vec![entry(
                json!({"type": "assistant", "message": {"usage": usage}}),
            )]));
            ds.records[0].cache_read_truthy
        };
        assert!(mk(json!({"cache_read_input_tokens": 12})));
        assert!(!mk(json!({"cache_read_input_tokens": 0})));
        assert!(!mk(json!({"cache_read_input_tokens": null})));
        assert!(!mk(json!({})));
    }

    #[test]
    fn stable_sort_keeps_equal_timestamps_in_input_order() {
        // Two user turns sharing a timestamp: the chain that forms depends on
        // which one the sort leaves first, and Python's `sorted` is stable.
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t", "message": {"content": "first"}})),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {"content": "reply"}})),
            entry(json!({"type": "human", "timestamp": "t", "message": {"content": "second"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 2);
        assert_eq!(ds.interactions[0].key, "t|first");
        assert_eq!(ds.interactions[0].responses, 1);
        assert_eq!(ds.interactions[1].responses, 0);
    }
}

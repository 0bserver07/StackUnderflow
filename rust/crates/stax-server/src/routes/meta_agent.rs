//! `routes/meta_agent.py` — 2 endpoints, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-077` | `GET ` | `/api/meta-agent/tools` | `/api/meta-agent/tools` | ported |
//! | `RS-5-078` | `POST` | `/api/meta-agent/chat ` | `/api/meta-agent/chat`  | **open** — DIV-138 |
//!
//! # The catalogue is the wire contract, so it is transcribed as data
//!
//! `GET /api/meta-agent/tools` returns `services/meta_agent.TOOL_CATALOG`
//! verbatim — 15 OpenAI-shaped tool schemas, a 533-line dict literal in the
//! reference — plus the names and the hop cap. There is nothing to compute: the
//! endpoint is a constant, and the only way to be byte-parity on a constant is
//! to hold the same constant.
//!
//! So [`TOOL_CATALOG_JSON`] is that literal, extracted once with
//! `ast.literal_eval` + `json.dumps` (never re-typed by hand) and parsed here
//! with `preserve_order` on, which keeps every key in the Python dict's
//! insertion order. Round-tripping data is the safe direction; paraphrasing a
//! 15 KB schema would not be.
//!
//! The risk this carries is honest and it is guarded: an upstream edit to the
//! catalogue would leave this copy stale. `MA-tools` is a deterministic,
//! read-only case row, so the differ reports that on the very next run — which
//! is a better tripwire than a comment asking someone to remember.
//!
//! # `POST /api/meta-agent/chat` — DIV-138, deferred whole
//!
//! It is not a JSON endpoint. It opens an `application/x-ndjson` stream, drives
//! a tool-call loop against a *live LLM* (`embeddings._resolve_endpoints()` —
//! cloud when configured, local Ollama otherwise), and executes store-reading
//! tools between hops through `services/meta_agent.py`'s 1,410-line executor.
//! Every one of those is out of an endpoint batch's scope on its own:
//!
//! * the body is a function of a model's sampling, so no two runs agree even
//!   against one server, let alone two;
//! * the loop is a network call the differ must not make (the `/ollama-api`
//!   precedent, DIV-066);
//! * and the executor is the largest unported service behind any route here.
//!
//! Its three 400 legs (bad JSON, empty `messages`, empty `model`) *are*
//! deterministic and would have been cheap rows — but half-porting a handler so
//! that three inputs answer and the rest 404 is worse than an honestly dark
//! surface, which is the ruling `!A-*` / DIV-082 already set for
//! `routes/agent_teams.py`. The path stays unmounted and `!MA-chat-*` reports
//! the gap every run. Both rows are safe to execute: Rust 404s without reading
//! anything, and Python rejects on validation before it opens a socket.

use axum::Router;
use axum::routing::get;
use serde_json::{Map, Value};

use crate::json::JsonBody;
use crate::state::AppState;

/// `MAX_TOOL_HOPS`.
const MAX_TOOL_HOPS: i64 = 5;

/// Mount this module's endpoints onto `router`.
///
/// `/api/meta-agent/chat` is deliberately absent — see the module docs
/// (DIV-138).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/meta-agent/tools", get(list_meta_agent_tools))
}

// ── GET /api/meta-agent/tools ────────────────────────────────────────────────

async fn list_meta_agent_tools() -> JsonBody {
    let catalog = tool_catalog();
    let names: Vec<Value> = catalog
        .iter()
        // `[t["function"]["name"] for t in TOOL_CATALOG]` — a `KeyError` on a
        // malformed entry in Python; here a malformed entry would be dropped,
        // which the length assertion in the tests forbids from ever mattering.
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .cloned()
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("tools".to_owned(), Value::Array(catalog.clone()));
    payload.insert("names".to_owned(), Value::Array(names));
    payload.insert("max_hops".to_owned(), Value::from(MAX_TOOL_HOPS));
    JsonBody::ok(Value::Object(payload))
}

/// The parsed catalogue, built once.
fn tool_catalog() -> &'static Vec<Value> {
    static CATALOG: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        // `preserve_order` is on (wave-0 decision, spec §6), so the parse keeps
        // the literal's key order and the re-render is byte-identical.
        match serde_json::from_str::<Value>(TOOL_CATALOG_JSON) {
            Ok(Value::Array(items)) => items,
            // Unreachable: the constant is checked by the tests below. An empty
            // catalogue is the visible failure, not a panic in a request.
            _ => Vec::new(),
        }
    })
}

/// `services/meta_agent.TOOL_CATALOG`, extracted mechanically — see the module docs.
const TOOL_CATALOG_JSON: &str = r#"
    [
        {
            "type": "function",
            "function": {
                "name": "search_past_decisions",
                "description": "Search the user's StackUnderflow store for past sessions whose messages mention a free-form query string. Returns a ranked list of matching sessions with project / cost / a short content excerpt. Use this for 'have I dealt with X before?' style questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Free-form text. Empty returns no matches."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions to return (1..20). Default 5.",
                            "minimum": 1,
                            "maximum": 20
                        },
                        "project": {
                            "type": "string",
                            "description": "Optional project slug filter (matches ``projects.slug``)."
                        },
                        "since": {
                            "type": "string",
                            "description": "Optional cutoff. ``\"7d\"`` / ``\"30d\"`` / ``\"24h\"`` or an ISO timestamp."
                        }
                    },
                    "required": [
                        "query"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_sessions_in_path",
                "description": "List the user's StackUnderflow sessions whose project filesystem path is ``path`` or any ancestor of it. Useful for 'show me what happened in this repo' / 'recent activity in this directory' questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or tilde-prefixed filesystem path (e.g. ``~/dev/myproj``)."
                        },
                        "since": {
                            "type": "string",
                            "description": "Optional cutoff (``\"30d\"`` / ``\"24h\"`` / ISO). Default: no cutoff."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions returned. Default 5.",
                            "minimum": 1,
                            "maximum": 20
                        }
                    },
                    "required": [
                        "path"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_sessions_touching_file",
                "description": "List sessions where ``file`` shows up as a tool argument (Read / Edit / Write) or in free-form message text. Use this for 'who touched X' / 'when did we last edit Y' questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "Path to the file (absolute or relative)."
                        },
                        "mode": {
                            "type": "string",
                            "description": "``\"read\"`` (only Read-tool hits), ``\"write\"`` (Edit/Write/MultiEdit/NotebookEdit hits), or ``\"any\"`` (default).",
                            "enum": [
                                "read",
                                "write",
                                "any"
                            ]
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions returned. Default 5.",
                            "minimum": 1,
                            "maximum": 20
                        }
                    },
                    "required": [
                        "file"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_project_summary",
                "description": "Return a flat summary for one project: session count, message count, lifetime cost in USD, first / last activity. Use for 'what's the state of this project?' / 'how big is X?'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "slug": {
                            "type": "string",
                            "description": "Project slug (e.g. ``\"my-project\"``). When omitted, summarises the current project context if one is available; otherwise returns an error."
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_cost_summary",
                "description": "Cross-project cost rollup over a fixed period. Returns ``total_cost`` USD plus per-project breakdown. Use for 'what did I spend this month?' / 'top spenders'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "period": {
                            "type": "string",
                            "description": "One of: ``\"today\"``, ``\"7days\"``, ``\"30days\"``, ``\"month\"`` (default), ``\"all\"``.",
                            "enum": [
                                "today",
                                "7days",
                                "30days",
                                "month",
                                "all"
                            ]
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Top-N projects to include. Default 10.",
                            "minimum": 1,
                            "maximum": 25
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_session_playback",
                "description": "Reconstruct what the AI agent did to the filesystem in session ``session_id`` up to time ``at``. Returns a list of touched files with metadata (no file bodies — those would blow the budget). Use for 'what did the agent change in session X?'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID (matches ``sessions.session_id``)."
                        },
                        "at": {
                            "type": "string",
                            "description": "ISO timestamp cutoff. Default: ``null`` means end-of-session (no cutoff applied)."
                        }
                    },
                    "required": [
                        "session_id"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "recommend_mode",
                "description": "Recommend the cheapest model that fits a task, based on the user's own past sessions. Pattern-matches the prompt's intent + token-band + language hints against past similar sessions and returns the model whose similar history had the lowest median cost. Returns confidence=0.0 when there isn't enough historical data (no opinion). Use this for 'this task fits a Sonnet, you used Opus' routing nudges.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The task prompt to score. Required, non-empty."
                        },
                        "current_model": {
                            "type": "string",
                            "description": "The model the caller would otherwise route to. Drives the cost_delta_usd field."
                        }
                    },
                    "required": [
                        "prompt"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_burn_projection",
                "description": "Project month-end spend against the user's plan and tell them if they're tracking to overrun. Returns the active plan, the current period's used / budget / remaining, the projected month-end total, the daily burn rate that fed the projection, the projection method (``linear`` or ``weighted-7d``), the estimated days until the plan limit at current burn, and (when crossed) the highest alert threshold. Use this for 'will I overrun this month?' / 'how am I tracking on Claude Pro?' style questions.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_recent_sessions",
                "description": "Return the most recently active sessions across the store. Use this for 'what did I work on lately?' style questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "project": {
                            "type": "string",
                            "description": "Optional project-slug filter."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions returned. Default 10.",
                            "minimum": 1,
                            "maximum": 25
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "recommend_skills",
                "description": "List repeated workflow patterns the user could turn into auto-generated Claude Code skills. Mines the local store for patterns appearing in ``threshold``+ distinct sessions within ``window_days`` and filters out anything they already have a skill for. Read-only — each row carries an ``accept_command`` the user can paste to install. Use for 'what should I automate?' / 'any skill suggestions for this project?'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "project": {
                            "type": "string",
                            "description": "Project slug to scope to. When omitted, the current project context is used if available; otherwise the call returns an error."
                        },
                        "threshold": {
                            "type": "integer",
                            "description": "Minimum distinct sessions a pattern must clear. Default 5.",
                            "minimum": 1,
                            "maximum": 50
                        },
                        "window_days": {
                            "type": "integer",
                            "description": "Lookback window in days. Default 30.",
                            "minimum": 1,
                            "maximum": 365
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_pr_outcomes",
                "description": "List PR outcomes for a repo from the local store (Spec 20 ingest). Returns the most recent PRs first, optionally filtered by ``state`` (open/merged/closed) and a ``since`` cutoff. Use this for 'what PRs landed in repo X?' / 'are there open PRs against this repo?' questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "GitHub / GitLab repo slug (``owner/repo``). Required — there is no implicit 'all repos' mode."
                        },
                        "state": {
                            "type": "string",
                            "description": "Optional state filter: ``open`` / ``merged`` / ``closed``. Default: no filter.",
                            "enum": [
                                "open",
                                "merged",
                                "closed"
                            ]
                        },
                        "since": {
                            "type": "string",
                            "description": "Optional cutoff. ``\"7d\"`` / ``\"30d\"`` / ``\"24h\"`` or an ISO timestamp. Filters on ``merged_at`` when present, falling back to row insert order."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max rows returned. Default 10.",
                            "minimum": 1,
                            "maximum": 50
                        }
                    },
                    "required": [
                        "repo"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_ci_runs",
                "description": "List CI runs from the local store (Spec 20 ingest). Filter by ``commit_sha`` (every workflow run that touched a commit) or ``status`` (success / failure / cancelled / in_progress / pending / skipped). Use this for 'did CI pass on commit X?' / 'show me recent failures' questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "commit_sha": {
                            "type": "string",
                            "description": "Optional commit SHA filter. Matches the ``commit_sha`` column exactly (full SHA)."
                        },
                        "status": {
                            "type": "string",
                            "description": "Optional status filter.",
                            "enum": [
                                "success",
                                "failure",
                                "cancelled",
                                "in_progress",
                                "pending",
                                "skipped"
                            ]
                        },
                        "repo": {
                            "type": "string",
                            "description": "Optional ``owner/repo`` slug filter."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max rows returned. Default 10.",
                            "minimum": 1,
                            "maximum": 50
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_file_risk",
                "description": "Risk summary for a file before you edit it: how many past sessions reverted / failed / worked. Returns counts plus up to five recent failure-mode session ids. Read those with ``get_session_playback`` to learn the trap before falling into it. Use whenever the user asks about a file with a rocky history (\"have I broken cost.py before?\").",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or working-directory-relative file path. ``~`` is expanded."
                        },
                        "since": {
                            "type": "string",
                            "description": "Optional cutoff. ``\"30d\"`` / ``\"7d\"`` / ``\"24h\"`` or an ISO timestamp. Default: no cutoff."
                        }
                    },
                    "required": [
                        "path"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_session_quality",
                "description": "Return the persisted static-analysis findings for one session (cyclomatic complexity, lint count, type completeness — pre/post deltas per touched file). Use this to answer 'did the agent improve or regress code quality in session X?' / 'how complex was the change?'. Returns an empty findings list when the session hasn't been analyzed yet (suggest the user run ``stackunderflow analyze session <id>``).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID (matches ``sessions.session_id``)."
                        }
                    },
                    "required": [
                        "session_id"
                    ]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "recommend_model_for_task",
                "description": "Recommend which model to use for a described task, based on the user's own OUTCOMES — not just cost. Where ``recommend_mode`` ranks on cost alone, this consults the comparative benchmark: for the matching task stratum (intent × size) it returns the model that historically won on the composite of success, cost-per-successful-outcome, and effort, with its evidence. Returns 'insufficient_evidence' honestly when the user's history can't support a call. Use for 'which model should I use for this refactor?' style routing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "intent": {
                            "type": "string",
                            "description": "Task intent. One of build / fix / explore / refactor / test / ops. Required.",
                            "enum": [
                                "build",
                                "fix",
                                "explore",
                                "refactor",
                                "test",
                                "ops"
                            ]
                        },
                        "size": {
                            "type": "string",
                            "description": "Optional task size band: tiny / small / med / large. Narrows to that stratum when given.",
                            "enum": [
                                "tiny",
                                "small",
                                "med",
                                "large"
                            ]
                        },
                        "language": {
                            "type": "string",
                            "description": "Optional dominant language hint (e.g. python)."
                        }
                    },
                    "required": [
                        "intent"
                    ]
                }
            }
        }
    ]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The names, read out of `services/meta_agent.py` itself.
    ///
    /// Not a second copy of the list: the source of truth is scanned for the
    /// `"name": "…"` entries inside `TOOL_CATALOG`, so an upstream rename fails
    /// here as well as in the differ.
    fn declared_names() -> Vec<String> {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow/services/meta_agent.py");
        // The Python reference lives on the python-legacy branch since the
        // split (2026-08-06); this cross-check runs where a reference checkout
        // exists and skips (empty) elsewhere.
        let Ok(text) = std::fs::read_to_string(source) else {
            return Vec::new();
        };
        let body = text
            .split_once("TOOL_CATALOG: list[dict[str, Any]] = [")
            .expect("the catalogue exists")
            .1;
        let body = body.split_once("\ndef ").map_or(body, |(head, _)| head);
        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                // The tool name sits on a `"name": "x",` line directly under
                // `"function": {`; parameter properties never use that key at
                // this indentation, so the leading-quote test is enough.
                let rest = line.strip_prefix("\"name\": \"")?;
                rest.split_once('"').map(|(name, _)| name.to_owned())
            })
            .collect()
    }

    #[test]
    fn the_embedded_catalogue_is_the_pythons_catalogue() {
        let catalog = tool_catalog();
        assert_eq!(catalog.len(), 15, "the catalogue lost or gained a tool");
        let ported: Vec<String> = catalog
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap_or("").to_owned())
            .collect();
        let declared = declared_names();
        if declared.is_empty() {
            eprintln!("SKIP catalogue cross-check — python reference absent (python-legacy)");
            return;
        }
        assert_eq!(ported, declared);
    }

    #[test]
    fn every_entry_is_an_openai_function_schema() {
        for tool in tool_catalog() {
            assert_eq!(tool["type"], Value::from("function"), "{tool:?}");
            let function = &tool["function"];
            assert!(function["name"].is_string(), "{tool:?}");
            assert!(function["description"].is_string(), "{tool:?}");
            assert_eq!(
                function["parameters"]["type"],
                Value::from("object"),
                "{tool:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_payload_is_the_three_keys_in_the_literals_order() {
        let rendered = list_meta_agent_tools().await.render();
        assert!(
            rendered.starts_with(r#"{"tools":[{"type":"function","#),
            "{rendered:.80}"
        );
        assert!(
            rendered.contains(r#""names":["search_past_decisions","#),
            "names block"
        );
        assert!(rendered.ends_with(r#""max_hops":5}"#), "{rendered:.80}");
        // The whole catalogue is inside one response — a size worth knowing,
        // since it is the largest constant body the server serves.
        assert!(
            rendered.len() > 10_000,
            "suspiciously small: {}",
            rendered.len()
        );
    }

    #[test]
    fn key_order_survives_the_json_round_trip() {
        // `preserve_order` is the reason this endpoint can be a constant at
        // all. If it were ever off, the first tool's keys would come back
        // alphabetised and every byte after the first brace would move.
        let first = &tool_catalog()[0];
        let keys: Vec<&str> = first
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["type", "function"]);
        let function_keys: Vec<&str> = first["function"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(function_keys, ["name", "description", "parameters"]);
    }
}

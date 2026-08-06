---
name: recall-past-decisions
description: Use when the user references a PAST DECISION, choice, or rationale — "what did we decide about caching?", "why is X done this way?", "didn't we agree to use Y?", "remind me how we handled Z before". Searches indexed message history across all prior sessions to find the conversation where the decision was made and surfaces the matching excerpt with timestamp.
---

# Recall a past decision from prior sessions

## Why this skill exists

The user often references decisions made in past sessions ("we decided to use SQLite", "I told you to skip X", "we landed on the Y approach last week"). Without searching history, the agent either guesses (often wrong) or asks the user to re-explain — both are bad. StackUnderflow indexes every message across every session; full-text search returns the matching excerpt + the session where it was said, so the agent can cite the decision back to the user.

## When this skill fires

Trigger when the user references a past conversation or decision, signaled by phrases like:

- "What did we decide about X?"
- "Why is X done this way?" (when "this way" implies a prior choice, not a code-level question)
- "Didn't we agree to / talk about / land on X?"
- "Remind me how we handled X before"
- "What was the rationale for X?"
- "I think we discussed this last week"

Do NOT fire for:

- Code-level questions answered by reading the current file ("why does this function take an int?" → just read the code)
- Factual questions about a library or API ("how does asyncio work?")
- The user gave the answer in the same turn ("we decided to use SQLite — implement that")

## Step 1: search past sessions

```bash
stackunderflow search-past-decisions "<query>" --format json --limit 10
```

Build the query from the user's reference — the substantive nouns/verbs, not the question scaffolding. Examples:

| User says | Query |
|---|---|
| "What did we decide about caching?" | `caching` or `cache strategy` |
| "Why are we using SQLite not Postgres?" | `SQLite Postgres` or `database choice` |
| "Didn't we agree the watcher should be opt-in?" | `watcher opt-in` |
| "How did we handle the partition migration?" | `partition migration` |

Optional flags:
- `--project SLUG` — restrict to one project (use the slug from `find-sessions-in-path`)
- `--since 30d` — restrict to recent history (default: all-time)
- `--limit N` — how many excerpts to return (default 10 is usually fine)

JSON shape:

```json
{
  "sessions": [
    {"session_id": "...", "project_slug": "...", "project_path": "...",
     "provider": "claude", "first_ts": "...", "last_ts": "...",
     "message_count": 42, "cost_usd": 0.85,
     "snippet": "matching excerpt with the query terms in context"}
  ]
}
```

For this command the `snippet` is the matching message excerpt — not the first user message. That's the substantive payload.

## Step 2: read the excerpts

Scan the `snippet` field on each result for the actual decision text. The full message lives in the session log if you need to drill in, but usually the excerpt is enough.

If `sessions` is empty: tell the user you couldn't find the prior decision. Don't fabricate one. Ask them to refresh your memory.

## Step 3: cite the decision back to the user

Format the answer with the citation:

> Yes — on 2026-04-22 you and the prior session landed on SQLite (session `abc123…`). The reasoning was [snippet excerpt]. Proceeding on that basis.

Why citation matters: the user can verify against the linked session. If the agent paraphrased a decision incorrectly, the user catches it immediately. Without the timestamp/session_id, the agent's recall is unverifiable and gets treated as a hallucination.

## Refining the search

If the first result set isn't useful:

- Broaden the query (drop a word)
- Drop the `--project` filter if the decision spanned projects
- Drop `--since` if the decision is older than the default window
- Try synonyms — "auth" vs "authentication", "cache" vs "caching"

Three retries is the budget. After that, tell the user the search came up empty and ask for a hint.

## Showing the user vs. consuming internally

Default `--format json` is for agent consumption. When the user asks "show me what we said about X" or wants to read the raw history, run:

```bash
stackunderflow search-past-decisions "<query>" --limit 10
```

Text output renders the matching excerpts with session ids, timestamps, and a snippet — readable as a search-result list.

## Alternate path: MCP

If the StackUnderflow MCP server is connected, the equivalent is the `search_past_decisions` tool with the same arguments. The CLI is the universally-available fallback.

## Cost

Local SQLite FTS query — no network, no LLM call. <500 ms even on 200K-event stores. Run liberally; an empty result is also a useful signal.

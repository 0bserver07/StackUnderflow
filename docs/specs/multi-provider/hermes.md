# Hermes Agent Specification

This document details the layout, event structure, and mapping strategy for the Hermes AI coding agent.

## Directory Layout

Hermes stores its on-disk configuration, memories, and session logs inside `~/.hermes/`. The layout is:

```
~/.hermes/
  config.yaml       # Global configuration
  SOUL.md           # Identity definition
  sessions/         # Active session JSONL files
    {sessionId}.jsonl
```

Session logs are stored directly under `~/.hermes/sessions/` as individual `.jsonl` files (or optionally nested in subdirectories).

---

## Event Structure

Hermes writes session data as JSONL events. There are three primary event types:

### 1. Session Initialization
```json
{"type": "session", "id": "session-uuid-1234", "timestamp": "2026-05-25T19:43:15Z", "cwd": "/Users/user/code/my-app"}
```

### 2. Model Changes
```json
{"type": "model_change", "data": {"model": "claude-3-5-sonnet"}, "timestamp": "2026-05-25T19:43:20Z"}
```

### 3. Assistant Messages (with usage)
```json
{
  "type": "message",
  "id": "msg-uuid-5678",
  "timestamp": "2026-05-25T19:45:00Z",
  "message": {
    "role": "assistant",
    "content": [{"type": "text", "text": "I can help with that."}],
    "model": "claude-3-5-sonnet",
    "provider": "anthropic",
    "usage": {
      "input": 120,
      "output": 60,
      "cacheRead": 20,
      "cacheWrite": 10
    }
  }
}
```

---

## Normalization Mapping

When normalizing assistant messages from Hermes:
* **`input`** -> `input_tokens`
* **`output`** -> `output_tokens`
* **`cacheRead`** -> `cache_read_tokens`
* **`cacheWrite`** -> `cache_create_tokens`
* **`model`** -> Preference is given to `message.model`; otherwise falls back to the most recent `model_change` event, or `"hermes-auto"`.

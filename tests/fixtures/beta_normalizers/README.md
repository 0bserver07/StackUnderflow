# Beta normalizer fixtures

Synthetic-but-spec-accurate fixtures used by
`tests/stackunderflow/etl/normalize/test_beta_normalizers.py` to drive
real-shape end-to-end tests for the 12 beta-flag-gated providers.

Each subdirectory matches a provider key from the normalizer registry
(`stackunderflow.etl.normalize`). The on-disk shape mirrors the layout
documented in `docs/specs/multi-provider/codeburn-catalog.md`.

| Provider | Fixture file(s) | Source format |
|---|---|---|
| `cursor_agent` | `transcript.jsonl` | Composer 2 JSONL |
| `opencode` | `session.json` | DB schema spec; materialised into SQLite at test time |
| `qwen` | `chat.jsonl` | Qwen CLI JSONL |
| `gemini` | `chat.jsonl` | Gemini CLI 0.39+ JSONL |
| `copilot` | `events.jsonl` | Legacy CLI events |
| `codeium` | `EMPTY` | Discovery-only stub (no parsable format yet) |
| `continue` | `session.json` | DB schema spec; materialised into SQLite at test time |
| `droid` | `session.jsonl`, `session.settings.json` | Factory CLI JSONL + sidecar |
| `kiro` | `chat.chat` | Kiro single-JSON `.chat` |
| `openclaw` | `session.jsonl` | OpenClaw JSONL |
| `pi` | `session.jsonl` | Pi/OMP JSONL |
| `kilocode` | `ui_messages.json`, `api_conversation_history.json` | Cline-family |
| `roocode` | `ui_messages.json`, `api_conversation_history.json` | Cline-family |

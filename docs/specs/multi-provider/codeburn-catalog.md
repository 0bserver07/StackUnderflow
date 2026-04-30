# CodeBurn Multi-Provider Catalog

This document provides a comprehensive reference of all 16 provider integrations supported by CodeBurn (a TypeScript codebase for harvesting AI provider usage data). It serves as a design reference for implementing parallel provider support in StackUnderflow's Python codebase.

---

## Provider Contract

Every provider must implement the `Provider` interface (from `/src/providers/types.ts`):

```typescript
export type Provider = {
  name: string
  displayName: string
  modelDisplayName(model: string): string
  toolDisplayName(rawTool: string): string
  discoverSessions(): Promise<SessionSource[]>
  createSessionParser(source: SessionSource, seenKeys: Set<string>): SessionParser
}
```

Each provider yields records of type `ParsedProviderCall`:

```typescript
export type ParsedProviderCall = {
  provider: string
  model: string
  inputTokens: number
  outputTokens: number
  cacheCreationInputTokens: number
  cacheReadInputTokens: number
  cachedInputTokens: number
  reasoningTokens: number
  webSearchRequests: number
  costUSD: number
  tools: string[]
  bashCommands: string[]
  timestamp: string
  speed: 'standard' | 'fast'
  deduplicationKey: string
  userMessage: string
  sessionId: string
}
```

---

## Shared Utilities

### `fs-utils.ts`
- **readSessionFile(filePath)**: Reads JSONL/text files up to 128 MB, using streaming for files >8 MB.
- **readSessionLines(filePath)**: Async generator for line-by-line streaming.

### `sqlite.ts`
- **openDatabase(path)**: Opens Node 22+ built-in SQLite (read-only).
- **isSqliteAvailable()**: Checks whether SQLite module is available.
- **SqliteDatabase**: Simple wrapper with `query<T>(sql, params?)` and `close()` methods.

### `cursor-cache.ts`
- Caches parsed Cursor results to `~/.cache/codeburn/cursor-results.json` to avoid re-parsing large vscdb files.

### `bash-utils.ts`
- **extractBashCommands(command)**: Strips quoted strings, splits on `&&`, `;`, `|`, extracts command basenames.

### `models.ts`
- **calculateCost(model, input, output, cacheWrite, cacheRead, webSearchRequests, speed)**: Computes USD cost from litellm pricing snapshot.
- **getModelCosts(model)**: Looks up model pricing; handles aliases (e.g., `claude-sonnet-4-6@20250929` → `claude-sonnet-4-6`).
- **setModelAliases(aliases)**: User-supplied model name remapping.

---

## Provider Details

### 1. Claude
- **Storage**: File-based discovery only (no parsing implemented).
- **Discovery**:
  - macOS/Linux: `~/.claude/projects/` + `~/Library/Application Support/Claude/local-agent-mode-sessions/` (macOS) / `~/.config/Claude/local-agent-mode-sessions/` (Linux)
  - Windows: `%APPDATA%\Claude\local-agent-mode-sessions\`
- **File Format**: Unknown; returns empty session parser.
- **Records**: N/A
- **Token / Cost Fields**: N/A
- **Project**: Folder names under `projects/`
- **Session ID**: N/A
- **Quirks**: Discovery only; parsing not yet implemented.

### 2. Codex
- **Storage**: JSONL streaming (`~/.codex/sessions/{YYYY}/{MM}/{DD}/rollout-*.jsonl`)
- **Discovery**:
  - macOS/Linux/Windows: `~/.codex/sessions/` (year/month/day directory hierarchy)
  - Env override: `CODEX_HOME`
- **File Format**: JSONL with entries:
  - `session_meta`: Stores `session_id`, `model`, `originator` ('codex')
  - `turn_context`: Model name
  - `response_item`: Tool calls, user/assistant messages
  - `event_msg` with `type: 'token_count'`: Token usage (`last_token_usage`, `total_token_usage`)
- **Records**: One call per token_count event (coalesced per turn).
- **Token Fields**: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens` (from `last_token_usage` or computed as delta from totals).
- **Project**: Derived from session `cwd` (sanitized: `/foo/bar` → `foo-bar`).
- **Session ID**: From `session_meta.session_id` or filename.
- **Quirks**: Normalizes OpenAI's cached-inclusive input counts to Anthropic semantics (separates cached from fresh input).

### 3. Copilot
- **Storage**: JSONL (two formats):
  - Legacy: `~/.copilot/session-state/{sessionId}/events.jsonl`
  - VS Code: `workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl`
- **Discovery**:
  - macOS: `~/.copilot/session-state/` + `~/Library/Application Support/Code/User/workspaceStorage/`
  - Windows: `%APPDATA%\Code\User\workspaceStorage\`
  - Linux: `~/.config/Code/User/workspaceStorage/`
- **File Format**:
  - Legacy: `{ type: 'session.model_change' | 'user.message' | 'assistant.message', ... }`
  - Transcript: `{ type: 'session.start', data: { producer: 'copilot-agent' } }` (newer format).
- **Records**: One per `assistant.message` event with `outputTokens > 0`.
- **Token Fields**: `outputTokens` (explicit or estimated from text length / 4). Input tokens estimated from user message length.
- **Model Inference**: Inferred from tool-call IDs (`toolu_bdrk_` → Anthropic, `call_` → OpenAI) when not explicit.
- **Project**: From `workspace.yaml` cwd or workspace UUID.
- **Session ID**: UUID or directory name; stored in legacy `session-state/{sessionId}/`.
- **Quirks**: Supports two file formats (legacy outputTokens vs. estimated); token counts may be estimated or missing.

### 4. Cursor
- **Storage**: SQLite vscdb (`~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` on macOS).
- **Discovery**:
  - macOS: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
  - Windows: `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
  - Linux: `~/.config/Cursor/User/globalStorage/state.vscdb`
- **DB Schema**:
  - Table: `cursorDiskKV`
  - Keys starting with `bubbleId:%` hold chat entries (JSON in `value`).
  - Keys `agentKv:blob:%` hold agent conversation data.
  - JSON fields extracted via `json_extract()`:
    - bubbles: `$.tokenCount.{inputTokens, outputTokens}`, `$.modelInfo.modelName`, `$.createdAt`, `$.conversationId`, `$.type`, `$.codeBlocks`
    - agent KV: `$.role` (user/assistant/tool/system), `$.content` (JSON or text), `$.providerOptions.cursor.{modelName, requestId}`
- **Records**:
  - Bubble type 1 = user input, type 2 = assistant response.
  - Each conversation can have multiple bubbles; aggregated by `conversationId`.
- **Token Fields**: Explicit `tokenCount.{inputTokens, outputTokens}` or estimated from text length (Cursor v3).
- **Project**: Fixed as `'cursor'`.
- **Session ID**: `conversationId` or `requestId` (for agent KV).
- **Caching**: Results cached to `~/.cache/codeburn/cursor-results.json` with DB mtime/size fingerprint.
- **Quirks**: Two data structures (bubbles + agentKv); v3 uses zero token counts (requires estimation); requires Node 22+ for SQLite.

### 5. Cursor Agent
- **Storage**: Text or JSONL transcripts + SQLite attribution DB.
- **Discovery**:
  - macOS: `~/.cursor/projects/`
  - Windows: unverified
  - Linux: `~/.cursor/projects/`
  - Transcripts in: `{project}/agent-transcripts/` (legacy `.txt`) or `{project}/agent-transcripts/{uuid}/*.jsonl` (Composer 2).
  - DB: `~/.cursor/ai-tracking/ai-code-tracking.db`
- **File Format**:
  - Text: Lines with markers `user:`, `A:`, `[Thinking]`, `[Tool call]`, `[Tool result]`.
  - JSONL: `{ role: 'user' | 'assistant', message: { content: [{ type: 'text' | 'tool_use', text?: string, name?: string }] } }`
- **Records**: One per assistant turn (grouped by input+assistant output).
- **Token Fields**: Estimated from character length / 4.
- **DB Lookup**: Queries `conversation_summaries` table for model name and updatedAt timestamp.
- **Project**: Prettified from directory name (timestamps, path prefixes stripped).
- **Session ID**: UUID from filename or SHA1 hash of path.
- **Quirks**: Supports two transcript formats (text and JSONL); tokens estimated from text; optional DB lookup for model metadata.

### 6. Droid
- **Storage**: JSONL + companion `.settings.json` file.
- **Discovery**:
  - Base: `$FACTORY_DIR` (env) or `~/.factory/`
  - Sessions in: `~/.factory/sessions/{projectHash}/`
- **File Format**:
  - JSONL: `{ type: 'session_start' | 'message', id, timestamp, cwd, message: { role: 'user' | 'assistant', content: [{ type: 'text' | 'tool_use', text, name, input }] } }`
  - Settings: `{ model, tokenUsage: { inputTokens, outputTokens, cacheCreationTokens, cacheReadTokens, thinkingTokens } }`
- **Records**: Session-level token counts (no per-call data); distributed evenly across assistant calls.
- **Token Fields**: From `.settings.json` `tokenUsage`.
- **Model**: From settings or unknown.
- **Project**: Derived from session `cwd`.
- **Session ID**: From `session_start.id` or basename.
- **Quirks**: Session-level token aggregation only; must sum and distribute across detected API calls; model stored separately.

### 7. Gemini
- **Storage**: JSON or JSONL (Gemini CLI format).
- **Discovery**:
  - `~/.gemini/tmp/{project}/chats/session-*.{json,jsonl}`
- **File Format**:
  - Single JSON (CLI ≤0.38): `{ sessionId, startTime, projectHash?, lastUpdated?, kind?, messages: [...] }`
  - JSONL (CLI ≥0.39): Metadata line + message lines.
  - Messages: `{ id, timestamp, type: 'user' | 'gemini' | 'info', content: string | [{ text }], tokens: { input, output, cached, thoughts, tool, total }, model, toolCalls: [{ id, name, args, displayName }], thoughts }`
- **Records**: One per session (aggregated).
- **Token Fields**: `tokens.{input, output, cached, thoughts}` per message; aggregated for session.
- **Project**: Directory name.
- **Session ID**: From message data or directory.
- **Quirks**: Tokens include cached as subset of input; fresh input = input - cached; thoughts counted as reasoning output.

### 8. KiloCode (Cline-based)
- **Storage**: VSCode Cline extension data.
- **Discovery**: Wraps `vscode-cline-parser.discoverClineTasks()` with extension ID `kilocode.kilo-code`.
- **Parsing**: Wraps `createClineParser()`.
- **Quirks**: Generic Cline parser; see roo-code and vscode-cline-parser below.

### 9. Kiro
- **Storage**: JSONL (`.chat` files).
- **Discovery**:
  - macOS: `~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/`
  - Windows: `%APPDATA%\Kiro\User\globalStorage\kiro.kiroagent\`
  - Linux: `~/.config/Kiro/User/globalStorage/kiro.kiroagent/`
  - Workspace Storage: `{globalStorage}/kiro.kiroagent/` + `~/Library/Application Support/Kiro/User/workspaceStorage/` (macOS)
- **File Format**:
  - JSON: `{ executionId, actionId, chat: [{ role: 'human' | 'bot' | 'tool', content: string }], metadata: { modelId, startTime, endTime, workflowId } }`
- **Records**: One per execution (aggregated chat).
- **Token Fields**: Estimated from content length / 4.
- **Model**: `metadata.modelId` (normalized, e.g., `claude.3.5.sonnet` → `claude-3-5-sonnet`); 'kiro-auto' if missing.
- **Tools**: Extracted from bot messages (regex: `<tool_use><name>(...)</name>`).
- **Project**: Derived from workspace directory or hash lookup in `.../workspace-sessions/`.
- **Session ID**: `metadata.workflowId` or filename.
- **Quirks**: Normalizes model names (dots to dashes); discovers workspace from hash + optional directory encoding.

### 10. OpenClaw
- **Storage**: JSONL.
- **Discovery**:
  - Checks multiple possible base dirs: `~/.openclaw/agents/`, `~/.clawdbot/agents/`, `~/.moltbot/agents/`, `~/.moldbot/agents/`.
  - Sessions in: `{agentsDir}/{agent}/sessions/`
  - Index: optional `sessions.json` or directory scan.
- **File Format**:
  - JSONL: `{ type: 'session' | 'model_change' | 'custom' | 'message', id, timestamp, customType?, data?, message: { role: 'user' | 'assistant', content: [{ type: 'text' | 'tool_use' | 'toolCall', text?, name?, arguments? }], model, provider, usage: { input, output, cacheRead, cacheWrite, cost } } }`
- **Records**: One per assistant message with usage data.
- **Token Fields**: Explicit `message.usage.{input, output, cacheRead, cacheWrite}` or `message.usage.cost.total`.
- **Model**: From `message.model` or latest `model_change` event.
- **Project**: Agent name (directory).
- **Session ID**: Session ID from index or basename.
- **Quirks**: Optional provider-embedded cost; fallback to calculateCost if not present.

### 11. OpenCode
- **Storage**: SQLite database.
- **Discovery**:
  - Data dir: `$XDG_DATA_HOME/opencode/` or `~/.local/share/opencode/`
  - Scans for `opencode*.db` files.
  - Sessions queried from DB.
- **DB Schema**:
  - Tables: `session`, `message`, `part`
  - Session: `id, directory, title, time_created, time_archived, parent_id`
  - Message: `id, session_id, time_created, data` (JSON: `{ role, modelID, tokens: { input, output, reasoning, cache: { read, write } }, cost }`)
  - Part: `message_id, session_id, data` (JSON: `{ type: 'text' | 'tool', text?, tool?, state: { input: { command } } }`)
- **Records**: One per assistant message.
- **Token Fields**: `message.data.tokens.{input, output, reasoning, cache.{read, write}}`.
- **Model**: From `message.data.modelID`.
- **Project**: From `session.directory` or `session.title`.
- **Session ID**: From path encoding `{dbPath}:{sessionId}` (SQLite + UUID).
- **Quirks**: Path-based session ID encoding to support multiple DB files; tool extraction from separate `part` table.

### 12. Pi (and OMP)
- **Storage**: JSONL.
- **Discovery**:
  - Pi: `~/.pi/agent/sessions/`
  - OMP: `~/.omp/agent/sessions/` (or `QWEN_DATA_DIR` env for both)
- **File Format**:
  - JSONL: `{ type: 'session' | 'message', id, timestamp, cwd, message: { role: 'user' | 'assistant', content: [{ type: 'text' | 'toolCall', text, name, arguments }], model, responseId, usage: { input, output, cacheRead, cacheWrite } } }`
- **Records**: One per assistant message with usage.
- **Token Fields**: `message.usage.{input, output, cacheRead, cacheWrite}`.
- **Model**: From `message.model` (default 'gpt-5').
- **Project**: Directory name or `cwd`.
- **Session ID**: From `session.id` or filename.
- **Quirks**: Pi and OMP share parser logic but use different base directories.

### 13. Qwen
- **Storage**: JSONL.
- **Discovery**:
  - `$QWEN_DATA_DIR/projects/` or `~/.qwen/projects/`
  - Sessions in: `{project}/chats/*.jsonl`
- **File Format**:
  - JSONL: `{ uuid, sessionId, timestamp, type: 'user' | 'assistant', model?, message: { role, parts: [{ text, thought?, functionCall: { name, args } }] }, usageMetadata: { promptTokenCount, candidatesTokenCount, thoughtsTokenCount, cachedContentTokenCount } }`
- **Records**: One per assistant entry with usageMetadata.
- **Token Fields**: `usageMetadata.{promptTokenCount, candidatesTokenCount, thoughtsTokenCount, cachedContentTokenCount}`.
- **Model**: From `entry.model` or 'qwen-auto'.
- **Tools**: From `functionCall.name` (mapped via toolNameMap).
- **Project**: Derived from directory name.
- **Session ID**: From `entry.sessionId`.
- **Quirks**: Thoughts (reasoning) included in token count; cached tokens are read-only.

### 14. Roo Code (Cline-based)
- **Storage**: VSCode Cline extension data.
- **Discovery**: Wraps `vscode-cline-parser.discoverClineTasks()` with extension ID `rooveterinaryinc.roo-cline`.
- **Parsing**: Wraps `createClineParser()`.
- **Quirks**: Generic Cline parser.

### 15. VSCode Cline Parser (shared)
- **Storage**: VSCode globalStorage for Cline-compatible extensions.
- **Discovery**:
  - macOS: `~/Library/Application Support/Code/User/globalStorage/{extensionId}/`
  - Windows: `%APPDATA%\Code\User\globalStorage\{extensionId}\`
  - Linux: `~/.config/Code/User/globalStorage/{extensionId}/`
  - Tasks in: `{globalStorage}/tasks/{taskId}/`
  - Files: `ui_messages.json`, `api_conversation_history.json`
- **File Format**:
  - ui_messages: `[{ type: 'say', say: 'user_feedback' | 'text' | 'api_req_started', text?: string, ts?: number }]`
  - api_conversation_history: `[{ role: 'user' | 'assistant', content: [{ text }] }]`
  - Model extracted from user message: `<model>...</model>` tag.
- **Records**: One per `api_req_started` event.
- **Token Fields**: Parsed from `text` JSON: `{ tokensIn, tokensOut, cacheWrites, cacheReads, cost }` or fallback to calculateCost.
- **Model**: Extracted from `<model>` tag in user message.
- **Project**: Fixed as displayName (passed in).
- **Session ID**: Task ID (directory name).
- **Quirks**: Generic parser shared by KiloCode and Roo Code; model extraction from user message markup.

---

## Cross-Cutting Patterns

### Storage Styles
| Style | Providers | Notes |
|-------|-----------|-------|
| **JSONL streaming** | Codex, Copilot, Droid, Kiro, OpenClaw, Pi, OMP, Qwen | Line-by-line parsing; supports large files. |
| **SQLite** | Cursor, OpenCode | Requires Node 22+ (`node:sqlite`); read-only access. |
| **Text/Transcripts** | Cursor Agent | Hand-written transcripts; marker-based parsing. |
| **VSCode globalStorage** | KiloCode, Roo Code (Cline parser) | JSON metadata files in extension directories. |

### Token Counting Strategies
| Approach | Providers | Details |
|----------|-----------|---------|
| **Explicit** | Codex, Copilot (some), Cursor (v2), Gemini, OpenCode, OpenClaw, Pi, OMP, Qwen | Fields directly present in storage. |
| **Estimated** | Copilot (legacy), Cursor (v3), Cursor Agent, Droid (distributed), Gemini (with special handling), Kiro | Length / 4 heuristic; or aggregated from settings. |
| **Hybrid** | OpenCode, OpenClaw | Prefers explicit; falls back to calculateCost. |

### Discovery Entry Points
All providers export a singleton instance:
```typescript
export const <provider> = create<Provider>Provider()
```

Main entry point: `/src/providers/index.ts`
```typescript
export async function getAllProviders(): Promise<Provider[]>
export async function discoverAllSessions(providerFilter?: string): Promise<SessionSource[]>
export async function getProvider(name: string): Promise<Provider | undefined>
```

Optional providers (Cursor, OpenCode, Cursor Agent) are lazily loaded and may return `null` if their dependencies are unavailable.

### Deduplication Keys
Each provider constructs a `deduplicationKey` to avoid double-counting:
- **Format**: `{provider}:{sessionId}:{uniqueId}` or hash variant
- **Examples**:
  - Codex: `codex:{path}:{timestamp}:{cumulativeTokens}`
  - Cursor: `cursor:{conversationId}:{createdAt}:{inputTokens}:{outputTokens}`
  - Gemini: `gemini:{sessionId}` (per-session aggregate)
  - Cline-based: `{providerName}:{taskId}:{index}`

---

## Known Gaps and Limitations

1. **Claude**: Discovery implemented; parsing not yet implemented.
2. **Cursor Agent**: Windows paths unverified (issue #55 in codeburn).
3. **SQLite dependency**: Cursor and OpenCode require Node 22+ or explicit `node:sqlite` availability.
4. **Cursor v3 tokens**: Zero token counts require character length estimation; accuracy depends on model consistency.
5. **Token normalization**: OpenAI (Codex, Copilot) includes cached tokens in input; Anthropic separates them. CodeBurn normalizes to Anthropic semantics.
6. **Cost estimation**: Some providers (Gemini, Kiro, Qwen, Cursor Agent) estimate costs from token counts; no provider-specific pricing is used except where explicitly embedded (OpenClaw).

---

## Integration Summary

**Total providers**: 16  
- **Core** (always available): 12 (Claude, Codex, Copilot, Droid, Gemini, KiloCode, Kiro, OpenClaw, Pi, OMP, Qwen, Roo Code)
- **Optional** (lazy-loaded): 3 (Cursor, OpenCode, Cursor Agent)
- **Shared**: VSCode Cline Parser (used by KiloCode and Roo Code)

**Most complex**: Cursor (SQLite + caching), Cursor Agent (multi-format transcripts + DB lookups)  
**Most complete**: Codex, OpenClaw (explicit token/cost data)  
**Most lenient**: Claude (discovery-only stub)


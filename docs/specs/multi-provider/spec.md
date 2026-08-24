# Multi-Provider ETL Spec

**Date:** 2026-04-30
**Status:** Design — ready for Wave 2 implementation
**Inputs:** `codeburn-catalog.md`, `adapter-audit.md`, `cost-calc-design.md`, `local-inventory.md`

## TL;DR

Adopt — not port — codeburn's multi-provider patterns. Extend StackUnderflow with **two abstractions**:

1. **`SessionRef.source_kind` + `source_hint`** — lets one adapter contract handle JSONL files, SQLite tables, and vscdb keys uniformly. (Adapter audit — Proposal 3.)
2. **Pluggable `ProviderPricer` modules** under `infra/providers/` — each provider owns its model heuristics, rates table, and token-normalization logic. (Cost-calc design — Option B.)

Implementation order driven by **local data on this machine**, not codeburn's roster:

- **Wave 2A** (proves SQLite + vscdb pattern): **Cursor** — 1.4 GB of vscdb data, real value, hardest case. If this works, every other SQLite-style adapter follows.
- **Wave 2B** (proves VS Code globalStorage + JSON-task pattern): **Cline** — 29 real tasks, 13 MB, validates the "many small JSON files per session" pattern shared by Cline / KiloCode / Roo Code.
- **Wave 2C** (deferred): opencode, Gemini, Qwen — no/low local data on this machine, low ROI today.

Codeburn attribution: comment-level (`# pattern adapted from codeburn:src/providers/cursor.ts`) on path constants and schema queries we lift verbatim. No code copy.

---

## 1. Adapter Contract Changes

### 1.1 SessionRef — add two optional fields

Per `adapter-audit.md` §3 Proposal 3:

```python
@dataclass(frozen=True, slots=True)
class SessionRef:
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
    source_kind: Literal["file", "database"] = "file"   # NEW
    source_hint: dict[str, Any] | None = None           # NEW
```

`source_hint` carries adapter-private metadata — table name, vscdb key prefix, last rowid. JSONL adapters leave both at defaults; existing `claude.py` / `codex.py` need **zero changes**.

### 1.2 `ingest_log` schema migration

Current:
```sql
CREATE TABLE ingest_log (
    file_path TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    mtime REAL NOT NULL,
    size INTEGER NOT NULL,
    processed_offset INTEGER NOT NULL,
    last_ingest_ts REAL
);
```

After:
```sql
CREATE TABLE ingest_log (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    provider TEXT NOT NULL,
    session_id TEXT,                  -- NULL for file-based
    storage_kind TEXT NOT NULL DEFAULT 'file'
        CHECK (storage_kind IN ('file', 'database')),
    mtime REAL NOT NULL,
    size INTEGER NOT NULL,
    processed_offset INTEGER,         -- bytes; NULL for database
    last_rowid INTEGER,               -- rowid; NULL for file
    last_ingest_ts REAL,
    UNIQUE(file_path, session_id)
);
```

Migration: alembic-style one-off in `store/schema.py` — copy old rows with `session_id=NULL, storage_kind='file', last_rowid=NULL`.

### 1.3 `run_ingest()` change

Branches on `ref.source_kind` to compute `since`:

```python
if ref.source_kind == "database":
    prior = conn.execute(
        "SELECT last_rowid FROM ingest_log WHERE file_path = ? AND session_id = ?",
        (str(ref.file_path), ref.session_id),
    ).fetchone()
    since = prior["last_rowid"] if prior else 0
else:
    prior = conn.execute(
        "SELECT processed_offset FROM ingest_log WHERE file_path = ? AND session_id IS NULL",
        (str(ref.file_path),),
    ).fetchone()
    since = prior["processed_offset"] if prior else 0

ingest_file(conn, adapter, ref, since_offset=since)
```

### 1.4 Test contract

`tests/python-legacy: adapters/contract.py` gets one new test:

```python
def test_read_since_offset_is_storage_aware(self):
    refs = list(self.adapter.enumerate())
    if not refs: return
    full = list(self.adapter.read(refs[0]))
    if len(full) < 2: return
    midpoint = full[len(full) // 2].seq
    resumed = list(self.adapter.read(refs[0], since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)
```

This works for both byte-offset (JSONL) and rowid (SQLite) resumption — `seq` is the right abstraction either way.

### 1.5 Codex normalization moves

The Codex `subtract_cached_from_input` logic in `adapters/codex.py:299-337` migrates to `infra/providers/openai.py:OpenAIPricer.normalize_tokens()`. Adapter just emits raw OpenAI-shape tokens. See §2.

---

## 2. Cost Layer Changes

Per `cost-calc-design.md` §3 Option B: pluggable per-provider modules.

### 2.1 New layout

```
stackunderflow/infra/providers/
    __init__.py          # registry
    base.py              # ProviderPricer ABC
    anthropic.py         # AnthropicPricer (extracted from costs.py)
    openai.py            # OpenAIPricer (Codex normalization lives here)
    cursor.py            # CursorPricer (no per-msg tokens; session_api fallback)
    cline.py             # ClinePricer (delegates to anthropic by parsed model tag)
```

### 2.2 ABC

```python
class ProviderPricer(ABC):
    provider_name: str
    @abstractmethod
    def canonicalize(self, model_id: str) -> str: ...
    @abstractmethod
    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]: ...
    @abstractmethod
    def rates_for(self, canonical: str) -> tuple[float, float, float, float] | None: ...
    @abstractmethod
    def supports_per_message_tokens(self) -> bool: ...
```

### 2.3 Service entry point

`compute_cost(tokens, model, provider="anthropic")` keeps its current signature for backward-compat; internally dispatches to `_REGISTRY[provider].normalize_tokens(...)` then `_compute(rates, normalized)`.

### 2.4 `Record` gets `provider`

Already has `provider`. Aggregator collectors already receive it. The change is ~10 call sites where `compute_cost(tokens, model)` becomes `compute_cost(tokens, model, provider=record.provider)`.

### 2.5 Cursor's no-tokens problem

`CursorPricer.supports_per_message_tokens() -> False`. Aggregator skips per-message cost for Cursor records and relies on session-level totals from the adapter (vscdb stores estimated token counts at the bubble level — codeburn estimates from text length÷4 when v3 returns zeros). Output flagged `cost_source: "estimated"`.

---

## 3. Wave 2 Implementation Plan

Driven by local-inventory tier ranking. Three providers worth implementing today; everything else is deferred until the user has data for it.

### 3.1 Wave 2A — Cursor adapter (critical path)

**Why first:** Largest local dataset (1.4 GB). vscdb is the *hardest* case (SQLite + JSON-in-values + bubble/agentKv split) — if the new abstractions handle Cursor cleanly, every other SQLite-style provider is mechanical.

**Reads:**
- `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (macOS)
- macOS-only for v1; Windows/Linux paths constants present but untested.

**Schema queries (codeburn-catalog.md §4):**
- `SELECT key, value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%'`
- `SELECT key, value FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'`
- `json_extract(value, '$.tokenCount.inputTokens')` etc.

**Session model:** one `SessionRef` per `conversationId`. `source_kind="database"`, `source_hint={"vscdb_key_prefix": "bubbleId:", "conversation_id": "..."}`.

**Token strategy:** explicit when `tokenCount.{input,output}` non-zero; estimate `len(text)/4` fallback (Cursor v3 zeros). Mark estimated records `cost_source: "estimated"`.

**Tests:** synthetic vscdb fixture with 2 bubbles + 1 agent kv; resume test using rowid; round-trip test asserting `seq` monotonic.

**Open PR:** `feat: cursor adapter (vscdb)`.

### 3.2 Wave 2B — Cline adapter (validates VS Code globalStorage pattern)

**Why second:** 29 *real* completed tasks on this machine, 13 MB of API conversation history. This pattern (`tasks/{taskId}/{ui_messages.json,api_conversation_history.json}`) is shared by Cline, KiloCode, and Roo Code — once one works, the others fall out as 20-line config diffs.

**Reads:**
- `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/`
- Per-task: `ui_messages.json`, `api_conversation_history.json`

**Session model:** one `SessionRef` per task directory. `source_kind="file"` (it's just JSON files), `source_hint=None`. The task ID *is* the session ID.

**Token strategy:** parse `tokensIn/tokensOut/cacheWrites/cacheReads` from `api_req_started` `say` events in `ui_messages.json` (codeburn-catalog.md §15). Model name extracted from `<model>` tag in user message — pricing routes through `ClinePricer` which delegates to `AnthropicPricer`/`OpenAIPricer` based on the parsed tag.

**Tests:** fixture with 2 tasks, one with explicit tokens, one with `<model>` tag delegation.

**Open PR:** `feat: cline adapter (vscode globalStorage)`.

### 3.3 Wave 2C — deferred

| Provider | Why deferred |
|---|---|
| **opencode** | No local DB on this machine. Pattern is nearly identical to Cursor's SQLite (`session`/`message`/`part` tables). 50-line adapter when user installs it. |
| **Gemini** | 12 MB but config-only — no conversation history visible. Re-evaluate when user actually uses Gemini CLI for sessions. |
| **Qwen** | 864 KB / 3 projects. Pattern is plain JSONL — trivial to add when there's data worth showing. |
| **Copilot** | <1 KB on disk (debug files only). Skip. |
| **Codeium** | Protobuf. Decoding cost > value. Skip. |
| **Continue** | SQLite indices but `sessions` table is empty on this machine. Skip. |
| **KiloCode / Roo Code** | Not installed. Add when user installs them; reuse Cline parser. |
| **Droid / Kiro / OpenClaw / Pi / OMP / Cursor Agent** | Not installed. Defer. |

---

## 4. Codeburn Attribution Policy

We are **not porting**. We adopt patterns. Attribution rules:

- **Path constants we copy verbatim** (e.g., `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`) get a one-line comment: `# path from codeburn:src/providers/cursor.ts`.
- **Schema queries lifted verbatim** (e.g., the bubbleId LIKE query) get the same.
- **No code blocks copied.** Python implementations are written from scratch against codeburn's documented schema.
- **No license file required** — codeburn is MIT, attribution comments satisfy.
- **Don't reference codeburn in user-facing strings** (provider names, dashboard labels, README) — internal-only.

---

## 5. Out of Scope (v1)

- Provider-specific UI tabs (each provider gets the existing dashboard, no special-case views).
- Provider-specific cost-saving heuristics.
- Cross-provider session deduplication beyond the existing `UNIQUE(provider, slug)` constraint.
- Compressed source files (.jsonl.gz, .db.gz).
- Windows / Linux discovery paths for Wave 2A and 2B (macOS-only first; constants in code, untested).
- Real-time tailing of vscdb (Cursor writes are sparse; periodic re-ingest is fine).

---

## 6. Implementation Sequence

```
Step 1  PR: SessionRef + ingest_log migration + AdapterContract test
        Files: adapters/base.py, ingest/__init__.py, store/schema.py,
               tests/python-legacy: adapters/contract.py
        No new provider yet. Existing tests must still pass.

Step 2  PR: infra/providers/ scaffold + AnthropicPricer + OpenAIPricer
        (Codex normalization moves out of adapters/codex.py)
        Files: infra/providers/{base,anthropic,openai,__init__}.py,
               infra/costs.py (now a thin wrapper),
               adapters/codex.py (drops normalization),
               stats/aggregator.py (passes provider= arg)
        420 existing tests still pass; new pricer tests added.

Step 3  PR: Cursor adapter
        Files: adapters/cursor.py, infra/providers/cursor.py,
               tests/python-legacy: adapters/test_cursor.py
        Beta-flagged in /settings, off by default.

Step 4  PR: Cline adapter
        Files: adapters/cline.py, infra/providers/cline.py,
               tests/python-legacy: adapters/test_cline.py
        Beta-flagged.

Step 5  PR: dashboard polish — surface provider in project list,
        show provider chip in session table, mark estimated-cost rows.
        Files: stackunderflow-ui/components/...
```

Steps 1–2 are **prerequisites** — Wave 2A/B can't start until they land. Steps 3 and 4 can run in parallel once 1–2 are merged.

---

## 7. Decision Log

| Question | Decision | Why |
|---|---|---|
| Adapter extension shape | Proposal 3 (extended SessionRef) | Backward-compatible; existing adapters need zero changes |
| Cost layer shape | Option B (pluggable modules) | Codex normalization belongs near pricing, not in adapters |
| First provider | Cursor | Largest local dataset; hardest case validates the abstraction |
| Codeburn relationship | Reference, not port | Attribution by comment on lifted constants |
| Per-message tokens for Cursor | Estimate w/ flag | Real billing API integration is out of scope |
| Windows / Linux paths | macOS only v1 | No machines to test on; ship what we can validate |

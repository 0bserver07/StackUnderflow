# AI Coding Agent Local Data Inventory

**User:** 0bserver07
**Date:** 2026-04-30
**Machine:** Darwin 25.3.0 (Apple Silicon)

---

## Executive Summary

This machine hosts extensive local data from multiple AI coding agents, with **Tier 1** priority given to Claude CLI (5.0GB), Codex (114MB), and Cursor (1.4GB). These three providers alone represent >6.5GB of real local data with very recent activity. Cline extension in VS Code also has substantial conversation history (13MB). Most other agents have minimal or zero local data.

---

## Detailed Inventory

### 1. Claude CLI (SUPPORTED — inventory only)

**Found?** Yes
**Location:** `~/.claude/`
**Format Detected:** Mixed (JSONL + SQLite + JSON)
**Item Count:**
- Project JSONL files: **3,496** files
- Session JSON files: **31** files
- Projects (directories): **1,209** directories
- File-history snapshots: **19,839** files

**Total Disk Size:** **5.0 GB**

**Most Recent Activity:** Apr 30, 2026 17:46:58 (file-history)
**Date Range:** Mar 31, 2026 to Apr 30, 2026

**Subdirectories with data:**
- `projects/` — 3,496 JSONL session files across 1,209 project dirs
- `sessions/` — 31 session metadata JSON files
- `file-history/` — 19,839 snapshot files tracking file edits
- `backups/`, `cache/`, `downloads/`, `plans/`, `shell-snapshots/`, `paste-cache/` — supporting data

**Sample JSONL (first 500 chars):**
```
{"type":"file-history-snapshot","messageId":"73b6ce51-8b66-4dfd-b09e-60fa934b1ea8",...}
{"parentUuid":null,"isSidechain":false,"userType":"external","cwd":"/Users/yadkonrad/.claude",...}
```

**Value:** VERY HIGH — primary Claude CLI usage log with months of project-level conversation history.

---

### 2. Codex CLI (SUPPORTED — inventory only)

**Found?** Yes
**Location:** `~/.codex/` + `~/Library/Application Support/Codex/`
**Format Detected:** JSONL + SQLite
**Item Count:**
- `history.jsonl`: **103 lines**
- Session JSONL files: **28** in `sessions/YYYY/MM/DD/`
- `logs_2.sqlite`: **16,167 rows**

**Total Disk Size:** 114 MB + 8.7 MB

**Most Recent Activity:** Apr 30, 2026 17:46 (logs_2.sqlite)
**Date Range:** Feb 6, 2026 to Apr 30, 2026

**Sample JSONL (first 500 chars):**
```
{"session_id":"5f9c9d0d-e655-4057-ba6c-90b570873fa7","ts":1754718491,"text":"should we be updated the index..."}
```

**Value:** HIGH — activity logging with conversation history and system logs. Very recent.

---

### 3. Cursor IDE

**Found?** Yes
**Location:** `~/.cursor/` (minimal) + `~/Library/Application Support/Cursor/User/`
**Format Detected:** SQLite vscdb
**Item Count:**
- `state.vscdb`: **790 rows** in ItemTable
- Workspace-specific `state.vscdb`: **20+** files
- Backup snapshots: **13** directories

**Total Disk Size:** 1.4 GB

**Most Recent Activity:** Feb 16, 2026 13:34:26
**Date Range:** Oct 22, 2024 to Feb 16, 2026

**Value:** MEDIUM-HIGH — Cursor stores IDE state and extension settings in vscdb with recent backups.

---

### 4. Cline Extension (VS Code)

**Found?** Yes
**Location:** `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/`
**Format Detected:** JSON + Git repos
**Item Count:**
- Task directories: **29** tasks with API/UI conversation history
- Checkpoint git repos: **2**

**Total Disk Size:** 13 MB

**Most Recent Activity:** Jan 15, 2026
**Date Range:** Aug 1, 2025 to Jan 19, 2026

**Sample Task History (first 500 chars):**
```
[{"id":"1737682863955","ts":1737682881708,"task":"@/app/services/ help me find services not used in my rails product",...}]
```

**Value:** HIGH — 29 complete tasks with full conversation history.

---

### 5. Qwen / Codext

**Found?** Yes
**Location:** `~/.qwen/`
**Format Detected:** JSONL + JSON
**Item Count:**
- Project JSONL: **3** files
- TODO items: **3** JSON files

**Total Disk Size:** 864 KB

**Most Recent Activity:** Apr 18, 2026 17:50:31
**Date Range:** Nov 5, 2025 to Apr 18, 2026

**Value:** MEDIUM — small but recent project tracking.

---

### 6. Cline in Cursor (`.cline/`)

**Found?** Yes
**Location:** `~/.cline/data/`
**Format Detected:** JSON
**Item Count:**
- Workspace state files: **23**
- Global state: 1 file

**Total Disk Size:** 100 KB

**Most Recent Activity:** Apr 30, 2026 17:09
**Date Range:** Mar 30, 2026 to Apr 30, 2026

**Value:** LOW-MEDIUM — minimal JSON structure, sparse data.

---

### 7. Gemini CLI / Google Indexing Agent

**Found?** Yes
**Location:** `~/.gemini/`
**Format Detected:** JSON (config, auth, state) + temp logs
**Item Count:**
- Config/state files: **8**
- Temp session logs: **5+**

**Total Disk Size:** 12 MB

**Most Recent Activity:** Apr 18, 2026
**Date Range:** Varies

**Value:** MEDIUM — auth/config stored locally. No user conversation history visible.

---

### 8. Codeium IDE Plugin

**Found?** Yes
**Location:** `~/.codeium/`
**Format Detected:** Protobuf + JSON
**Item Count:**
- Chat state files: **3**
- Implicit completion caches: **20+**

**Total Disk Size:** 449 MB

**Most Recent Activity:** Jan 27, 2025
**Date Range:** Dec 12, 2024 to Jan 27, 2025

**Value:** MEDIUM — large footprint but binary format (protobuf). Most recent activity is Jan 2025.

---

### 9. SuperMaven (Code Completion)

**Found?** Yes
**Location:** `~/.supermaven/`
**Format Detected:** JSON (config only)
**Item Count:** 1 config file

**Total Disk Size:** 4.0 KB

**Value:** LOW — config only, no usage data.

---

### 10. Aider CLI

**Found?** Yes
**Location:** `~/.aider/`
**Format Detected:** JSON
**Item Count:** 2 config files, 1 cache

**Total Disk Size:** 292 KB

**Most Recent Activity:** Jan 18, 2025
**Date Range:** Jan 11-18, 2025

**Value:** LOW — no conversation history, only config and analytics.

---

### 11. Kaysmith (Interactive Agent)

**Found?** Yes
**Location:** `~/.kaysmith/`
**Format Detected:** JSONL + JSON
**Item Count:**
- Session JSONL: **19 lines**

**Total Disk Size:** 40 KB

**Most Recent Activity:** Nov 13, 2025
**Date Range:** Nov 13, 2025

**Value:** LOW — minimal data, appears experimental.

---

### 12. VS Code Copilot Extension

**Found?** Minimal
**Location:** `~/Library/Application Support/Code/User/globalStorage/github.copilot-chat/`
**Format Detected:** None (debug files only)
**Item Count:** 2 debug files

**Total Disk Size:** <1 KB

**Value:** LOW — no local conversation history.

---

### 13. Continue IDE Extension

**Found?** Yes
**Location:** `~/.continue/`
**Format Detected:** SQLite + JSON
**Item Count:**
- SQLite databases: 3
- Sessions file: **empty**

**Total Disk Size:** 497 MB

**Value:** MEDIUM — large footprint from indexing, but sessions are empty.

---

### 15. Antigravity (Google IDE + CLI)

**Found?** Yes
**Locations:**
- Install: `/Users/<u>/.antigravity/` (symlinked to `/Applications/Antigravity.app/` when installed)
- Data root: `~/.gemini/antigravity/`, `~/.gemini/antigravity-ide/`, `~/.gemini/antigravity-cli/`
- Backup mirror: `~/.gemini/antigravity-backup/` (byte-identical to `antigravity/`)
- App support (VS Code-style state): `~/Library/Application Support/Antigravity/User/globalStorage/state.vscdb`

**Format Detected:** Mixed — plaintext metadata + encrypted protobuf payload

**Item Count (sample machine, May 2026):**
- IDE conversations: 5 `.pb` files (encrypted) + 1 `agyhub_summaries_proto.pb` (plaintext)
- CLI conversations: 4 `.pb` files (encrypted) + `history.jsonl` (81 user prompts across 3 conversations)
- Implicit context: 5 `.pb` files (encrypted)
- Brain state: 5 per-conversation subdirs

**Encryption block:** Per-message text and token counts are AES-encrypted at rest with a key in the macOS Keychain under `Antigravity Safe Storage / acct=Antigravity Key`. The key extracts as 16 bytes (base64). Standard schemes tested against a real `.pb` file all fail:

- AES-GCM (nonce-first-12, nonce-last-12)
- Chromium safe-storage PBKDF2 (b64-string and raw-bytes passwords, salt=`saltysalt`, 1003 iters, AES-128-CBC, IV=16 spaces)
- AES-CTR with nonce offsets 0/4/8/16
- ChaCha20-Poly1305 (12-byte nonce, key‖key for 32 bytes)
- Tink prefix envelopes

Entropy stays at 8.000 bits/byte after every attempt. The encryption is implemented inside the 134 MB Go binary at `~/.local/bin/agy` (symbols include `cipher.AEAD`, `*aesCtrWrapper`, `chacha20poly1305`, plus the string `"serializing trajectory: %w"`). Unblocking would require Ghidra/IDA reverse-engineering of the binary's read path. Out of scope for v1.

**Plaintext data the adapter exposes:**
- `agyhub_summaries_proto.pb` — `protoc --decode_raw` parses cleanly: repeated `ConversationSummary { uuid, title, start_ts, last_ts, workspace { uri, git_remote, branch }, ... }`.
- `history.jsonl` — `{display, timestamp_ms, workspace, conversationId}` per line.
- vscdb `antigravityUnifiedStateSync.trajectorySummaries` — nested base64 protobuf, IDE-side equivalent of the summaries.

**Adapter status:** Implemented as beta (`STACKUNDERFLOW_BETA_ANTIGRAVITY=1`). Surfaces 9 conversations across 6 workspaces on the sample machine, 90 records total. All records carry `raw["cost_source"] = "encrypted"` and zero tokens — the cost layer should render "tokens unavailable" rather than infer dollars.

**Value:** MEDIUM — project/workspace coverage and user prompt corpus are accessible; per-message text and token economics are not.

**Recency:** May 23, 2026

**Follow-up to unlock full data:** Reverse-engineer the trajectory read path inside `~/.local/bin/agy`. Specifically, locate the call sequence around `serializing trajectory:` and trace the cipher mode + key derivation. Could yield a stable parser; could also break on every Antigravity release.

---

### 14. Claude.json (Top-level)

**Found?** Yes
**Location:** `~/.claude.json`
**Format Detected:** JSON
**Item Count:** 1 file (499 KB)

**Most Recent Activity:** Apr 30, 2026 17:47

**Content:** Claude CLI global configuration (2,883 startups, theme, tips history)

**Value:** METADATA — useful for session reconstruction.

---

## Priority Ranking

### Tier 1 (Implement First — High Value, Active Data)

1. **Claude CLI** (`~/.claude/`)
   - Size: 5.0 GB
   - Data: 3,496 JSONL files, 1,209 projects, 31 sessions, 19,839 file snapshots
   - Format: JSONL + SQLite
   - Recency: **Apr 30, 2026 (today)**
   - **Why:** Primary Claude interface with extensive structured conversation history and real-time activity

2. **Codex CLI** (`~/.codex/` + Library)
   - Size: 114 MB + 8.7 MB
   - Data: 103-line history, 28 session files, 16,167 log rows
   - Format: JSONL + SQLite
   - Recency: **Apr 30, 2026 (today)**
   - **Why:** Active system with very recent logs and structured conversation history

3. **Cursor IDE** (`~/Library/Application Support/Cursor/`)
   - Size: 1.4 GB
   - Data: 790 state entries, 20+ workspace snapshots, 13 backup versions
   - Format: SQLite vscdb
   - Recency: Feb 16, 2026
   - **Why:** Large footprint, IDE-integrated, contains extension state and workspace data

### Tier 2 (Worth Supporting — Some Data, Moderate Value)

4. **Cline Extension (VS Code)** — 13 MB, 29 complete tasks with API/UI history
5. **Codeium IDE** — 449 MB, protobuf chat states (format requires decoding)
6. **Continue IDE** — 497 MB, SQLite indices (sessions empty)

### Tier 3 (Skip for Now — No/Minimal Local Data)

- Qwen (864 KB)
- Gemini CLI (12 MB, config/auth only)
- SuperMaven (4.0 KB)
- Aider CLI (292 KB)
- Kaysmith (40 KB)
- Cline in Cursor (100 KB)
- VS Code Copilot (<1 KB)

---

Tier 1 providers (Claude CLI, Codex, Cursor) represent 6.5+ GB of recent, high-value data — start implementation there.

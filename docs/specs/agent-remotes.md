# Agent remotes — query other machines' session history in place

**Status:** Design proposal — agreed direction, not yet scheduled.
**Origin:** 2026-07-30. During the tmos-hq migration, one agent (on the Mac)
observed another agent's live session (on tmos-hq) by hand: ssh → query the
remote store → watch messages land within seconds of being written. This spec
productizes that.
**Related:** `docs/cross-agent-knowledge-rfc.md` (the knowledge-layer vision;
names this exact cold-start gap), `docs/specs/sync-hub.md` (the *aggregation*
answer — a hub that merges stores), issue #100 (encrypted aggregate sync).
This is the third point in that design space: **federation**. Don't merge
stores; call them where they live. A copy is stale the moment it lands — a
call never is (observed concretely: the tmos copy of the store overtook the
Mac original within a day, because only tmos had a running watcher).

---

## The metaphor, made precise

A session address is `host + data-dir + project`:

```
tmos-hq : /media/tmos-bumblebe/dev_dev/year26/jul26/stackunderflow-data : <project-slug>
└ area code               └ exchange (the dataset)                        └ number
```

Both halves of the transport already exist as of the July work:

- **`settings.app_dir()` / `$STACKUNDERFLOW_HOME`** made the dataset an
  addressable value instead of a fixed `~/.stackunderflow`.
- **`sync/ssh_store.py`** established ssh as a first-class transport, system
  `ssh` binary, BatchMode, no new dependencies.
- **The `--json` envelopes** (`stackunderflow.memory/1`,
  `stackunderflow.resume/1`) are versioned, golden-fixtured, token-bounded
  contracts. **The envelope is the wire protocol.** Nothing new to invent.

## Form: CLI + skill. Not MCP, not a daemon, not a shared DB.

- MCP was already tried and retired here in favour of the memory CLI
  (`cross-agent-knowledge-rfc.md` header). Don't re-pitch it.
- A live shared SQLite across machines is corruption bait and violates
  local-first. Never.
- No new daemon: the remote end only needs sshd + `stax` on PATH, both already
  true on tmos-hq.
- Agent-facing surface is the existing skills mechanism: the SKILL.md that
  documents `stax memory …` grows a "remotes" section. Any agent that can shell
  out — Claude Code, Codex, Cursor — gets it for free, which keeps the
  provider-neutral property that differentiates this tool.

## Phase 1 — the address book and `--at` (small)

```
stax remote add tmos-hq ssh://user@host.example-tailnet.ts.net:22/media/.../stackunderflow-data
stax remote ls | rm

stax memory sessions  --at tmos-hq [--project <slug>]
stax memory decisions --at tmos-hq "topic"
stax memory ask       --at tmos-hq "question"
stax resume           --at tmos-hq [PATH]
```

Implementation: `remotes` map in config.json →
`ssh <host> "STACKUNDERFLOW_HOME=<dir> stax <argv> --json"` → validate the
`schema` field of the envelope → render exactly as local output (or pass
through raw with `--json`). Unknown/newer schema: print raw, warn, exit 0 —
version skew between machines must degrade, not break.

- Reuses `_SSH_BASE_OPTS` / target parsing from `sync/ssh_store.py`.
- Read-only by construction: the remote command allowlist is the `memory` and
  `resume` namespaces only. `--at` on anything else is an error.
- Auth is ssh's problem (keys, agents, ProxyJump, tailnet) — exactly like the
  sync transport. No credentials of our own.

## Phase 2 — observe (small-medium)

```
stax observe tmos-hq                     # most recent active session there
stax observe tmos-hq <session-id>        # tail one session
```

Poll the remote store for new `messages` rows (`content_text`, `role`, `seq` —
note the column is `content_text`, not `content`), render like a log tail.
Freshness bound = the remote watcher's ingest lag, measured at ~seconds on
tmos-hq. `--json` emits an envelope per batch for programmatic use.

## Phase 3 — the inbox (medium; the actual telephone)

```
stax msg send tmos-hq "P0 mart fix landed, branch feat/…, your move"
stax msg inbox [--json]     # own messages; read marks seen
```

- Wire: a message is a small JSON file under the *recipient's* data dir,
  `inbox/<from-device>/<ulid>.json`, written over the same ssh transport
  (put = temp + rename, exactly the ssh_store discipline). No broker.
- **Delivery to agents rides the existing hook surface** — the same
  SessionStart/UserPromptSubmit hooks that already inject `[StackUnderflow
  memory]` context inject `[StackUnderflow inbox] 1 message from mac: …`.
  That mechanism is proven daily in production sessions.
- Store-and-forward, not chat. Agents don't block on replies; they leave
  word and check back. That matches how agents actually work.
- v1 messages are plaintext (own machines, own tailnet). Upgrade path if ever
  needed: the sync layer's age identity/fingerprint machinery already exists.

## Why this is the right scope

The hard 80% is shipped: addressable datasets, an ssh transport idiom, a
versioned JSON contract, per-second ingest, and a hook channel into live agent
context. Phases 1–3 are wiring, not architecture.

## Non-goals

- Merging stores (that's sync-hub / #100 — sibling designs, untouched).
- Live bidirectional chat, presence, typing indicators. Store-and-forward only.
- Any always-on service beyond sshd and the existing watcher.

## Security notes

- Transcripts contain secrets (verified empirically — a password pasted in a
  session was later retrievable via `memory ask`). `--at` moves that material
  between machines; the boundary is ssh auth plus the fact that both ends are
  the same person's hardware. Document loudly; do not add a false crypto layer
  that implies more.
- `stax remote` URLs may embed usernames/hosts — config.json already holds
  infrastructure detail of this kind; backups of the data dir must keep
  treating it as sensitive.

## First real use (acceptance test)

Two live agents, one on each machine, complete a task using only the tool:
tmos's agent finishes a work item, `stax msg send mac …`; the Mac agent's next
session surfaces it via hook, reads the branch with
`stax memory sessions --at tmos-hq`, and replies. No human relay.

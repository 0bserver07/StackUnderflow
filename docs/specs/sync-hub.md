# Sync Hub — self-hosted cross-device session aggregation

**Status:** Design proposal — not scheduled, not implemented.
**Audience:** maintainer; anyone implementing cross-device sync.
**Scope:** a self-hosted server ("the hub") that aggregates session data from
several of one person's machines into a single store and dashboard.
**Related:** issue #100 (Spec 28 — BYO-bucket sync) is an *independent sibling
design* for the same problem. It is not superseded by this document and this
document does not depend on it — see [Relationship to issue #100](#relationship-to-issue-100).

---

## The problem

A developer with three machines — laptop, desktop, dev container — has three
disconnected stores. `~/.stackunderflow/store.db` is per-machine: each one
sees only the sessions that ran on it. There is no way to ask "how much did I
spend across everything this month" or "show me every session on this
project" without manually copying files around.

The store is derived data — it rebuilds from the source JSONL on the next
ingest. So the unit that actually needs to move between machines is the source
session log, not the database.

---

## What the hub is

A self-hosted server. The user runs `stackunderflow hub serve` on one box they
control — a desktop, a home server, their own VPS. Their other machines pair
to it and push their session logs up. The hub re-runs the ordinary ingest over
everything it receives and serves the ordinary dashboard over the union.

It is not a new analytics product. It is the existing dashboard pointed at a
store that happens to hold every machine's sessions instead of one machine's.

---

## Two non-negotiables

These constrain every decision below. They are stated first so nothing later
quietly violates them.

### 1. It is not, and never becomes, a SaaS

The hub is self-hosted only. There is no maintainer-run service, no managed
tier, no accounts, no multi-tenancy, no billing. The hub is the same
`stackunderflow` package the user already installed, started in a different
mode. "Local-first" widens to "your-infrastructure-first": *local* becomes
your own network, not one laptop. A hosted service holding many users' source
code would betray the project's privacy promise and is explicitly out of scope
forever — not "v2", never.

### 2. It is additive and optional

Every client keeps working fully standalone and offline, exactly as today. The
local `store.db` stays authoritative for that machine. The hub is a *superset*
view, never a dependency: hub down, network gone — the laptop's own dashboard
still runs. A machine that never pairs with a hub never behaves differently.

---

## Why this is tractable — sessions union, they don't merge

Cross-device sync usually means conflict resolution. Here it does not, and the
schema is the reason.

`sessions` is `UNIQUE (project_id, session_id)` (`v001_initial.sql`).
`session_id` is a UUID minted by the coding tool, globally unique, and every
session file is written by exactly one machine. Aggregating N machines is
therefore a **union at the session grain** — no two machines ever write the
same session, so there is nothing to merge.

v1 narrows the surface further: **sync session JSONL only** (`projects/**/*.jsonl`).
Machine-specific config — `settings.json`, `skills/`, `commands/` — stays
local and is never synced. With config excluded, the only data crossing the
wire is append-mostly, device-unique session logs. Conflict resolution, the
hard part of every sync system, **does not exist in v1 by construction.**

The one shared key is `projects (provider, slug)`. Two machines with a
checkout at the same path produce the same slug and merge into one project row
— which is correct, it is the same logical project. Per-machine provenance is
kept by a `device_fk` on `sessions`. The accepted v1 limitation: the same
logical project living at *different* paths on different machines (say
`/Users/x/app` vs `/home/x/app`) yields different slugs and will not merge.

---

## Architecture

```
  laptop                desktop               hub box
  ┌────────────┐        ┌────────────┐        ┌─────────────────────────┐
  │ ~/.claude/ │        │ ~/.claude/ │        │ hub/devices/<uuid>/...  │ ← per-device mirror
  │ store.db   │        │ store.db   │        │ store.db (the union)    │
  │ sync auto ─┼──push──┼─ sync auto ┼──push──┤ hub serve               │
  └────────────┘        └────────────┘        │   ↳ run_ingest per dev  │
        ▲                      ▲              │   ↳ marts refresh       │
        └── own dashboard ─────┘              │   ↳ unified dashboard ──┼── browser
            (still works offline)            └─────────────────────────┘
```

Each device's pushed bytes land in a per-device directory on the hub —
`hub/devices/<device_uuid>/projects/...`. That directory is a `~/.claude/`-shaped
tree. The hub runs the *ordinary* ingest over it. The merge is not a special
code path: it is N ordinary ingests writing into one store, each tagged with a
`device_fk`.

### What it reuses

The hub is mostly existing code in a new arrangement.

| Need | Reused from |
|---|---|
| HTTP app, router registration | `server.py` — `app.include_router()` |
| Ingest + transactional writer | `stackunderflow/ingest/` — `run_ingest`, `writer.py` |
| Resumable offset-based reads | `SourceAdapter.read(ref, since_offset=N)`, `SessionRef.file_size` / `source_kind` (`adapters/base.py`) |
| Per-file byte watermark | `ingest_log.processed_offset` (`v001_initial.sql`) |
| Filesystem watcher (for streaming) | `etl/watcher.py` — `watchfiles`, 200 ms debounce, daemon thread |
| Single-instance lock | `etl/lock.py`, `~/.stackunderflow/server.lock` |
| Marts + dashboard routes | `etl/marts/`, `routes/`, the React build in `static/react/` |
| Schema migration runner | `store/schema.py` — `apply()` |
| Full-tree exclude list (optional mirror) | the backup `excludes` list in `cli.py` |

The genuinely new code is small: a receiver router, a push client, a pairing
flow, and one schema migration.

### The trust model

The hub decrypts and reads everything — it must, to run the pipeline and serve
a dashboard. That is the deliberate, central trade against issue #100.

| | Sync Hub (this spec) | BYO-bucket (#100) |
|---|---|---|
| Storage sees | plaintext | ciphertext — zero-knowledge |
| Can run the pipeline | yes — serves a unified dashboard | no — dumb object store |
| Cross-device merge | one real store, `device_fk`-tagged | virtual UNION view of N snapshots |
| You must trust | the box the hub runs on | nothing — the bucket is opaque |
| Best when | you have a box you control | you only trust object storage |

Because the hub reads plaintext, **it must run on hardware the user controls.**
The privacy contract, stated plainly in the docs and the `hub serve` startup
banner: *the hub holds every machine's source code in readable form; point it
at a box you do not control and you have handed that box your source.* This is
the same trust the user already extends to `~/.stackunderflow/store.db`, which
is unencrypted at rest today — the hub widens that disk, it does not change the
kind of trust.

`hub serve` binds a non-loopback interface (other machines must reach it),
unlike `start`, which is loopback-only. That is exactly why TLS and device
tokens are mandatory for the hub and unnecessary for `start`.

---

## Data model

Two schema changes, one per role. A store can be a client, a hub, or both; the
table irrelevant to a role simply stays empty.

### Hub side — the device dimension

```sql
-- Present when this store backs a `hub serve` instance.
CREATE TABLE devices (
  id           INTEGER PRIMARY KEY,
  device_uuid  TEXT NOT NULL UNIQUE,         -- stable, minted at `sync link`
  label        TEXT NOT NULL,                -- "laptop", "work-mac", "dev-box"
  token_hash   TEXT NOT NULL,                -- hash of the device token
  first_seen   REAL NOT NULL,
  last_seen    REAL NOT NULL,
  revoked      INTEGER NOT NULL DEFAULT 0
);

ALTER TABLE sessions ADD COLUMN device_fk INTEGER REFERENCES devices(id);
CREATE INDEX idx_sessions_device ON sessions(device_fk);
```

`device_fk` is provenance, not identity — sessions already union cleanly by
UUID, so the column only answers "which machine ran this" for filtering and
per-device breakdowns. `NULL` is the sentinel for the hub's own local
sessions (those that predate sync); no backfill is required.

### Client side — pairing state

```sql
-- Present when this machine is paired to a hub.
CREATE TABLE hub_sync_state (
  id            INTEGER PRIMARY KEY CHECK (id = 1),  -- single-row
  device_uuid   TEXT NOT NULL,
  hub_url       TEXT NOT NULL,
  paired_at     TEXT NOT NULL,
  last_push_ts  TEXT,
  push_offsets  TEXT NOT NULL DEFAULT '{}'           -- JSON {source_path: byte_offset}
);
```

`push_offsets` is the per-file byte watermark — the same idea as
`ingest_log.processed_offset`, but tracking how far each file has been *pushed*
rather than how far it has been *ingested locally*. The two pointers are
distinct concerns, so this is a separate table rather than a column on
`ingest_log`.

The device **token is not stored in the database.** It goes in the OS keychain,
or a `~/.stackunderflow/hub-token` file with mode `0600`, and may also be
supplied via env var — the existing `_Opt` env→file resolution already handles
secret-shaped settings that should not sit in `config.json`.

### Why one merged store, not a virtual union

#100's bucket design merges N encrypted snapshots into a virtual `UNION` view
at read time, because object storage cannot run a pipeline. The hub can. It
re-ingests every device's mirror into **one real store**, sessions tagged by
`device_fk`. Every existing `routes/` query, every mart, the whole dashboard
works unchanged — it just sees more rows. No virtual view, no read-time merge.
This is a direct simplification the hub buys over the bucket approach.

### Migration slot

This needs one migration carrying both tables above. #100 has reserved
**v023** for its own `sync_state` table, and HANDOFF marks pre-assigned schema
slots as sacred. The slot for *this* migration is a maintainer decision at
implementation time — this spec deliberately does not assign one.

---

## The wire protocol

### Stage 1 — snapshot push

The client ships raw JSONL byte ranges. It does **not** parse and ship
normalized records: parsing happens once, on the hub, so the hub owns a single
parser version and devices on older schema versions cause no skew.

```
POST /api/hub/push
Authorization: Bearer <device-token>

  frames, each:
  { "path":      "projects/<slug>/<session>.jsonl",
    "offset":    <int>,        # byte offset this frame starts at
    "file_size": <int>,        # total current file size
    "bytes":     <raw> }
```

The client discovers what to watch via each adapter's `watch_paths()`, reads
the new tail of each file (`seek` to `push_offsets[path]`, read to EOF), ships
the frames, and advances `push_offsets`. The hub appends `bytes` into
`hub/devices/<uuid>/<path>`, then runs `run_ingest` over that device's mirror
with `device_fk` set.

- **Append (common):** `offset` equals the hub's current file size → append.
- **Rewrite:** `offset` is 0 (or `file_size` shrank) → the tool rewrote the
  file in place; the hub truncates and replaces. `backup.md` documents that
  Claude Code does rewrite session files in place, so this path is real.
- **Gap:** `offset` exceeds the hub's current size → the hub answers `409`
  with its known size and the client re-ships from there. This makes a dropped
  connection a recoverable event, not a corruption.

### Stage 3 — streaming

The same frames over a persistent connection (WebSocket, or chunked HTTP),
driven by the `watchfiles` watcher that `etl/watcher.py` already uses, instead
of a periodic batch. RPO drops from the push interval to seconds. The watcher
is debounced (the existing 200 ms window coalesces append bursts) and runs in
a daemon thread under a single-instance lock modelled on `etl/lock.py`.

### Encryption and transport

- **v1: TLS + bearer token.** TLS protects the wire; a self-signed cert is
  fine on a LAN, a real cert for a VPS. The device token from pairing
  authenticates each frame.
- **App-layer encryption** (libsodium `crypto_secretstream` via `pynacl`)
  on top of TLS is left as optional v2 hardening for the case where TLS
  terminates at a reverse proxy the user does not fully trust. For the hub it
  is defense-in-depth, not the primary control — unlike #100, where
  client-side encryption *is* the only control because the bucket is
  untrusted. This is where #100's open "age vs pynacl" question would resurface
  if pursued.

---

## Pairing and auth

Single-user throughout — every device belongs to one person. No passwords, no
orgs, no roles.

1. `stackunderflow sync link <hub-url>` on the new machine.
2. The hub, running `hub serve`, prints a short-lived pairing code to its
   console (the device-pairing pattern: the code proves physical access to
   both ends).
3. The user enters the code on the client. The hub mints a device token,
   inserts a `devices` row, and returns the token.
4. The client stores the token (keychain / `0600` file) and writes
   `hub_sync_state`.

`stackunderflow hub devices` lists paired devices and last-seen times.
`stackunderflow hub revoke <uuid>` sets `revoked = 1` — the next frame from
that device is rejected immediately.

---

## CLI surface

| Command | Side | Does |
|---|---|---|
| `hub serve [--host] [--port]` | server | run the hub: receiver + ingest + dashboard |
| `hub devices` | server | list paired devices |
| `hub revoke <uuid>` | server | revoke a device token |
| `sync link <hub-url>` | client | pair this machine to a hub |
| `sync push` | client | one-shot push of new session bytes |
| `sync status` | client | watermark, last push, hub reachability |
| `sync auto --enable / --disable` | client | continuous streaming daemon (Stage 3) |
| `sync pull` | client | bootstrap a fresh machine from the hub (Stage 4) |

`hub` and `sync` are new Click groups alongside the existing `backup` group in
`cli.py`.

> ⚠️ **Namespace overlap.** Issue #100 also defines `stackunderflow sync
> init / push / pull / status / auto` for its bucket backend. If both designs
> ever ship, the `sync` namespace collides and needs disambiguating (e.g.
> `sync --backend hub|bucket`, selecting `hub_sync_state` vs #100's
> `sync_state`). Reconciliation is out of scope here, per the decision to keep
> #100 independent.

---

## Staged rollout

Each stage is independently useful and shippable.

1. **Push + receive + re-ingest.** `sync push` / `hub serve` receiver. The hub
   builds a unified, `device_fk`-tagged store. An off-site copy of every
   machine's *session data* falls out for free — the irreplaceable part. (This
   does not replace `backup create`, which still covers the full `~/.claude/`
   tree including config; see [Open questions](#open-questions).)
2. **Unified dashboard.** `hub serve` runs the existing dashboard over the
   merged store. Multi-device viewing — open the hub's URL, see every machine.
   Mostly wiring: the dashboard already exists; the new surface is an optional
   per-device filter in the UI.
3. **Streaming.** `sync auto` replaces periodic push with continuous delta
   shipping. RPO drops to seconds.
4. **Pull / bootstrap.** `sync pull` — a new machine pulls full history from
   the hub in one command.

---

## Non-goals

- **No hosted service.** Self-host only. (See non-negotiable #1.)
- **No multi-tenancy, accounts, orgs, or billing.**
- **No team mode in v1** — multiple people sharing one hub is v2 at the
  earliest. #100 likewise defers team mode.
- **No config-file sync.** `settings.json`, `skills/`, `commands/` stay
  machine-local. This is what keeps v1 conflict-free.
- **No telemetry.** The project's no-telemetry promise is unchanged.
- **The hub never becomes a required dependency.** (See non-negotiable #2.)

---

## Hard parts and risks

- **The hub sees plaintext.** A compromised hub box exposes every device's
  source code. The only mitigation is "it is your box" — documented loudly, not
  engineered away.
- **New auth surface.** A leaked device token grants read access to all synced
  session data. Revocation must be immediate and obvious.
- **Adapter re-rooting.** Adapters read from fixed roots (`~/.claude/` etc.).
  The hub must run them against `hub/devices/<uuid>/`. This is the one genuine
  new adapter-layer change — see [Open questions](#open-questions).
- **Hub store growth.** The union of N devices grows roughly N× faster than a
  single machine's store. The v008 `messages_YYYYMM` partitioning
  (`docs/specs/messages-partitioning.md`) directly mitigates this; the hub is
  the strongest argument for keeping partitions healthy.
- **Streaming robustness.** Reconnect, gap detection (the `409` + resend
  path), and in-place-rewrite detection all have to be correct or a backup
  silently rots.
- **A real service to operate.** Uptime, TLS cert lifecycle, and the hub box
  needs its own `backup create`.
- **Two sync designs in-repo.** This spec and #100 share the problem and the
  `sync` CLI namespace; that tension is unresolved by decision.

---

## Open questions

- **Adapter re-rooting mechanism** — a `root: Path` parameter on the adapter,
  an env override, or a thin `HubMirrorAdapter` that wraps an existing adapter
  at a new root. The adapter contract already isolates path logic inside the
  adapter, so the change is contained whichever way it goes.
- **Full-tree mirror as an option.** Should the hub optionally mirror each
  device's *entire* `~/.claude/` (reusing the backup `excludes` list), so it
  doubles as an off-site `backup create` target — or stay strictly
  session-JSONL-only in v1?
- **App-layer encryption on top of TLS** — worth it for the
  proxy-terminated case, or is TLS the right stopping point for v1?
- **Auth primitive** — bearer token (simple) vs mTLS client certificates
  (stronger, heavier).
- **Migration slot** — maintainer's call; #100 holds v023.
- **`sync` CLI namespace** reconciliation with #100.

---

## Relationship to issue #100

Issue #100 ("Spec 28 — multi-device sync: opt-in client-side-encrypted BYO
bucket") and this spec are **two independent designs for the same problem,
kept separate by decision.** Neither supersedes the other in the repository.

- **#100 — BYO bucket.** Zero-knowledge: the user's own S3/R2/B2/MinIO bucket
  stores ciphertext, no server runs, devices merge snapshots into a virtual
  union view at read time. Strongest when the user trusts object storage and
  nothing else.
- **This spec — sync hub.** A trusted, self-hosted server reads plaintext,
  runs the real pipeline, and serves one merged dashboard. Strongest when the
  user has a box they control and wants the unified live view.

They are not meant to both ship as-is — they share the `sync` CLI namespace
and the underlying problem. Choosing one, or shipping #100's bucket as a
"no-server tier" beneath the hub, is a maintainer decision this spec
deliberately leaves open. The [trust-model table](#the-trust-model) above is
the core trade-off between them.

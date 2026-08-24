# Multi-Device Sync — opt-in, client-side-encrypted, bring-your-own bucket

**Status:** Phases 1–2 shipped (issue #100, Spec 28 — `size-xl`, wave-6). Phase 1 (one-way encrypted backup, schema v028) and Phase 2 (two-way multi-device read, schema v029) are implemented under `stackunderflow/sync/` + `routes/sync.py`; Phases 3–4 remain design.
**Audience:** maintainer; anyone implementing cross-device sync.
**Scope:** aggregate one person's several machines (laptop + work box + dev container) into one analytics view, by pushing **client-side-encrypted aggregates** to the user's **own** S3-compatible bucket. Zero-knowledge: the bucket stores ciphertext only, no StackUnderflow-hosted service exists, and **raw transcripts never leave the machine**.
**Unblocks the issue's gate:** issue #100 stays `needs-design` "until `docs/specs/sync-protocol-v1.md` lands." This document is that spec (filename `docs/specs/multi-device-sync.md`).

---

## 0. Relationship to the issue text and to `sync-hub.md`

Three framings of the same problem exist in-repo. This spec reconciles them and picks the tightest-privacy option deliberately.

| | Source | Sync unit | Storage trust |
|---|---|---|---|
| **This spec (#100)** | issue #100 + campaign "bigger bet" | **encrypted aggregates only** | zero-knowledge bucket |
| `docs/specs/sync-hub.md` | independent sibling proposal | raw session JSONL | a *trusted* self-hosted server reads plaintext |
| issue #100 body (literal) | original issue | "encrypted SQLite snapshot per device" | zero-knowledge bucket |

**Deliberate scope decision — sync aggregates, not the store.** The issue body imagined shipping an "encrypted SQLite-incremental snapshot per device." The campaign doc (`docs/campaigns/intelligence-layer.md`, *"Bigger bet — privacy-preserving team layer"*) sharpens this to **"encrypted aggregates only (never raw transcripts) … the mart schema (aggregates are already the storage unit)."** This spec honors the campaign framing and narrows the sync unit from *the store* to *the mart aggregates*. Rationale:

1. **Tighter privacy contract.** Transcripts (`messages.content_text`, `raw_json`) categorically never cross the wire — not even as ciphertext. The bucket cannot hold source code at all, encrypted or not.
2. **Smaller payload.** The marts are already the compact roll-up of the raw log (tokens, cost, counts per day/project/model/tool/command). They are orders of magnitude smaller than the raw JSONL.
3. **Structurally conflict-free merge.** Each session is aggregated exactly once, on its origin machine; no device ever re-aggregates another device's session. Cross-device merge becomes an additive union, not a conflict-resolution problem (§5.3).

**Honest limitation of this choice:** cross-device sync delivers the unified **analytics dashboard** (cost, tokens, projects, models, tools, commands, daily trends — all mart-backed). It does **not** let you read or full-text-search *another* machine's transcripts, because the transcript bytes never leave that machine. A user who wants cross-device transcript browsing is describing the `sync-hub.md` trusted-server model instead — a different, deliberately looser trust trade. Both can coexist; see §12.

---

## 1. Goal & scope

### Sync WHAT

**In scope — the aggregate (mart) tables**, the storage unit the dashboard already reads (`python-legacy: store/mart_queries.py`, `stackunderflow/etl/marts/`):

| Mart | Grain | Migration |
|---|---|---|
| `daily_mart` | day × project × provider × model × speed | v006 |
| `session_mart` | session_id (globally unique UUID) | v006 |
| `project_mart` | project | v006 |
| `provider_day_mart` | day × provider | v006 |
| `model_day_mart` | day × model × speed | v006 |
| `tool_mart` | day × project × provider × tool | v007 |
| `command_mart` | day × project × command | v007 |
| `command_day_mart` | day × command | v025 |
| `message_tool_mart` | per message-tool (carries `file_path`) | v011 |

MVP syncs the **Overview/Cost core** — `daily_mart`, `project_mart`, `provider_day_mart`, `model_day_mart`, `session_mart` — which backs the Overview and Cost tabs. `tool_mart` / `command_mart` / `command_day_mart` / `message_tool_mart` are added in a later phase behind the same machinery (they are just more shard families). Whether `message_tool_mart` (which carries `file_path`) is ever synced is a maintainer call (§11) — it leaks the most structure to a future teammate under team mode.

**Explicitly NOT synced, ever:**
- **Raw transcripts** — `messages` (`content_text`, `raw_json`, `tools_json`), the source JSONL, and the per-message `usage_events` **fact** rows. Only the derived marts move.
- **`price_book`** (v024). Pricing is *not* per-device data; it is a shared rate card exercised by `tests/python-legacy: infra/test_pricing_invariants.py`. Keeping it off the wire is what keeps those invariants untouched (§10).
- **Search / Q&A / tags** (`search_index.db`, `qa_pairs.db`, `tags.json` — the `_CRITICAL_ARTIFACTS` sidecars in `cli.py`). These are transcript-derived and machine-local. `backup create` already covers them; sync does not.
- **`mart_watermark`** and other bookkeeping — internal, per-machine, meaningless cross-device.
- **Config** — `settings.json`, `config.json`, `skills/`, `commands/`. Machine-local by design.

### Non-goals (v1)

- **Team mode** — multiple people sharing one bucket. Deferred to v2 (§8, §11). v1 is **single-user, multi-device** only.
- **Cross-user discovery** — someone else's sessions showing up in your store.
- **Peer-to-peer** — no central bucket. Out of scope.
- **A StackUnderflow-hosted service / accounts / billing.** Never. The user brings their own bucket and their own cloud credentials.
- **Cross-device transcript reading / search** — a consequence of aggregates-only (§0).
- **Telemetry** — unchanged: none, ever.

---

## 2. Threat model & privacy contract

The privacy contract is the product. It is stated here so nothing below quietly violates it, and it must be reproduced in the `sync init` banner and the docs.

### What each party can and cannot see

| Party | Can see | Cannot see |
|---|---|---|
| **Bucket host** (S3 / R2 / B2 / MinIO operator) | opaque `age` ciphertext blobs; blob sizes; blob count; upload timestamps; in the MVP object layout also: random per-device UUIDs, mart-family names, month granularity | **any** plaintext — no cost/token numbers, no model names, no project names/paths, no `cwd`, no `file_path`, no transcript, no aggregate value whatsoever |
| **Network observer** | TLS-wrapped S3 traffic to the user's bucket endpoint | contents (TLS) + payload (also `age`-encrypted underneath) |
| **A compromised device** | that device's local plaintext store **and** the sync key ⇒ can decrypt everything on the bucket | nothing more than the user already had on that box |
| **The user** | everything (holds the key) | — |

The MVP layout leaks *structure* (how many devices, which mart families, monthly cadence, sizes) but never *content*. A hardened opaque-manifest layout (§4.4) removes the structure leak too, at the cost of debuggability. The docs must state exactly which layout is in force and what it exposes — no hand-waving.

### Compromised-device analysis

A device holds both the local plaintext store (already true today) and the sync key. So a compromised device is a total compromise of that user's data **plus** read access to the bucket — the bucket adds no *new* confidentiality loss beyond "the attacker can now also read the encrypted off-site copy." Mitigations:

- Sync key never sits in `store.db` or `config.json`. It resolves **env → OS keychain → `0600` file** (§3.2), matching the existing `_Opt` secret-shaped resolution in `settings.py`.
- **Revocation in the shared-key model = key rotation + re-encrypt-all** (documented, heavyweight). There is no per-device revocation in v1 because all devices share one identity. Per-device keys (which *would* enable revocation) are the team-mode machinery, a documented v2 trade (§8, §11).

### Key management

- The user holds the key. **Losing it makes the bucket copy unrecoverable ciphertext** — that is what zero-knowledge means, not a bug. `sync init` prints a loud, unmissable destructive warning and refuses to overwrite an existing identity without `--force` plus an explicit "I have backed up the previous key" confirmation.
- The key is generated locally at `sync init`. It never transits any StackUnderflow-controlled channel (there is none).

### Opt-in gating

- **Default OFF.** With no `sync_identity` row, there is no network, no bucket, no credentials, and the `[sync]` dependencies need not even be installed. Every existing surface behaves exactly as today.
- Enabling sync is an explicit, per-machine `sync init`. No auto-enable, no nag.
- **No telemetry regardless of sync state.**

---

## 3. Crypto design

**Principle: use a well-audited, off-the-shelf format; write zero novel crypto.** We never touch a nonce, a raw AEAD, or a key-schedule ourselves.

### 3.1 Format & primitive — `age`

Encrypt every blob with **`age`** (Filippo Valsorda's audited file-encryption spec) via the **`pyrage`** binding (Rust `age`), shipped under the optional `[sync]` extra. `age` internally does: ephemeral X25519 → HKDF-SHA256 → ChaCha20-Poly1305 over a chunked **STREAM** construction (64 KiB authenticated frames, nonce managed by the format). We consume it as `encrypt(recipient, plaintext) → armored bytes` / `decrypt(identity, bytes) → plaintext`. Nothing lower-level is our responsibility.

`age` is recommended over raw libsodium because it gives three things for free that this design needs:
- **native multi-recipient** — the clean forward path to team mode (encrypt one shard to N teammate public keys), §8;
- **passphrase mode** (scrypt recipient) for users who prefer a memorized secret;
- **streaming AEAD** so large shards don't require whole-blob buffering.

`pynacl` (libsodium `crypto_secretstream`) is the documented fallback if the maintainer prefers a pure-libsodium dependency; it loses native multi-recipient (team mode would then need our own envelope, which we want to avoid). The issue's own list — "`pyrage` (or `pynacl`)" — matches; this spec picks **`pyrage`/age**.

### 3.2 Identity & key derivation

- At `sync init` we generate a random **X25519 age identity** (`AGE-SECRET-KEY-1…`). This single identity is the user's; the user copies it to each of their devices (like a password-manager "secret key"). All of a user's devices share it in v1.
- The matching **recipient** (`age1…`) is the public half. Its **fingerprint** = a short hash (e.g. SHA-256 truncated) of the recipient, used for display and to tag manifests so a wrong-key pull fails fast and legibly.
- **Storage:** env `STACKUNDERFLOW_SYNC_KEY` → OS keychain → `~/.stackunderflow/sync-identity` (mode `0600`). Never in `store.db`/`config.json`. `sync_identity.key_fingerprint` stores only the **fingerprint**, never the secret.
- **KDF:** only for **passphrase mode**, where the identity is derived from a passphrase via `age`'s **scrypt** recipient (tunable work factor; the docs warn a weak passphrase weakens everything). In key mode there is no KDF — the X25519 secret *is* the key material.

### 3.3 Granularity — per-shard blobs

Neither per-whole-snapshot (re-uploads everything on any change) nor per-record (tiny objects, leaks cardinality, expensive on S3). **A blob is one shard = one `(mart family, month)`** — e.g. `daily_mart` for `2026-07`. This mirrors the existing incremental philosophy: `messages_YYYYMM` partitioning (v008) and the backup `--link-dest` model both shard by "only the changed part moves." Only shards whose plaintext content-hash changed are re-encrypted and re-uploaded.

Each shard is serialized to a **canonical, deterministic** form (sorted rows, fixed field order, no wall-clock/random — mirroring the determinism discipline the codebase already enforces), then `age`-encrypted. Determinism gives a stable **content-hash** (SHA-256 of the plaintext) that drives idempotency and dedup (§5.4).

### 3.4 Integrity & rollback resistance

- `age`'s per-frame AEAD authenticates every shard: a truncated, corrupt, or tampered blob **fails to decrypt** — never a silent partial read.
- The (encrypted) manifest records each shard's expected **plaintext content-hash**; the puller re-verifies after decrypt, catching a valid-for-key-but-swapped blob.
- The manifest carries a **monotonic generation counter** per device. A puller rejects a manifest whose generation is *lower* than the last one it accepted for that device — blunting a malicious bucket **replaying an old manifest**. This is best-effort: a fully malicious bucket can always *withhold* updates (an availability attack, never a confidentiality one). Documented as such.

---

## 4. Sync protocol

### 4.1 Object layout — per-device write isolation

```
<bucket>/stackunderflow/v1/
  <device-uuid>/                     # random UUID, not tied to hostname/user
    manifest.age                     # encrypted index (the commit point)
    shards/
      daily_mart.2026-07.age
      project_mart.all.age
      provider_day_mart.2026-07.age
      ...
```

**A device writes only under its own `<device-uuid>/` prefix.** It never mutates another device's objects. Pull is strictly read-only against other prefixes. Two consequences:

- **Concurrent multi-device push is conflict-free at the object layer** — disjoint prefixes, no object ever contended.
- The issue's invariant *"merge doesn't write to remote on read"* holds by construction.

### 4.2 Manifest as the commit point (two-phase, crash-safe)

Push is two-phase so a crash can't corrupt a reader's view:

1. **Upload changed shards** (`shards/*.age`). S3 `PUT` is atomic per object, so each shard is all-or-nothing.
2. **Overwrite `manifest.age`** last. The manifest is the *only* object a puller trusts; it lists `{shard_key → {object_key, content_hash, bytes}}` plus the generation counter.

A crash between phases leaves **orphan shards** the current manifest doesn't reference — invisible to readers, reclaimed by the next successful manifest write or a periodic GC (§7). A reader never sees a half-applied state: it reads the old manifest until the new one lands atomically.

### 4.3 Push / pull with an outbox + cursors

**Push (`sync push`)** — one device's changed aggregates → its own prefix:
1. Mart refresh marks affected shards **dirty** and bumps their `generation` in `sync_outbox` (wired into the existing `MartBuilder.refresh` watermark step — a refreshed `(mart, month)` dirties its shard).
2. For each dirty shard, recompute the canonical serialization + content-hash. If the hash equals `last_pushed_hash`, **skip** (idempotent no-op). Else `age`-encrypt, `PUT`, record `last_pushed_hash`, clear dirty.
3. Rewrite `manifest.age` (phase 2). Advance nothing that isn't confirmed uploaded.

**Pull (`sync pull`)** — every *other* device's aggregates → local remote-landing tables:
1. `LIST` the `stackunderflow/v1/` prefixes → the set of remote device UUIDs (skip our own).
2. For each remote device, fetch `manifest.age`, decrypt, enforce the generation-monotonicity check (§3.4).
3. For each shard, compare the manifest's `content_hash` against `sync_cursors.remote_content_hash`. Unchanged → skip download. Changed → `GET`, decrypt, verify hash, upsert into the remote-landing table for that device, advance the cursor.

The outbox is a *push* watermark (how far each shard has been uploaded); cursors are *pull* watermarks (per remote device, per shard, what we last ingested). They are distinct concerns, hence distinct tables — the same reasoning the sync-hub spec applies to `push_offsets` vs `ingest_log.processed_offset`.

### 4.4 Optional hardening — opaque manifest

To also hide *structure* from the bucket, replace readable object keys with `HMAC(key, logical_shard_name)` and let the (encrypted) manifest hold the logical→opaque mapping. The bucket then sees only opaque keys, sizes, count, and timing — not mart names, months, or device count-by-purpose. This is a defined Phase-3 option, not the MVP default (readable keys are debuggable and the confidentiality contract already holds). The docs must state which layout is active and its exact metadata delta.

### 4.5 Re-keying: local `project_id` → stable `(provider, slug)`

The subtle correctness point. `projects.id` is a **local autoincrement — different on every machine.** Marts key on `project_id`, so raw mart rows are **not** cross-device-comparable. At **export**, every shard is re-keyed from local `project_id` to the machine-stable identity **`(provider, slug)`** (a `JOIN projects` at serialize time; `project_mart` already carries `provider`+`slug`). The union (§5) groups on `(provider, slug, …)`, never on a local id. `session_mart` needs no re-key — `session_id` is already a globally-unique, tool-minted UUID.

---

## 5. Conflict resolution & merge

### 5.1 The core claim — aggregates union, they don't conflict

Because we sync **derived aggregates** and **each session is aggregated exactly once on its origin device**, no device ever re-aggregates another device's work. Cross-device merge is therefore an **additive union at the stable grain**, not conflict resolution. Two rules cover everything:

- **Within one device (re-push):** the newest shard **replaces** that device's prior shard for the same `(mart, month)`. A shard is a full restatement of that device's current totals, so replace — never add — is correct. Last-write-wins *within a device*, keyed on the monotonic `generation`, not a wall clock (clock-skew-proof).
- **Across devices (the union):** **SUM** the per-device shards at the stable grain `(provider, slug, day, model, speed, …)`. Contributions come from disjoint session sets, so summing is exact — including `session_count`, which sidesteps the additive-mart DISTINCT-count trap (v007) precisely because sessions never span machines.

### 5.2 The one genuine limitation — same project, different paths

The same logical project checked out at different filesystem paths on two machines (`/Users/x/app` vs `/home/x/app`) produces **different slugs** and will **not** merge — it shows as two projects. This is identical to the limitation `sync-hub.md` documents and is inherent to slug-by-path identity. Documented, not engineered away in v1.

### 5.3 Why double-counting is structurally impossible (a safety advantage)

Snapshot-sync designs risk double-counting when the same session gets aggregated on two machines. **Aggregates-only sync cannot cause that**, because it never moves the *raw sessions* that would be re-aggregated — each session is aggregated once, on its home device, and only the roll-up travels. The *only* way to induce duplication is the user **out-of-band copying raw `~/.claude` JSONL between machines** (e.g. a manual rsync, or also running the `sync-hub` mirror). That precondition — one machine per session — is already assumed by every design in the repo. Enabling BYO-bucket sync is the *sanctioned* cross-device path specifically so the user doesn't hand-copy raw logs. Guidance: **don't do both.**

Defense in depth if it happens anyway: the merge layer dedups `session_mart` rows by `session_id` (globally unique), tie-breaking to the earliest-seen device, and can flag a session claimed by two devices into a `merge_warnings` counter (observability, replacing the issue's speculative `conflict_count`). **Spec #16 (deterministic content-hash import IDs, Wave 2) makes this reliable** — the same session reproduces the *same* id on both machines, so a duplicate is detectable by equality rather than heuristics. That is the cross-device-merge-safety the content-hash IDs were called out to provide.

### 5.4 Idempotency

- Re-push with unchanged data ⇒ content-hash matches `last_pushed_hash` ⇒ **zero uploads**.
- Re-pull with unchanged remote ⇒ manifest hash matches the cursor ⇒ **zero downloads**.
- Determinism (canonical serialization, no `Date.now`/random) guarantees a given dataset always hashes identically — the property the tests assert (§9).

---

## 6. Storage & transport

- **BYO S3-compatible bucket.** `boto3` with a configurable `endpoint_url` speaks to AWS S3, Cloudflare R2, Backblaze B2, and MinIO unchanged. `sync init --bucket s3://my-bucket [--endpoint https://…]`.
- **The code depends on a narrow `ObjectStore` interface** — `put(key, bytes)`, `get(key) → bytes`, `list(prefix) → keys`, `delete(key)`. `boto3` is one implementation; the test fake is another (§9). This keeps S3 quirks out of the sync logic and makes the suite hermetic.
- **Credentials are the user's own cloud creds**, never a StackUnderflow-issued key. They resolve via the standard AWS chain (`~/.aws/…`, `AWS_*` env) or explicit `STACKUNDERFLOW_SYNC_S3_*` env vars — **never** persisted to `store.db` or `config.json` (secret-shaped ⇒ env/keychain only, matching `_Opt`).
- **"No API keys required for core."** The core product — local ingest + dashboard — needs zero credentials and zero network, exactly as today. Bucket creds are required only for the opt-in `sync` commands, and they are the user's own.
- **No StackUnderflow-hosted service, no account, no telemetry.** There is nothing to sign up for.
- **Optional dependency group `[sync]`** = `boto3` + `pyrage`. Import-guarded: a core install never pulls them, and `sync …` with the extra missing prints a one-line install hint instead of a traceback.

---

## 7. Data model (additive tables only)

All additive — no existing table altered, so a store with sync disabled is byte-for-byte unchanged. This supersedes the issue's single `sync_state` table (which conflated identity, push-watermark, and pull-watermark, and assumed a single global `event_id` that doesn't survive re-keying across devices).

```sql
-- Device identity + bucket config. Single row.
CREATE TABLE sync_identity (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  device_uuid      TEXT NOT NULL,          -- random, minted at `sync init`
  key_fingerprint  TEXT NOT NULL,          -- fingerprint ONLY; secret lives in keychain/0600/env
  bucket_url       TEXT NOT NULL,
  endpoint_url     TEXT,                   -- NULL = AWS default; set for R2/B2/MinIO
  layout_version   INTEGER NOT NULL DEFAULT 1,
  created_at       TEXT NOT NULL
);

-- Push watermark: what this device owes the bucket.
CREATE TABLE sync_outbox (
  shard_key        TEXT PRIMARY KEY,       -- "daily_mart.2026-07"
  content_hash     TEXT,                   -- current local plaintext hash
  generation       INTEGER NOT NULL DEFAULT 0,
  dirty            INTEGER NOT NULL DEFAULT 1,
  last_pushed_hash TEXT,
  last_pushed_ts   TEXT
);

-- Pull watermark: per remote device, per shard, what we last ingested.
CREATE TABLE sync_cursors (
  remote_device_uuid  TEXT NOT NULL,
  shard_key           TEXT NOT NULL,
  remote_content_hash TEXT NOT NULL,
  pulled_at           TEXT NOT NULL,
  PRIMARY KEY (remote_device_uuid, shard_key)
);

-- Known peer devices + human aliases ("work-mac", "dev-box").
CREATE TABLE sync_remote_devices (
  remote_device_uuid TEXT PRIMARY KEY,
  alias              TEXT,
  key_fingerprint    TEXT,
  first_seen         TEXT NOT NULL,
  last_seen          TEXT NOT NULL
);
```

**Remote landing tables.** Pulled + decrypted remote mart rows land in per-mart shadow tables suffixed `_remote`, each mirroring its local mart's columns **but** replacing local `project_id` with `(provider, slug)` and adding `device_uuid` provenance — e.g.:

```sql
CREATE TABLE daily_mart_remote (
  device_uuid   TEXT NOT NULL,
  provider      TEXT NOT NULL,
  slug          TEXT NOT NULL,     -- stable identity, NOT local project_id
  day           TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  speed         TEXT NOT NULL DEFAULT 'standard',
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read    INTEGER NOT NULL DEFAULT 0,
  cache_create  INTEGER NOT NULL DEFAULT 0,
  message_count INTEGER NOT NULL DEFAULT 0,
  session_count INTEGER NOT NULL DEFAULT 0,
  cost_usd      REAL NOT NULL DEFAULT 0.0,
  PRIMARY KEY (device_uuid, provider, slug, day, model, speed)
);
```

MVP ships `_remote` twins for the Overview/Cost core (`daily`, `project`, `provider_day`, `model_day`, `session`); the rest follow as their shards are added. A remote device's rows are wholly REPLACE-on-pull for the shards it changed (§5.1).

**The union read overlay (dashboard stays unchanged by default).** A dedicated read module — `sync/merge.py` → `unioned_daily(…)` etc. — computes `local mart (JOIN projects for slug) UNION ALL <mart>_remote`, then `GROUP BY (provider, slug, …)` `SUM(...)`. Routes call it **only** when sync is enabled **and** the caller asks for the cross-device scope (`?scope=all-devices`, or a UI toggle). **Default scope is this-device-only**, so:
- with sync off, not one query changes;
- the merged path is opt-in and off the mart `<100ms` fast-path, so the perf-budget tests are untouched (§10).

**Migration slot.** The issue reserved **v023**, but the schema is already at **v027** (`schema.py: CURRENT_VERSION = 27`) and Wave-2 spec #16 targets the next slot. The `v023` reservation is **stale**. This work takes **the next free additive slot at implementation time (v028+, coordinated with #16)** — a maintainer assignment, per the pre-assigned-slot convention. (Schema-migration numbers are unrelated to the software-version guard; nothing here touches `__version__.py` / `pyproject.toml` / CHANGELOG / tags.)

---

## 8. Phased plan

Each phase is independently useful and shippable.

**Phase 0 — design.** This document. Satisfies the issue's `needs-design` gate.

**Phase 1 — MVP: one-way, encrypted backup-to-bucket. (SHIPPED — schema v028.)**
`sync init`, `sync push`, `sync status`. Encrypt the Overview/Cost-core marts → the user's own prefix. No pull, no merge. Delivers an **off-site, zero-knowledge encrypted backup of your aggregates** and exercises the whole stack — keys, `age`, `ObjectStore`, canonical serialization, outbox, manifest commit — end to end at minimum risk. New module `stackunderflow/sync/`: `keys.py`, `cipher.py`, `bucket.py`, `serialize.py`, `runner.py`; the `sync` Click group beside `backup` in `cli.py`; the additive migration (`sync_identity`, `sync_outbox`).

**Phase 2 — two-way: multi-device read. (SHIPPED — schema v029.)**
`sync pull` + `sync/merge.py` union overlay + `sync_cursors` + `sync_remote_devices` + the five `<mart>_remote` landing tables + the `?scope=all-devices` read path and `routes/sync.py` (`GET /api/sync/status` + `GET /api/sync/overview`). This is the issue's headline goal: laptop + work + dev-container in one analytics view.

`sync pull` LISTs every *other* device's prefix (skipping our own), fetches + decrypts each `manifest.age`, enforces the monotonic-generation replay guard (§3.4), and downloads only shards whose content-hash moved since the last pull — **idempotent: an unchanged peer downloads nothing** (only the tiny per-device manifest, the commit point, is re-read). Each shard's plaintext hash is re-verified before it REPLACE-lands into `<mart>_remote` (month-scoped, so re-ingesting one month never wipes a device's others) and its `sync_cursors` row advances. Pull is strictly **read-only against the bucket** — it never PUTs to any prefix — and never writes `usage_events` / `price_book` / transcripts. `sync/merge.py` then overlays `local (JOIN projects for slug) UNION ALL <mart>_remote`, SUMming at the stable `(provider, slug, …)` grain, and dedups `session_mart` by the globally-unique `session_id` (deterministic local-then-lowest-device tiebreak) into a `merge_warnings` counter (§5.3).

**Read surface — default-off, byte-identical.** The merged view is opt-in behind an explicit **`?scope=all-devices`**; the default `this-device` scope runs no union at all, so the existing dashboard path is unchanged and off the mart `<100ms` fast-path. `GET /api/sync/overview?scope=all-devices` returns the merged totals / per-day trend / per-project / per-provider-day / per-device breakdown + `merge_warnings`; `GET /api/sync/status` reports local config, known peers, and whether cross-device data is available. CLI: `stackunderflow sync pull` (add `--json` for a scriptable envelope); the merged dashboard read is then live at `/api/sync/overview?scope=all-devices`.

**Phase 3 — daemon, pruning, hardening.**
`sync auto --enable` — a daemon-thread continuous push modeled on `etl/watcher.py` (`watchfiles`, debounce) under a single-instance lock modeled on `etl/lock.py`. Retention: prune shards older than `--keep-months` **after** merge confirmation; GC orphan shards not referenced by the current manifest. Optional opaque-manifest layout (§4.4).

**Phase 4 — team mode (v2, deferred; explicitly out of scope for v1).**
`age`'s native multi-recipient: encrypt each shard to N teammates' public keys; a shared bucket; per-device/per-member keys (which finally enable *revocation*). Team mode additionally requires **redacting path-shaped fields** (`session_mart.cwd`, `project_mart.path`/`display_name`, `message_tool_mart.file_path`) that would leak one member's filesystem structure to another — a redaction pass with no v1 analog. Deferred by the issue and by this spec.

---

## 9. Test strategy (hermetic — no real network, no real bucket)

- **Fake object store.** A `tmp_path`/in-memory `ObjectStore` implementing `put/get/list/delete`. The whole suite runs against it. A real **MinIO container or `moto`** integration test is *optional*, network-gated, and **not** in the default `pytest` run (CI has no network to a bucket).
- **Crypto roundtrip.** Encrypt on "device A" → `put` to the fake → `pull` on "device B" with the same identity → decrypt → assert mart equality. **Wrong key** ⇒ `age` auth failure ⇒ clean "not encrypted for your key" error, **no local mutation, no partial merge.**
- **Merge correctness.** Two device snapshots sharing `(provider, slug, day)` from **disjoint** sessions ⇒ union **SUMs** (incl. `session_count`). Re-push from one device (new `generation`) ⇒ **REPLACE**, no double-count. `session_id` seen on two devices ⇒ deduped, `merge_warnings` incremented.
- **Re-keying.** Fixtures where "device A" and "device B" assign *different* local `project_id`s to the same `(provider, slug)` ⇒ they merge into one project row; different slugs ⇒ two rows (§5.2).
- **Idempotency.** Re-push unchanged ⇒ **zero** `put`s; re-pull unchanged ⇒ **zero** `get`s (assert call counts on the fake).
- **Determinism.** The same dataset serializes to the same bytes and the same content-hash across runs — no `Date.now`/random in the path.
- **Failure injection.** Fake raises on `put`/`get` ⇒ graceful; outbox keeps its dirty flags; `sync push` exits non-zero (scriptable). Truncated ciphertext ⇒ skip + warn. Manifest references a missing object ⇒ skip + warn, keep last-known merged data. Stale manifest (lower `generation`) ⇒ rejected (§3.4).
- **Default-off invariants.** With no `sync_identity` row: every existing route/query is byte-identical; assert the migration adds only new tables and alters none; assert `test_pricing_invariants.py` and the mart `<100ms` perf tests are unaffected; assert the sync path **never** writes `usage_events` or `price_book`.

---

## 10. Invariants honored

- **Local-first, default-off** — no `sync_identity` ⇒ no network, no bucket, no `[sync]` deps needed; nothing changes.
- **No telemetry** — untouched; sync ships only to the *user's own* bucket, ciphertext-only.
- **No external service dependency for core** — sync is an opt-in extra; the core product needs zero credentials and zero network.
- **Pricing invariants unaffected** — `price_book` is never synced; the sync path never writes `usage_events`; the merge overlay is read-only and opt-in ⇒ `test_pricing_invariants.py` stays green.
- **Mart `<100ms` fast-path** — the union overlay is off the hot path (opt-in `?scope=all-devices`); default single-device queries are unchanged.
- **Additive schema** — new tables only; a sync-off store is unchanged.
- **No rolled-own crypto** — `age`/`pyrage` (or `pynacl`) only.
- **Version guard** — no `__version__` / `pyproject` / `package.json` / CHANGELOG / tag edits. Schema-migration number is a maintainer assignment at implementation time.
- **No MCP** (`548d33f`), **no external-library references** in any user-facing surface — respected (the crypto/cloud deps named here are implementation dependencies in an internal spec, not competitor comparisons).

---

## 11. Open questions (maintainer calls)

- **Migration slot** — v028+, coordinated with spec #16; maintainer assigns.
- **`sync` CLI namespace** collides with `sync-hub.md`'s `sync link/push/pull/status/auto`. If both ever ship, disambiguate via `sync --backend bucket|hub` (this spec owns `bucket`; `sync_identity` vs the hub's `hub_sync_state`). Reconciliation deferred per the "keep #100 independent" decision.
- **Shared-key vs per-device-key in v1.** Shared key is simplest and matches "single user, all my devices," but gives no per-device revocation. Per-device keys (team-mode machinery) enable revocation at v1 cost. Recommendation: **shared key for v1**, per-device keys with team mode in v2.
- **Passphrase mode on by default?** Recommendation: **key mode default**, passphrase (scrypt) opt-in with an explicit weak-passphrase warning.
- **MVP object layout.** Recommendation: **readable keys** (debuggable; confidentiality already holds); opaque-manifest as Phase-3 hardening.
- **Sync `message_tool_mart`?** It carries `file_path`. Harmless under v1 (ciphertext to an untrusted bucket) but the biggest structure-leak to a future *teammate*. Recommendation: **exclude from MVP**, gate behind team-mode redaction.
- **`sync pull --bootstrap`** for a brand-new machine (issue Stage 4 analog) — pull all peers before the first local ingest. Natural Phase-2 add.

---

## 12. Coexistence with `sync-hub.md`

`sync-hub.md` (trusted self-hosted server, reads plaintext, serves a unified live dashboard incl. transcripts) and this spec (zero-knowledge bucket, aggregates-only, analytics view) are **two independent designs for the same problem, kept separate by decision.** The trust trade is the axis:

| | This spec (BYO bucket) | Sync hub |
|---|---|---|
| Storage sees | ciphertext — zero-knowledge | plaintext |
| Transcripts leave the machine | **never** | yes (to your box) |
| Cross-device view | analytics (marts) | full incl. transcripts + search |
| You must trust | nothing (opaque bucket) | the box the hub runs on |
| Best when | you trust only object storage | you have a box you control |

Neither supersedes the other. Shipping this as a "no-server tier" beneath the hub, or shipping only one, is a maintainer decision left open here.

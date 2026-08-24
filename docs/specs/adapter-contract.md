# Adapter Contract — implementing a new `SourceAdapter`

**Audience:** anyone adding a 21st coding-tool integration to StackUnderflow.
**Scope:** the `SourceAdapter` Protocol and the dataclasses it produces. The store-side schema your records land in is documented separately in [session-schema-v1.md](session-schema-v1.md).

A source adapter is the bridge between a coding tool's native on-disk format (JSONL, vscdb, SQLite, JSON blob) and StackUnderflow's raw layer. The adapter answers two questions: *what sessions exist?* and *what records do they contain?* The ingest layer drives adapters; nothing downstream — routes, services, the memory CLI — ever touches an adapter.

---

## The Protocol

`python-legacy: adapters/base.py` defines the contract:

```python
class SourceAdapter(Protocol):
    name: str

    def enumerate(self) -> Iterable[SessionRef]:
        """Yield every session this adapter can see on disk."""

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Yield records from `ref`, starting at `since_offset` bytes in."""

    def watch_paths(self) -> list[Path]:
        """Return root paths the watcher should follow for live ingest."""
```

Three methods. `enumerate()` and `read()` are mandatory; `watch_paths()` is optional — return `[]` (or omit entirely) for periodic-only ingest.

### `name`

A short string used as the `provider` value in `projects.provider`, `messages` rows (via the joined column), and `usage_events.provider`. Lowercase, single word, no spaces. Examples: `claude`, `codex`, `cursor`, `cline`, `kilocode`, `opencode`, `cursor_agent`. Use the underscore form (`cursor_agent`) for compound names; the registry key matches the provider value verbatim.

### `enumerate() -> Iterable[SessionRef]`

Discovery. Walk the on-disk format and yield one `SessionRef` per parseable session. Cheap operation — called every ingest cycle. Defer expensive work (parsing message bodies, computing token totals) to `read()`.

```python
@dataclass(frozen=True, slots=True)
class SessionRef:
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
    source_kind: Literal["file", "database"] = "file"
    source_hint: dict[str, Any] | None = None
```

`project_slug` deserves care. It's the join key in `projects` (`UNIQUE(provider, slug)`); rows that share a slug collapse into the same project. The Cursor adapter learned this the hard way (see `v005_cursor_workspace_redistribute.py`): it originally stamped a fixed `slug = "cursor"`, collapsing every workspace into one row. The fix derives a per-workspace slug from the file paths the conversation references. **Pick a slug that maps 1:1 with the user's mental "project" boundary.**

`source_kind` distinguishes JSONL files (resume by byte offset) from SQLite-backed sources (resume by rowid). `source_hint` is adapter-private metadata (vscdb key prefix, conversation id, table name) that `read()` will need but the rest of the pipeline ignores.

### `read(ref, *, since_offset=0) -> Iterable[Record]`

Parsing. Yield one `Record` per source-message starting at `since_offset` bytes in (or rows in, for `source_kind="database"`). `since_offset=0` means "from the beginning."

```python
@dataclass(frozen=True, slots=True)
class Record:
    provider: str
    session_id: str
    seq: int
    timestamp: str
    role: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str
    tools: tuple[str, ...]
    cwd: str | None
    is_sidechain: bool
    uuid: str
    parent_uuid: str | None
    raw: dict
    speed: Literal["standard", "fast"] = "standard"
```

Field-by-field guidance:

- **`provider`** — same value as `self.name`.
- **`session_id`** — opaque string; must be stable across re-reads of the same source.
- **`seq`** — monotone integer within a session. The writer uses it as the dedup key (`UNIQUE(session_fk, seq)`). Start at 0 or 1 — convention is 0 — and increment by 1 per record.
- **`timestamp`** — ISO 8601 UTC. Used to derive `day` (`YYYY-MM-DD`) at normalize time; malformed timestamps route to `messages_unknown` post-v008.
- **`role`** — `'user'`, `'assistant'`, `'system'`, or whatever role string your provider uses. Normalizers filter on this; the canonical billable role is `'assistant'`.
- **`model`** — model id verbatim from the source. `None` is fine for non-assistant rows. Don't synthesise placeholder strings — Claude's `<synthetic>` sentinel is now stripped to `NULL` (v004) precisely to avoid that pattern.
- **Token counts** — the canonical 4-token shape (`input_tokens`, `output_tokens`, `cache_create_tokens`, `cache_read_tokens`) used by Anthropic. Providers with a different shape should flatten at adapter level — see `OpenAIPricer.normalize_tokens()` for the Codex-shape transform (subtract `cached_input_tokens` from `input`, fold `reasoning_output_tokens` into `output`). If your source format has no token counts, use `0`s and rely on the normalizer's `cost_source='estimated'` path.
- **`content_text`** — the message body as a string. Used for search, length-based estimation, and skill synthesis. Strip nothing — preserve markdown / code blocks verbatim.
- **`tools`** — a tuple of tool-name strings invoked by the message. An assistant turn that called `Read` three times and `Edit` once should yield `("Read", "Read", "Read", "Edit")`. The mart builders distinguish distinct vs. total via `event_count` (distinct) and `calls_total` (total).
- **`cwd`** — current working directory if your provider records it; informational, surfaced in the dashboard.
- **`is_sidechain`** — `True` for sub-agent / Task-tool fan-outs that aren't the primary conversation thread. Claude Code uses this for parallel agents; most providers leave it `False`.
- **`uuid`** / **`parent_uuid`** — message identifiers if your provider has them. Powers conversation-tree reconstruction.
- **`raw`** — the verbatim provider payload as a dict. Preserved in `messages.raw_json` so future code can re-parse fields the adapter didn't surface. Don't trim it — disk is cheap.
- **`speed`** — `'fast'` for Anthropic priority-tier rows (which bill at ~6× standard rates), `'standard'` for everything else. Only the Anthropic pricer interprets this today; safe default for non-Anthropic adapters.

### `watch_paths() -> list[Path]`

Live ingest. Return canonical roots that the watcher (`python-legacy: etl/watcher.py`) should monitor for changes. JSONL adapters return their parent directory; vscdb-style adapters return the SQLite file itself (`watchfiles` fires on byte-level change either way).

Returning `[]` opts out of live-watching — the adapter falls back to periodic ingest. Most beta adapters do this until they've been validated against real source data.

---

## Registration

Adapters register themselves at package import time. The registry is **self-discovering**: `python-legacy: adapters/__init__.py` walks the package and registers every class satisfying the `SourceAdapter` shape, so the adapter file needs no `register()` call and `__init__.py` needs no per-adapter import:

```python
# python-legacy: adapters/myprovider.py — no registration boilerplate; being a
# SourceAdapter-shaped class in this package is enough to be discovered.

class MyProviderAdapter:
    name = "myprovider"
    def enumerate(self): ...
    def read(self, ref, since_offset=0): ...
```

Every adapter is always on; one whose source directory is absent simply yields nothing. Per-adapter fidelity lives in `stackunderflow/adapters/capabilities.json`, where a `beta` status means *pending broad validation*, not opt-in.

> **(Superseded: adapters are now always-on, flags removed.)** Earlier revisions gated adapters behind a `_beta_enabled("NAME")` check reading `STACKUNDERFLOW_BETA_NAME`, and treated turning an adapter on as a release decision. That mechanism was removed — adapters self-register unconditionally.

The matching normalizer is discovered the same way from `stackunderflow/etl/normalize/` (keyed on the class `provider_name`, plus optional `provider_aliases`). See [session-schema-v1.md § Per-provider normalizer contracts](session-schema-v1.md#per-provider-normalizer-contracts) for the normalizer half of the contract.

---

## Idempotency

The ingest layer calls `enumerate()` every cycle and `read()` every time a session's `mtime` / `size` / `rowid` advances. Both methods MUST be safe to re-run. In particular:

- **`enumerate()` is read-only.** Never write to the store from inside it.
- **`read()` re-emits.** A re-read with `since_offset=0` MUST yield the same records as the first read. The dedup key (`UNIQUE(session_fk, seq)`) catches duplicate writes at the store layer, but the adapter shouldn't rely on that — produce the same records on every call.
- **`since_offset` is the resume cursor.** For `source_kind="file"` it's a byte offset; for `source_kind="database"` it's a rowid. Honour it — re-parsing the whole file every cycle works but defeats the watcher's sub-second latency target.

---

## Privacy + safety

- **No network calls.** Adapters parse local files. A network call inside an adapter is a bug.
- **No prompts in logs.** Log paths, timestamps, counts. Never log `content_text` or `raw` at INFO level.
- **Defensive parsing.** Source formats drift — wrap json/sqlite/decode calls in `try/except` and skip malformed records rather than crashing the ingest cycle. The watcher catches and logs adapter exceptions, but a noisy adapter spams the user's logs. Be defensive about empty / corrupt / partially-written files; the watcher races writers in some setups.
- **Be polite about file handles.** Close files, close cursors. The store opens a long-lived connection; don't pile up handles.

---

## Reference adapters

Read these in this order:

1. **`python-legacy: adapters/codex.py`** — the cleanest JSONL-only example. Single file format, well-defined record shape, clear `read()` loop.
2. **`python-legacy: adapters/claude.py`** — the most-tested adapter; covers JSONL plus the legacy `~/.claude/history.jsonl` fallback and the `<synthetic>` model cleanup.
3. **`python-legacy: adapters/cursor.py`** — the canonical vscdb / SQLite-backed example. Shows `source_kind="database"`, rowid resume, and the per-workspace slug derivation (the bug that motivated v005).
4. **`python-legacy: adapters/cline.py`** — VS Code globalStorage walking. Also home to `KiloCodeAdapter` and `RooCodeAdapter` — sibling extensions on the same parser base, differing only in filesystem root.
5. **`python-legacy: adapters/_streaming.py`** — shared JSONL streaming helper used by several adapters.

The thirteen most recently added adapters are useful as case studies for unusual source formats (Kiro's missing tokens, Codeium's protobuf stub, Pi+OMP's shared parser).

---

## Checklist for a new adapter

- [ ] Implement `SourceAdapter` Protocol in `stackunderflow/adapters/<name>.py`.
- [ ] Pick `name` and a slug derivation that matches the user's project boundary.
- [ ] Set `source_kind` correctly; populate `source_hint` if `read()` needs it.
- [ ] Yield records with the canonical 4-token shape; flatten provider-native shapes inside the adapter.
- [ ] Implement `watch_paths()` only when you've validated live ingest works for your source format.
- [ ] Nothing to register by hand — dropping the module in `stackunderflow/adapters/` auto-registers it. Add the adapter's fidelity row to `stackunderflow/adapters/capabilities.json`.
- [ ] Ship a matching `Normalizer` in `stackunderflow/etl/normalize/<name>.py` and register it in `python-legacy: etl/normalize/__init__.py`. Pick a `cost_source` that honestly describes how cost is derived — see [session-schema-v1.md § cost_source enum](session-schema-v1.md#cost_source-enum).
- [ ] Update the per-provider table in [session-schema-v1.md](session-schema-v1.md#per-provider-normalizer-contracts) and the README provider count.
- [ ] Add fixtures + tests under `tests/stackunderflow/adapters/` and `tests/stackunderflow/etl/normalize/`.
- [ ] Document the source path in `docs/multi-provider.md`. (There is no env-var block to update — registration is automatic.)

---

## See also

- [session-schema-v1.md](session-schema-v1.md) — the on-disk schema your records land in.
- [etl-architecture.md](etl-architecture.md) — the three-layer pipeline.
- [multi-provider/spec.md](multi-provider/spec.md) — the original multi-provider design that the Protocol grew out of.
- `python-legacy: adapters/base.py` — the Protocol + dataclasses (the source of truth).
- `python-legacy: etl/normalize/base.py` — the matching `Normalizer` ABC.

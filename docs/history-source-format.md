# External history-source plugin format (`stackunderflow-history-jsonl-v1`)

Some session sources have no local transcript on disk — they are cloud-gated,
or niche enough that a bespoke adapter is not worth maintaining. For those,
StackUnderflow owns only a **format** and a **runner**: you supply an export
command that streams your history to stdout as this JSONL, and StackUnderflow
validates it and imports it under one `custom` provider.

```
stackunderflow import --history-source <name-or-path>
```

`stax import …` is the same command.

> **Guardrails, not a sandbox.** The export command is *your own code, running
> as you*. StackUnderflow runs it with no shell, a cleared + allowlisted
> environment, and byte/time caps so ordinary footguns (a stray `$(…)` in an
> argv, an env var leaking into a child, a runaway process) can't bite. That is
> **not** a security boundary. If you point the manifest at a hostile command,
> you have already lost. Only wire up commands you trust.

---

## 1. The manifest — `stackunderflow-history-plugin.json`

```json
{
  "schema": "stackunderflow-history-jsonl-v1",
  "source_id": "amp",
  "command": ["amp-export", "--format", "stackunderflow"],
  "cursor": "",
  "timeout_seconds": 120,
  "max_output_bytes": 67108864,
  "env_passthrough": ["AMP_TOKEN"]
}
```

| field | required | meaning |
|---|---|---|
| `source_id` | ✅ | Names the source. Restricted to `[A-Za-z0-9._-]` (it becomes a project slug and an on-disk cursor filename — no spaces, no path separators). One `custom` provider hosts every source; the `source_id` namespaces its projects. |
| `command` | ✅ | The export command as an **argv list** (not a shell string). Run with `shell=False` — nothing is word-split or glob-expanded. |
| `cursor` | | Seed cursor for the very first run (see §4). Opaque. Default: none. |
| `timeout_seconds` | | Wall-clock cap on the export run. Default `120`, hard max `3600`. |
| `max_output_bytes` | | Cap on stdout the runner will buffer. Default `64 MiB`, hard max `512 MiB`. Exceeding it fails the import. |
| `env_passthrough` | | Extra parent-env variable **names** forwarded to the command (an allowlist — see §3). Default: none. |
| `schema` | | If present, must equal `stackunderflow-history-jsonl-v1`. |

### Resolving `--history-source`

The value is resolved in order:

1. an existing file (the manifest itself);
2. an existing directory containing `stackunderflow-history-plugin.json`;
3. a named source under `./.stackunderflow/history-plugins/<name>/` (project-local),
   then `~/.stackunderflow/history-plugins/<name>/`.

---

## 2. The stream — one JSON object per line

The command writes UTF-8 JSONL to **stdout**. Each line is one object with a
`type`. Blank lines are ignored. The **entire** stream is validated before any
row is written, so a malformed line late in the stream can never leave half an
import committed (see §5).

Record shapes mirror the internal adapter DTOs.

### `session`

Establishes a session and, optionally, its project.

```json
{"type": "session", "session_id": "s-42", "project": "billing-service",
 "cwd": "/work/billing-service", "title": "retry storm",
 "first_timestamp": "2026-06-01T10:00:00+00:00",
 "last_timestamp": "2026-06-01T10:05:00+00:00"}
```

- `session_id` (required) — the source's own session identity.
- `project` (optional) — human project name. With it the session lands under
  `<source_id>--<project>`; without it, under `<source_id>`.
- `cwd`, `title`, `first_timestamp`, `last_timestamp` — optional metadata.

A `session` line is optional: a `message` that names a never-declared session
still imports (its project defaults to `<source_id>`).

### `message`

One turn.

```json
{"type": "message", "session_id": "s-42", "seq": 1,
 "timestamp": "2026-06-01T10:01:00+00:00", "role": "assistant",
 "content": "I'll add exponential backoff.", "model": "amp-large",
 "input_tokens": 1200, "output_tokens": 340,
 "cache_read_tokens": 100, "cache_creation_tokens": 0,
 "tools": ["Edit"]}
```

- `session_id`, `seq`, `role` (required). `role` ∈ `user | assistant | system | tool`.
- `seq` (required) — a **non-negative integer that is unique within the
  session across every `message` *and* `file_touch`**, monotonic in emit order.
  It is the stable identity of the record: re-import dedupes on `(session, seq)`.
  A duplicate `seq` in a session is a validation error.
- `timestamp` — ISO-8601. Optional but strongly recommended (it drives the
  monthly partition a row lands in and every time-based rollup).
- `content` — message text. Optional (default `""`).
- `model` — optional. Token fields — optional, non-negative (default `0`).
- `tools` — optional list of tool **names** invoked in the turn.

### `file_touch`

A file the agent read or wrote during the session.

```json
{"type": "file_touch", "session_id": "s-42", "seq": 2,
 "path": "/work/billing-service/service.py", "operation": "edit",
 "timestamp": "2026-06-01T10:02:00+00:00"}
```

- `session_id`, `seq`, `path` (required). `seq` shares the session's sequence
  with messages (so give touches their own slots).
- `operation` (optional, default `edit`) — `read | write | edit | create |
  delete | …`. Mapped to a tool name (`Read`/`Write`/`Edit`).

A `file_touch` becomes a lightweight row whose content carries the path, so the
session surfaces in `stackunderflow memory file <path>` /
`find_sessions_touching_file`.

### `cursor`

Optional; typically last. Reports where the export got to (see §4).

```json
{"type": "cursor", "cursor": "opaque-token-abc123"}
```

If several appear, the last wins. If none appears, the stored cursor is left
unchanged.

---

## 3. Environment

The command runs with a **cleared** environment. Only these are forwarded:

- a fixed base allowlist: `PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`;
- every name you list in the manifest's `env_passthrough` (an allowlist — put
  credentials your export needs, e.g. `AMP_TOKEN`, here);
- `STACKUNDERFLOW_HISTORY_CURSOR`, set by the runner to the cursor to resume
  from (§4).

Everything else in your shell is dropped before the command starts.

---

## 4. The cursor — opaque, stored, replayed

The cursor lets an export resume instead of re-exporting everything. It is an
**opaque string** — StackUnderflow stores it and hands it back, and **never
interprets it**.

1. Before each run, the last stored cursor for this `source_id` (or the
   manifest's `cursor` seed on the first run) is placed in
   `STACKUNDERFLOW_HISTORY_CURSOR`.
2. Your command reads it, emits everything since, and ends with a `cursor`
   record naming its new position.
3. **Only after every row commits** is the new cursor stored.

The cursor is persisted in a sidecar file under the state dir
(`~/.stackunderflow/history_sources/<source_id>.cursor.json`), keyed by
`source_id`. Re-import is safe regardless: ids are content-addressed (§6), so
replaying an already-imported window is a no-op rather than a duplication.

---

## 5. Fail-closed

Every failure aborts the whole import and **leaves the stored cursor
un-advanced**, so the next run replays the same window:

- the command exits non-zero, times out, or overruns `max_output_bytes`;
- any stream line is not valid JSON, not an object, an unknown `type`, or
  violates a record's field rules (bad `role`, negative `seq`, duplicate `seq`,
  …).

Because validation runs over the entire stream *before* the first write, a
malformed line means **nothing** is written — not even the valid lines that
preceded it.

---

## 6. Idempotent, content-addressed ids

Re-importing the same content is a no-op, and imports are safe to merge across
machines, because identity is derived from content rather than from a
machine-local counter:

- the store **session id** is `"<source_id>:<session_id>"` — stable and
  globally distinct;
- each row's **uuid** is a content hash of its fields, so an identical record
  hashes identically on any machine;
- row-level dedupe rides on the existing `(session, seq)` uniqueness, so a
  re-run's `INSERT OR IGNORE` keeps the original rows and ids.

---

## 7. Worked example

A minimal export in any language; here, a shell script:

```sh
#!/usr/bin/env sh
# my-export.sh — reads $STACKUNDERFLOW_HISTORY_CURSOR, writes v1 JSONL.
cat <<'EOF'
{"type":"session","session_id":"s-1","project":"demo"}
{"type":"message","session_id":"s-1","seq":0,"role":"user","content":"hi","timestamp":"2026-06-01T10:00:00+00:00"}
{"type":"message","session_id":"s-1","seq":1,"role":"assistant","content":"hello","model":"m","output_tokens":3,"timestamp":"2026-06-01T10:00:01+00:00"}
{"type":"file_touch","session_id":"s-1","seq":2,"path":"/work/demo/app.py","operation":"edit"}
{"type":"cursor","cursor":"2026-06-01T10:00:01+00:00"}
EOF
```

```json
{"schema":"stackunderflow-history-jsonl-v1","source_id":"demo",
 "command":["/abs/path/to/my-export.sh"],"timeout_seconds":30}
```

```
stackunderflow import --history-source /abs/path/to/stackunderflow-history-plugin.json
```

A runnable fake export used by the test suite lives at
`tests/fixtures/history_source/fake_amp_export.py`.

---

## 8. What lands, and what doesn't (yet)

Imported rows are queryable immediately through the store: the project appears
under the `custom` provider, and `memory file` / `find_sessions_touching_file`
find file touches. Because there is no cost normalizer for the `custom`
provider, custom messages are **not** priced into the cost marts — they carry
tokens but no dollar figure. Adding a `custom` normalizer (to light up cost) is
a separate, additive follow-up.

# Batch E / misc — findings

`routes/misc.py` (134 lines) → `routes/misc.rs`, `services/ollama_proxy.rs`,
`services/pricing_refresh.rs`.

Three items were open at claim time:

| Item | Method | Path | Was | Now |
|---|---|---|---|---|
| `RS-5-079` | `GET` | `/api/pricing` | open, DIV-065 | ported, verified by `rust/PRICING-REFRESH-DIFFER.md` |
| `RS-5-080` | `POST` | `/api/pricing/refresh` | open, DIV-065 | ported, same |
| `RS-5-084` | `GET`/`POST`/`PUT`/`DELETE` | `/ollama-api/{path:path}` | open, DIV-066 | ported, **`!M-ollama` flipped** |

Findings are numbered locally; **the integrator assigns DIV ids from 153.**

---

## The two measurements everything else rests on

Both were taken before any code was written, and both are one command to redo.

**M1 — port 11434 is closed.** `ss -lnt` lists no listener on 11434;
`exec 3<>/dev/tcp/127.0.0.1/11434` and `exec 3<>/dev/tcp/::1/11434` both answer
`Connection refused`; `getent ahosts localhost` resolves to `127.0.0.1` only;
`curl -m 5 http://localhost:11434/api/tags` is `curl: (7) Failed to connect`.

**M2 — outbound HTTPS works.**
`curl -o /dev/null -w '%{http_code} %{size_download}'` against
`https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`
answered **`200 1670646`**. The campaign is *not* network-sandboxed. This is the
opposite of the assumption the mission brief allowed for, and it is what makes
finding 1 a real divergence rather than a shared failure path.

---

## 1. `_fetch_from_litellm` is HTTPS, and this workspace has no TLS — the port is pinned to the failure leg

*Python:* `stackunderflow/services/pricing_service.py:301` —
`urllib.request.urlopen(self.litellm_url, timeout=10)` where `litellm_url` is
`https://raw.githubusercontent.com/…` (line 29).

`grep -nE '^name = "(rustls|native-tls|openssl|reqwest|ureq)"' rust/Cargo.lock`
returns nothing. `hyper` is in the tree for the *server* side only. The batch-E
fence forbids `Cargo.toml` edits, and a raw `TcpStream` reaches :443 and then has
nothing to say to it.

So `services::pricing_refresh::fetch_from_litellm` returns `None` permanently.
That is not a stub standing in for unwritten code: it is *exactly* what
`_fetch_from_litellm` returns when `urlopen` raises, so every caller downstream
is the reference's own code path, fully ported. What is missing is the **success
leg**, not a branch of the port.

Combined with M2, the consequence is mechanical:

* every `/api/pricing` branch that FETCHES diverges (Python `source: "litellm"`,
  Rust `source: "cache"`/`"default"`);
* every branch that does NOT fetch is byte-identical;
* `POST /api/pricing/refresh` diverges on every call.

**Maintainer decision, not an agent's:** whether `stax-server` should gain a TLS
client (and which one) is a manifest change and a dependency-surface decision.
Until then this is permanent and documented rather than hidden.

## 2. `POST /api/pricing/refresh` — 200 `success` vs 500 `error`

*Python:* `routes/misc.py:47-52`.

`force_refresh()` returns `True` on this host (the fetch works), so the reference
answers `200 {"status":"success","message":"Pricing updated successfully"}` **and
writes** `cache/pricing.json` plus `source='live'` rows into `price_book`.

The port answers `500 {"status":"error","message":"Failed to fetch pricing from
LiteLLM"}` — the reference's own `else` branch, confirmed against the reference
by failing its fetch through a dead `https_proxy` (an environment condition, not
a patch): `force_refresh() = False`.

Note the two 500 shapes this endpoint has and that they are *different*: the
`else` branch is `{"status", "message"}` and the `except` branch is `{"error"}`.
Both are ported as written.

## 3. `_is_cache_valid` lets `TypeError` escape — a naive cached timestamp is a 500

*Python:* `pricing_service.py:266-276` (and identically `278-290`).

```python
try:
    cache_time = datetime.fromisoformat(timestamp_str.replace("Z", "+00:00"))
    age = datetime.now(UTC) - cache_time          # ← inside the try…
    return age < self.cache_duration
except (ValueError, AttributeError):              # ← …but TypeError is not caught
    return False
```

`datetime.now(UTC) - naive` raises `TypeError`, which is not in the tuple, so it
escapes `get_pricing` entirely and `routes/misc.py:34` renders it as
`500 {"error": "Failed to get pricing: can't subtract offset-naive and
offset-aware datetimes"}`.

**Measured, not transcribed** (law 6). Reproduced by the port. This is a latent
defect in the reference — a hand-written or older cache file with a naive stamp
turns the pricing tab into a 500 — but a case row and a faithful port come before
a fix, and the fix is `pricing_service.py`, not this member's file.

## 4. `cache_data["pricing"]` is a subscript on both legs — a missing key is a 500

*Python:* `pricing_service.py:55` and `:78`.

A cache file that parses but has no `pricing` key raises `KeyError`, whose
`str()` is the key's *repr*: `500 {"error": "Failed to get pricing: 'pricing'"}`,
quotes included. Measured; ported; unit-tested against the measured string.

## 5. A non-dict cache raises `AttributeError` naming the Python type

*Python:* `pricing_service.py:50` — `cache_data.get("timestamp")` after
`if cache_data:`.

`_load_cache` is `json.load`, which happily returns a list, a string, a number or
a bool. Truthiness is checked first, so `[]`/`""`/`0`/`false`/`null` fall to the
no-cache branch, but `[1,2]`, `"hello"`, `5` and `true` reach the `.get` and
raise. Measured, all four:

| cache body | 500 body |
|---|---|
| `[1, 2]` | `{"error": "Failed to get pricing: 'list' object has no attribute 'get'"}` |
| `"hello"` | `… 'str' object has no attribute 'get'"}` |
| `5` | `… 'int' object has no attribute 'get'"}` |
| `true` | `… 'bool' object has no attribute 'get'"}` |

Ported, including the `int`/`float` split `json.load` makes and `serde_json`
preserves.

## 6. `PricingService.__init__`'s `mkdir` moves from startup to per-request

*Python:* `pricing_service.py:33` — `self.cache_dir.mkdir(parents=True, exist_ok=True)`,
run once when `_lifespan` constructs the service.

The port has no service layer, so `PricingService::new` runs per request and
mkdirs then. The observable end state is identical (an empty `cache/` exists),
and the one endpoint that could tell them apart — `GET /api/pricing/doctor` — probes
the cache *file* via `read_cache_status`, not the directory. Recorded rather than
assumed harmless because it interacts with `_maybe_clean_cold_cache`, which
`rmtree`s that same directory at startup.

## 7. The `503` "pricing service is not available" leg is not ported

*Python:* `routes/misc.py:18-22` and `41-45` — `if deps.pricing_service is None`.

`_lifespan` constructs the service inside a `try/except` that only logs, and the
constructor only mkdirs, so `None` means the process could not create a
directory. There is no object in the port that can be absent, and inventing a
nullable service to report on would be the same fabrication `health_check`
already declines to make (see `routes/misc.rs`'s note on the all-`true` services
map). Unreachable on any working install; named, not hidden.

## 8. `_save_to_cache` and the `price_book` live append are not ported

*Python:* `pricing_service.py:178-264`.

Three writes hang off a *successful* fetch: the JSON overlay, an
`append_live_snapshot` of `source='live'` rows into `price_book` (per-token rates
scaled by 1e6, stamped "as of today", provider `anthropic` for every entry), and
`refresh_price_book_cache()` to re-prime the in-memory book. With finding 1 in
force there is no reachable caller, so none of it is written: an untestable
writer aimed at the maintainer's store is not something this campaign should
carry. If TLS ever lands, this is the work that lands with it.

Worth flagging for the architect regardless: that append is the reason a single
`/api/pricing` call can change a dollar figure in an unrelated endpoint's answer.

## 9. `RATE_CARD` is frozen at import; `crate::pricing::engine` is built per request

*Python:* `infra/costs.py:411` — `RATE_CARD = {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}`,
a **module-level** dict evaluated at import time. `get_model_pricing` consults
`_overlay_rates` (i.e. `cache/pricing.json`, `lru_cache`d) and the price-book
seam — and `server.py`'s lifespan flips that seam ON *after* `infra.costs` is
imported. So the `source: "default"` payload is a snapshot of the rate card as it
looked before the server was ready to serve.

The port builds `crate::pricing::engine(&conn, package_dir)` per request
(manifest + `price_book`), which is the *live* source. Three seams therefore
differ in principle: import-time freeze, the JSON overlay, and the book.

No divergence is demonstrable today: the `default` leg is only reached when there
is no cache AND the fetch fails, which cannot happen on this host (M2), and the
port's own probe of the reference under a failed fetch produced the manifest
values — 53 entries, first key `claude-fable-5`, first value
`{"input_cost_per_token": 1e-05, "output_cost_per_token": 5e-05,
"cache_creation_cost_per_token": 1.25e-05, "cache_read_cost_per_token": 1e-06}`,
no nulls — which is what `engine.rate_card()` yields. Recorded as an
**unverified-by-construction** leg, and marked as such in the source.

Law 2 was applied: `crate::pricing::engine`, never `default_engine`.

**Touches DIV-147's seam.** `crate::pricing::engine` does not apply
`settings.model_aliases` while `infra.costs.compute_cost` consults them on every
call. `get_model_pricing` — which is what `RATE_CARD` is built from — calls
`resolve_model_alias(model, _user_aliases())` on line 389, so this leg reads the
alias map on the Python side and not on the Rust side. Still no divergence today
(the alias map is empty on every tested state) and **not fixed here**: `pricing.rs`
is batch A's file and outside this fence.

## 10. httpx's default request headers are not synthesised by the proxy

*Python:* `routes/misc.py:110-116`.

`httpx.AsyncClient` merges its own defaults *under* the caller's headers before
sending: `accept: */*`, `accept-encoding: …`, `connection: keep-alive`,
`user-agent: python-httpx/…`. A browser request supplies most of those and wins,
but a request that omits one gets httpx's.

`services::ollama_proxy` sends only the forwarded headers plus `Host`. This is
invisible to the differ — the difference is in bytes that reach *Ollama*, the one
participant the harness never compares — but it is a real difference and is named
rather than assumed away.

One header IS reproduced deliberately: `httpx._content.encode_content` emits
`Content-Length` **only for a non-empty body**, so a proxied `GET` carries no
`content-length` at all. Adding one "for correctness" would be a byte the
reference does not send.

## 11. A non-UTF-8 header value is dropped by the port and latin-1-forwarded by starlette

*Python:* `routes/misc.py:115` — `request.headers.items()` decodes latin-1, so a
non-UTF-8 header survives (as mojibake) and is forwarded.

`axum::http::HeaderValue::to_str` fails on it, and `forwarded_headers` drops it.
Unreachable from the dashboard, and writing a latin-1 transcoder for it would be
unmeasured code. Named.

## 12. The proxy drops the query string, on both sides

*Python:* `routes/misc.py:107` — `ollama_url = f"http://localhost:11434/api/{path}"`.

The URL is built from the path parameter alone; `request.url.query` is never
read. So `/ollama-api/tags?stream=true` proxies to `…/api/tags` with no query.
Not a divergence — the port does the same thing for the same reason — but it is
reference behaviour that a reader would assume otherwise, and `M-ollama-query`
pins it.

## 13. `/ollama-api` with no trailing slash: 307 vs 404 — DIV-133's class, not a new one

FastAPI's `redirect_slashes` answers `307` for the bare path; axum has no
equivalent and 404s. Identical to `!PL-plan-slash` / DIV-133, which the batch-E
claim assigns to the architect as a `lib.rs` change and puts explicitly outside
this batch's charter. **No case row added**, so the merged file does not grow a
second instance of a known-open class.

## 14. `M-ollama`'s determinism is a property of the machine, not of the code

The flip from `!M-ollama` to `M-ollama` is correct *because* 11434 is closed
(M1). The row does not and cannot self-guard. If Ollama is started on this host:

* `GET /ollama-api/tags` returns the installed model list — different on every
  machine, and different after any `ollama pull`;
* `POST /ollama-api/generate` streams, taking the `transfer-encoding: chunked`
  branch, which is unmeasured on both sides and would be compared as a stream;
* the rows would go red, and the cause would be the daemon, not the port.

The case file carries this as a loud banner with the re-verification commands.
The honest alternative — leaving the row `!` forever — was rejected because a
permanently-suppressed row hides a real regression, and the condition is one
`ss -lnt` away from being checked.

## 15. `tokio`'s `time` feature is used but not declared by `stax-server`

`services/ollama_proxy.rs` uses `tokio::time::timeout` for the 120 s deadlines.
`stax-server`'s `[dependencies].tokio` declares `rt-multi-thread`, `macros`,
`net`, `signal` — **not** `time`. It compiles because `axum`'s `http1` feature
pulls `hyper-util`, which enables `tokio/time`, and cargo unifies features.

That is the "correct only by accident of feature unification" hazard the
workspace `Cargo.toml` already calls out in its `serde_json` comment. It is a
build break waiting to happen, not a silent wrong answer, and the fix is a
one-word `Cargo.toml` edit that this member is fenced out of. **Architect's
call.**

## 16. Two private helpers are now duplicated three ways

* `truthy(&Value)` — Python truthiness over JSON — exists privately in
  `routes/pricing.rs` and now privately in `services/pricing_refresh.rs`.
* `isoformat_utc` / `now_isoformat` — `datetime.now(UTC).isoformat()`, including
  CPython's omission of the fractional part when `microsecond == 0` — exists
  privately in `routes/cost.rs` and now in `services/pricing_refresh.rs`.

Both belong in `crate::pyops` (law 9's list already owns `path_name`,
`char_prefix`, `sql_value`, `COST_KEYS`). Not lifted here because the existing
copies live in other members' files. Named so the dedup is one edit.

---

## Verification

**The shared crate did not compile at hand-off, and not because of this member.**
`crates/stax-server/src/services/benchmark_stats.rs` (the `benchmark` member's
file) declares an `unsafe extern` block for `erf(3)` under `#![forbid(unsafe_code)]`
in `lib.rs`; earlier in the session `services/live.rs` was checked in as a
`SCRATCH FEASIBILITY PROBE` declaring `extern crate futures_core;`, `http_body`
and `tokio_stream`, three crates the manifest does not carry. Neither file is
inside this fence.

So everything below was verified twice: once in the worktree, for what the
worktree could answer, and once in an **isolated copy** at
`$TMPDIR/…/verify/rust` — an `rsync` of `rust/` with `benchmark_stats.rs`
replaced by a placeholder and nothing else changed. The worktree was not
modified to make a gate pass.

| gate | result |
|---|---|
| `rustfmt --edition 2024 --check` on the three files (worktree) | **clean** |
| `cargo build -p stax-server --message-format short` (worktree) | **zero** diagnostics attributable to `routes/misc.rs`, `services/ollama_proxy.rs`, `services/pricing_refresh.rs`; the only errors are `benchmark_stats.rs` ×2 |
| `cargo clippy -p stax-server --all-targets -- -D warnings` (copy) | 12 hits total, **0 in this member's files** (they are in `forks`, `patterns`, `search`, `tags`, `live`, `playback`, `playback_fs`) |
| `cargo test -p stax-server` (copy) | **858 passed, 10 failed** — every failure in another member's module (`sessions::compare` ×2, `forks` ×2, `live`, `patterns` ×2, `playback` ×2, `playback_fs`) |
| `cargo test -p stax-server --lib -- routes::misc services::ollama_proxy services::pricing_refresh` | **34 passed, 0 failed** |

29 of those 34 are new: 13 in `services/pricing_refresh.rs`, 10 in
`services/ollama_proxy.rs` (two standing up a real one-shot HTTP upstream on an
ephemeral port to prove the framing), 6 in `routes/misc.rs` (all driving the
router with `oneshot`).

`rust/PRICING-REFRESH-DIFFER.md` — **both parts run**, on `:8098`/`:8099`, one
home per side. Six deterministic cases byte-identical including status; four
fetching cases divergent exactly as findings 1 and 2 predict. The run also
caught the side effect in the act: **one `GET /api/pricing` appended 24
`source='live'` rows to Python's `price_book` and wrote `cache/pricing.json`,
while the Rust side wrote nothing.** That is the measured form of the argument
for the no-case-row rule — no longer an inference from DIV-059.
`.parity-state/fresh` was untouched.

## Left open

1. Re-run `PRICING-REFRESH-DIFFER.md` Part B against a **release** binary from
   the worktree once `benchmark_stats.rs` compiles. Nothing in the result is
   expected to move (debug vs release changes no response byte here), but the
   caveat is on the record.
2. Findings 1 and 15 are manifest decisions and belong to the architect /
   maintainer.
3. Finding 16's dedup, and finding 9's DIV-147 overlap, are cross-fence edits.
4. Whether axum's `method_not_allowed_fallback` emits the `Allow` header
   starlette sets on its 405 (`Allow: GET, POST, PUT, DELETE` for this route) is
   unmeasured. It is a shared `lib.rs` behaviour, not this route's, and no
   existing case row exercises a 405 on a multi-verb path — `CB-method-post` hits
   a single-verb one. The port's `a_fifth_verb_is_not_claimed_by_the_proxy_route`
   asserts the status only, deliberately.

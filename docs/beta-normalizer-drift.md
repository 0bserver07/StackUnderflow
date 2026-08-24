# Beta normalizer drift report

**Date:** 2026-05-06
**Source of truth:** `docs/specs/multi-provider/codeburn-catalog.md`
**Coverage:** end-to-end via `tests/python-legacy: etl/normalize/test_beta_normalizers.py`

This report walks each beta-flag-gated provider (12 normalizers + 1
discovery-only stub) through real-shape fixtures and grades each against
the canonical event spec. Every entry was validated by passing fixture
data through the registered adapter, then through the registered
normalizer, then asserting the canonical `usage_events` shape, the
expected `cost_source` enum value, and defensive empty/malformed input.

Fixtures live under `tests/fixtures/beta_normalizers/<provider>/`. Each
fixture mirrors the on-disk layout documented in the catalog spec.

| Provider | Status | Notes |
|---|---|---|
| `cursor_agent` | ✅ matches spec | Always-estimated path; 2 events from JSONL fixture; cost_source=`estimated` exclusively. |
| `opencode` | ✅ matches spec | SQLite session/message/part schema honoured; reasoning correctly folded into `output_tokens`; cache.{read,write} mapped to canonical 4-key shape. |
| `qwen` | ✅ matches spec | Token math correct (`promptTokenCount - cachedContentTokenCount`, thoughts folded into output). Previously stamped `unknown`; **resolved 2026-05-13** — the canonical RATE_CARD now covers the full Qwen family (max, plus, turbo, coder, coder-plus, qwen3-coder, auto). |
| `gemini` | ✅ matches spec | Same shape as qwen — math correct. Previously stamped `unknown`; **resolved 2026-05-13** — RATE_CARD now covers gemini-2.5-pro/flash/flash-lite, 1.5-pro/flash, the 3.x forward-looking placeholders, and gemini-auto. |
| `copilot` | ⚠️ minor drift → **fixed** | See "Copilot model-priority swap" below. |
| `codeium` | ✅ matches spec | Discovery-only stub yields zero records, normalizer registered as a no-op generator. Confirmed end-to-end. |
| `continue` | ✅ matches spec | Schema-introspection (`_sniff_schema`) correctly identifies `sessions` + `messages` tables; rate_card cost_source emitted. |
| `droid` | ✅ matches spec | Session-level token totals distributed evenly across assistant messages; thinking tokens folded into output; cache_creation/cache_read mapped from `cacheCreationTokens`/`cacheReadTokens`. Sum-preservation verified (4000 / 1400 / 600 / 2000 totals re-emerge from the per-event split). |
| `kiro` | ✅ matches spec | One Record per execution (whole chat rolled up); always-estimated; model normalisation `claude.3.5.sonnet` → `claude-3-5-sonnet` confirmed end-to-end. |
| `openclaw` | ✅ matches spec | `model_change` event correctly establishes rolling model; explicit usage block mapped to canonical 4-key shape. |
| `pi` (incl. `omp`) | ✅ matches spec | `cacheRead`/`cacheWrite` mapped to canonical names; default `gpt-5` model resolves through RATE_CARD; `omp` provider key routes to the same `PiNormalizer` class. |
| `kilocode` | ✅ matches spec | Cline-family parser shared with `cline.py`; `<model>` tag in first user message resolved correctly; `tokensIn`/`tokensOut`/`cacheWrites`/`cacheReads` parsed out of `api_req_started.text`. |
| `roocode` | ✅ matches spec | Same Cline-family path as kilocode; only the extension-id differs at the adapter layer. |

### Result summary

- **Matches spec:** 12 of 13 (cursor_agent, opencode, qwen, gemini, codeium, continue, droid, kiro, openclaw, pi, kilocode, roocode)
- **Minor drift, fixed in this report:** 1 of 13 (copilot)
- **Broken:** 0

---

## Detail: Copilot model-priority swap (⚠️ → ✅ fixed)

### What was diverging

`python-legacy: adapters/copilot.py::CopilotAdapter.read()` resolved the
model for each `assistant.message` event with this priority:

```
1. event.model   (explicit on the event)
2. tool-call-id prefix inference  (toolu_* → claude-auto, call_* → gpt-auto)
3. current_model (from session.model_change / session.start)
4. "copilot-auto" literal
```

When a session opened with a `session.model_change` declaring a fully
qualified id like `claude-sonnet-4-5-20250929`, **and** a later
assistant message carried a `toolu_*` tool-call id, the second event's
model was downgraded from the explicit
`claude-sonnet-4-5-20250929` to the family-only `claude-auto`. This
silently dropped model granularity in `model_day_mart`, the per-model
FilterBar, and any per-model cost breakdown for the rest of the session.

The codeburn catalog spec is ambiguous here ("Inferred from tool-call
IDs … *when not explicit*") — `current_model` from `session.model_change`
is arguably "explicit" too, since the user / client deliberately
declared it.

### Severity

Medium-low. The wrong field still routes to the right pricer (both ids
canonicalize to Anthropic), so cost computation didn't round-trip to
zero — just lost per-model granularity in marts and the dashboard.

### Fix (applied)

Swap priority so `current_model` wins over the tool-call-id inference:

```python
model = (
    _extract_model(event)
    or current_model
    or _infer_model_from_tool_calls(tool_calls_field)
    or "copilot-auto"
)
```

Plus a regression test
(`test_read_session_model_change_beats_tool_call_id_inference` in
`tests/python-legacy: adapters/test_copilot.py`) that locks in the new
priority order:

> A `session.model_change` of `claude-sonnet-4-5-20250929` followed by
> two assistant messages — the second carrying a `toolu_01abc` tool-call
> — must keep `claude-sonnet-4-5-20250929` as the model on **both**
> records.

### Verification

- The regression test passes against the new priority.
- The two pre-existing inference tests (`test_read_vscode_infers_model_from_tool_call_id`,
  `test_read_infers_openai_from_call_prefix`) still pass — both fixtures
  open without a `session.model_change`, so the inference path remains
  the active fallback when no explicit model has been declared.
- The end-to-end beta-normalizer test for copilot now sees a single
  model in its event set instead of two (`claude-auto` + the explicit
  id) — confirmed in
  `test_beta_normalizer_canonical_event_shape[copilot]`.

---

## Notes on `cost_source=unknown` for qwen / gemini

**Resolved 2026-05-13.** The qwen and gemini fixtures previously stamped
`cost_source='unknown'` because their real model ids (`qwen-coder-plus`,
`gemini-1.5-pro`) were not members of the canonical
`stackunderflow.infra.costs.RATE_CARD`. The pricing sweep extended
`_CANONICAL_IDS` with the full Qwen and Gemini families (and added an
un-dated `claude-3-5-sonnet` alias for Kiro-style normalisation):

```
qwen:    cost=0.00274 USD, cost_source=rate_card  (qwen-coder-plus → QwenPricer)
gemini:  cost=0.00832 USD, cost_source=rate_card  (gemini-1.5-pro  → GeminiPricer)
```

The fix also corrects the normalizer-side provider→pricer routing in
`python-legacy: etl/normalize/base.py::_PROVIDER_TO_PRICER` so that
beta-normalizer rows price against their own provider's rate table
instead of falling through to Anthropic's (which would have invented
roughly 3-4× the correct dollar figure even after RATE_CARD membership
was satisfied).

Regression tests in `tests/python-legacy: etl/normalize/test_beta_normalizers.py`:

- `test_beta_normalizer_fixture_emits_rate_card_cost_source` — every
  fixture-backed beta normalizer (opencode, qwen, gemini, copilot,
  continue, droid, openclaw, pi, kilocode, roocode) yields at least one
  `cost_source='rate_card'` event and zero `'unknown'` events.
- `test_beta_model_id_in_canonical_rate_card` — every representative
  model id (the 16 qwen + gemini variants plus the un-dated
  claude-3-5-sonnet alias) is present in `RATE_CARD` with strictly
  positive input + output rates.

---

## Methodology

The validation was driven by
`tests/python-legacy: etl/normalize/test_beta_normalizers.py`. For each
provider it runs six parametrized assertions:

1. **`test_beta_normalizer_registered`** — provider key resolves through
   the registry.
2. **`test_beta_normalizer_canonical_event_shape`** — fixture →
   adapter.read() → normalizer.normalize() emits canonical-shape events
   with all required keys (`source_message_fk`, `provider`, `model`,
   `speed`, `input_tokens`, `output_tokens`, `cache_read_tokens`,
   `cache_create_tokens`, `cost_usd`, `cost_source`, `ts`, `day`,
   `session_id`, `project_id`, `role`).
3. **`test_beta_normalizer_cost_semantics`** — `cost_usd` is a
   non-negative float; `cost_source` is one of the spec enum values;
   estimation-only adapters (cursor_agent, kiro) stamp `estimated`
   exclusively; explicit-token adapters stamp `rate_card` or `unknown`.
4. **`test_beta_adapter_empty_root_yields_no_events`** — pointing the
   adapter at a non-existent path yields zero events without raising.
5. **`test_beta_normalizer_user_role_yields_no_events`** — user-role
   rows are non-billable; every normalizer drops them.
6. **`test_beta_normalizer_malformed_raw_json_does_not_raise`** —
   malformed `raw_json` does not propagate exceptions to the caller.

Total new test cases: 78 (6 × 13 providers).

Fixtures are checked in for inspection. SQLite-backed providers
(`opencode`, `continue`) ship a `session.json` schema spec that the
test materialises into an actual SQLite DB at `tmp_path` time —
keeping the fixture readable in a text editor while still exercising
the SQLite read path end-to-end.

---

## What was not exercised

- **OMP-vs-Pi adapter split.** The Pi adapter scans both `~/.pi/agent/sessions/`
  and `~/.omp/agent/sessions/`; only the `pi` root is exercised. The
  registry routes both `pi` and `omp` provider keys to the same
  `PiNormalizer` class — verified — but no fixture under `~/.omp` is
  walked.
- **Cursor Agent legacy `.txt` transcripts.** Only the Composer 2
  JSONL format is in the fixture; the legacy `.txt` reader has its own
  path with separate marker-line parsing. Existing
  `tests/python-legacy: adapters/test_cursor_agent.py` covers both
  formats, but the beta-normalizer end-to-end test only exercises one.
- **Live (non-rate-card) cost overlay.** `infra/costs.py` supports a
  LiteLLM-style overlay; the test runs against the static RATE_CARD
  only. Live-overlay round-trip is out of scope for normalizer drift.
- **Real on-disk data.** Per the maintainer's note in HANDOFF, only
  claude/codex/cursor/cline/gemini/droid/qwen have actual local data on
  the maintainer's machine. The other betas were only validated against
  these fixtures. Catching the next "Cursor v3 conversationId-in-the-key"
  for those providers will require live data the maintainer doesn't
  have today.

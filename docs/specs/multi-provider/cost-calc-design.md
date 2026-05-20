# Multi-Provider Cost Calculation Design

**Date:** April 30, 2026  
**Status:** Design Proposal  
**Scope:** Architecture for extending StackUnderflow's cost system to support Cursor, Copilot, Gemini, and other providers alongside Claude and OpenAI.

---

## Section 1: Current State Analysis

### 1.1 How `infra/costs.py` Works

The current system is **model-keyed**, not provider-keyed. Core design:

- **Model families** are encoded in `_Family` enum (26 variants: Claude Opus/Sonnet/Haiku across versions, OpenAI GPT/Codex families).
- **Rates table** `_RATES` is indexed by `_Family` enum, holding tuples of `(input_rate, output_rate, cache_write_rate, cache_read_rate)` in $/million-tokens.
- **Model ID resolution** happens via `_identify(model_id: str) -> _Family`:
  - Splits on hyphens and dots, converts to lowercase set membership checks.
  - Example: `"claude-opus-4-6"` → matches `"opus"`, `"6"`, `"4"` tokens → `_Family.OPUS_46`.
  - Fallback to `_FALLBACK = _Family.SONNET_35` if no match.
- **Dynamic overlay** via `PricingService`:
  - Fetches LiteLLM pricing JSON, re-indexes to `_Family`, merges with hardcoded `_RATES`.
  - All model names (OpenAI, Anthropic, third-party) are coerced into a single `_Family` per model.

### 1.2 Where Costs Are Computed in the Pipeline

**Single entry point:** `compute_cost(tokens: dict[str, int], model: str) -> dict[str, float]`

**Invocation locations** (grep results):
- `aggregator.py` line 18: imported at module level.
- `aggregator.py` lines 293, 337, 342, 373, 442, 526, 711, 792, 912, 1323: called in 10+ collector methods.
  - `_SessionCostCollector.result()` (line 293)
  - `_CommandCostCollector.ingest_interaction()` (line 337)
  - `_ToolCostCollector.ingest()` (line 373)
  - `_RetryCollector.ingest_interaction()` (lines 526, 711)
  - `_OutlierCollector.ingest_interaction()` (line 442)
  - Various summary functions.
- `routes/cost.py`: re-exported but not directly called.

**Pipeline step:** Cost computation happens **in the aggregator**, after records are enriched but during the collector pass:
1. Records arrive with raw token counts (keys: `"input"`, `"output"`, `"cache_creation"`, `"cache_read"`).
2. Each collector independently calls `compute_cost(tokens_dict, model)` when building its output.
3. No normalization or provider-aware transformation happens before the aggregator — it is the aggregator's responsibility.

### 1.3 Codex Token Normalization: The Nested Cached-Tokens Issue

**OpenAI convention:** Cached input tokens are nested inside `input_tokens` in the API response.  
**Anthropic convention:** Cached tokens are separate (`cache_creation`, `cache_read`).

In `adapters/codex.py` lines 299–337, the `_attach_tokens_to_last_assistant()` function normalizes this:

```python
raw_input = int(last_usage.get("input_tokens", 0) or 0)      # Includes cached
cached = int(last_usage.get("cached_input_tokens", 0) or 0)  # The subset to extract
raw_output = int(last_usage.get("output_tokens", 0) or 0)
reasoning = int(last_usage.get("reasoning_output_tokens", 0) or 0)

# Normalize to Anthropic shape: fresh input separate from cached input
updated = Record(
    input_tokens=max(raw_input - cached, 0),      # Fresh only
    output_tokens=raw_output + reasoning,          # Bundle reasoning into output
    cache_creation_tokens=0,                       # OpenAI doesn't bill writes
    cache_read_tokens=cached,                      # Cached tokens moved here
    ...
)
```

This normalization occurs in the adapter layer, before records reach the aggregator. The cost computation layer assumes all records are in Anthropic format (separate cache counts).

---

## Section 2: Multi-Provider Concerns

### 2.1 Known Token-Counting Divergences

| Aspect | Anthropic | OpenAI Codex | Cursor | Copilot | Gemini |
|--------|-----------|--------------|--------|---------|--------|
| Cached input nesting | Separate field | Nested in `input_tokens` | Unknown (no per-msg tokens) | Likely nested | Unknown |
| Reasoning tokens | N/A | In `output_tokens` | N/A | Unknown | N/A |
| Cache write billing | Charged (1.25×) | Not charged (0.0×) | N/A | Unknown | Unknown |
| Token granularity | Per-message | Per-message | Per-session (billing API) | Unknown | Per-message |
| Model name format | `claude-X-Y-ZZZZ` | `gpt-X.Y-codex`, `gpt-5.4` | `cursor-*`, `claude-*` (dot-version, tier-last) | `copilot-*` | `gemini-*` |

### 2.2 Model Name Key Strategy

**Problem:** Different providers emit wildly different model identifiers:
- Anthropic: `claude-opus-4-6`, `claude-sonnet-4-5-20250929`
- OpenAI: `gpt-5`, `gpt-5.3-codex`, `gpt-4o-mini`
- Cursor (per codeburn): `cursor-auto`, `claude-4.6-sonnet` (dot-version, tier-last, no provider prefix)
- Copilot: `copilot-auto`, `copilot-openai-auto`, `copilot-anthropic-auto`
- Gemini: `gemini-2.5-pro`, `gemini-3.1-pro-preview`

**Current approach (insufficient for multi-provider):**
- Single model ID → family mapping via parse-heuristics.
- Works because the codebase only tracks Anthropic + OpenAI, which have distinct prefixes and version conventions.

**Recommended:** Adopt a **`(provider, canonical_model)` composite key**:
- `provider`: source identifier (`"anthropic"`, `"openai"`, `"cursor"`, `"copilot"`, `"gemini"`, etc.).
- `canonical_model`: normalized model name (pin and date suffixes stripped, provider prefix stripped).
- Store rates indexed by this composite; fall back via substring matching on canonical name.

**Rationale:**
- Avoids collisions: `cursor-auto` and `copilot-auto` could both resolve to Claude Sonnet internally, but require different billing assumptions.
- Explicit; no ambiguity from heuristics.
- Scales: adding a new provider is additive, not a refactor of the enum.

### 2.3 Providers with No Per-Message Token Counts

**Cursor's billing model:**
- Does not return per-request token counts in session logs.
- Cost is computed from a separate billing API, usually in aggregate.
- **No per-message cost decomposition possible**.

**Design implications:**
- Some collectors (e.g., `_CommandCostCollector`, `_ToolCostCollector`) will have no input to work with.
- Must either:
  1. Skip cost computation for providers lacking per-message tokens, or
  2. Use a "synthetic" cost allocation (e.g., round-robin split of daily total across commands).
- Must flag the source in output JSON so dashboards can mark cost data as approximate.

---

## Section 3: Three Design Options

### Option A: Provider-Aware Family Enum (Minimal Change)

**Schema:**

```python
class _ProviderFamily(Enum):
    """(provider, family) pairs enumerated at startup from a YAML/JSON config."""
    ANTHROPIC_OPUS_46 = ("anthropic", "opus-4-6")
    ANTHROPIC_SONNET_46 = ("anthropic", "sonnet-4-6")
    OPENAI_GPT_5_CODEX = ("openai", "gpt-5-codex")
    CURSOR_AUTO = ("cursor", "auto")  # Semantic: always resolves to current Cursor default
    COPILOT_AUTO_ANTHROPIC = ("copilot", "auto-anthropic")
    GEMINI_25_PRO = ("gemini", "gemini-2.5-pro")
    # ... etc, ~50–80 variants

_RATES: dict[_ProviderFamily, tuple[float, float, float, float]] = { ... }
```

**Lookup key:** `(provider, canonical_model_name)`

**Lookup logic:**
- `_identify(model_id: str, provider: str) -> _ProviderFamily`
  - Still uses token-based heuristics (e.g., split on hyphens, detect "opus" + "4" + "6").
  - Adds provider hint to resolve ambiguities (e.g., "auto" means different things for Cursor vs. Copilot).
- Fallback chain:
  1. Exact match on `_ProviderFamily`.
  2. Substring match (e.g., `claude-sonnet-4-5-*` → matches family for any date pin).
  3. Provider-default (e.g., Cursor → Claude Sonnet 4.5).

**Cost computation signature:**

```python
def compute_cost(
    tokens: dict[str, int],
    model: str,
    provider: str = "anthropic",  # default for backward compat
) -> dict[str, float]:
    ...
```

**Where computation moves:** Stays in aggregator, but now aggregator must pass `provider` to compute_cost. Requires threading provider through collectors via enriched records.

**Migration path:**
1. Extend `Record` dataclass to include optional `provider` field.
2. Update adapters to populate `provider` on each record.
3. Update aggregator collectors to unpack `(provider, model)` from record.
4. Update all `compute_cost` call sites.
5. Leave hardcoded default for backward compat if provider is missing.

**Pros:**
- Minimal structural change to existing code.
- Heuristic lookup still works; just scoped by provider.
- Fallback behavior is clear and explicit.

**Cons:**
- Enum grows large (50–80 variants); hard to maintain by hand.
- Heuristic fragility increases with more providers (e.g., does "4" in "Cursor 4.6 Sonnet" mean version or tier?).
- No separation of concerns: pricing table and naming heuristics live in same module.

---

### Option B: Pluggable Provider Modules (Modular)

**Schema:**

```
stackunderflow/infra/providers/
  ├── base.py            # ProviderPricer ABC
  ├── anthropic.py       # AnthropicPricer
  ├── openai.py          # OpenAIPricer
  ├── cursor.py          # CursorPricer
  ├── copilot.py         # CopilotPricer
  ├── gemini.py          # GeminiPricer
  └── registry.py        # Provider discovery and setup
```

**Per-provider module structure:**

```python
class AnthropicPricer(ProviderPricer):
    """Anthropic-specific cost logic."""
    
    provider_name = "anthropic"
    
    def get_canonical_name(self, model_id: str) -> str:
        """Strip date/pin suffixes; return 'claude-opus-4-6', etc."""
        return model_id.replace(/..., stripped)  # normalize
    
    def identify_family(self, canonical: str) -> str:
        """Resolve canonical name to pricing family."""
        # Heuristic matching for Anthropic models
        return "opus-4-6" if "opus" in canonical and "4-6" in canonical else ...
    
    def get_rates(self, family: str) -> tuple[float, ...] | None:
        """Look up rates for family, or None if unknown."""
        return self._rates_table.get(family)
    
    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Apply provider-specific token normalization."""
        # Anthropic: no-op (already separate)
        return raw
```

**Lookup hierarchy:**

```python
class PricingService:
    def __init__(self):
        self.providers: dict[str, ProviderPricer] = {
            "anthropic": AnthropicPricer(),
            "openai": OpenAIPricer(),
            "cursor": CursorPricer(),
            ...
        }
    
    def compute_cost(
        self,
        tokens: dict[str, int],
        model: str,
        provider: str = "anthropic",
    ) -> dict[str, float]:
        pricer = self.providers.get(provider)
        if not pricer:
            pricer = self.providers["anthropic"]  # fallback
        
        canonical = pricer.get_canonical_name(model)
        normalized_tokens = pricer.normalize_tokens(tokens)
        
        family = pricer.identify_family(canonical)
        rates = pricer.get_rates(family) or pricer.default_rates()
        
        return self._compute_from_rates(normalized_tokens, rates)
```

**Where computation moves:** Into a stateful `PricingService` (or renamed from the existing one). Aggregator calls `pricing_service.compute_cost(...)` instead of `infra.costs.compute_cost(...)`.

**Migration path:**
1. Create `providers/` subpackage with base ABC and one working implementation (Anthropic).
2. Gradually extract OpenAI-specific logic from `_identify()` into `OpenAIPricer`.
3. Implement stub pricers for new providers (initially return sensible fallback rates from LiteLLM).
4. Wire `PricingService` as a singleton dependency in aggregator.
5. Update all aggregator call sites to use `pricing_service.compute_cost(...)`.

**Pros:**
- Clean separation of concerns: each provider owns its heuristics, rates, token normalization.
- Easy to add a new provider: implement one subclass, register it.
- Token normalization is per-provider, not global (Codex subtraction lives in `OpenAIPricer`, not in adapters).
- Testable in isolation.

**Cons:**
- Larger refactor: must introduce abstract base class and registry.
- Runtime overhead: method dispatch instead of table lookup.
- Requires careful inheritance design to avoid boilerplate.

---

### Option C: Data-Driven Rates Config (Declarative)

**Schema:** YAML/TOML rates file indexed by `(provider, model_name)` tuples.

```yaml
pricing:
  anthropic:
    opus-4-6:
      input_cost_per_token: 0.000015
      output_cost_per_token: 0.000075
      cache_write_cost_per_token: 0.0000187
      cache_read_cost_per_token: 0.0000015
      cache_write_disabled: false
      token_norms:
        - op: "subtract_cached_from_input"  # Apply Codex-style subtraction
          enabled: false
    sonnet-4-6:
      input_cost_per_token: 0.000003
      output_cost_per_token: 0.000015
      cache_write_cost_per_token: 0.00000375
      cache_read_cost_per_token: 0.0000003
      cache_write_disabled: false
      token_norms: []

  openai:
    gpt-5-codex:
      input_cost_per_token: 0.00125
      output_cost_per_token: 0.010
      cache_write_cost_per_token: 0.0  # OpenAI doesn't charge writes
      cache_read_cost_per_token: 0.000125
      cache_write_disabled: true
      token_norms:
        - op: "subtract_cached_from_input"
          enabled: true
    gpt-5.4:
      input_cost_per_token: 0.0025
      output_cost_per_token: 0.020
      cache_write_cost_per_token: 0.0
      cache_read_cost_per_token: 0.00025
      cache_write_disabled: true
      token_norms:
        - op: "subtract_cached_from_input"
          enabled: true

  cursor:
    auto:
      input_cost_per_token: null  # No per-message tokens
      output_cost_per_token: null
      token_count_available: false
      billing_model: "session_api"  # Cursor uses separate billing API
      fallback_to: "anthropic/sonnet-4-6"

  gemini:
    gemini-2.5-pro:
      input_cost_per_token: 0.000001
      output_cost_per_token: 0.000004
      cache_write_cost_per_token: 0.0  # Unknown; assume none
      cache_read_cost_per_token: 0.00000004  # ~10% of input
      cache_write_disabled: true
      token_norms: []
```

**Lookup logic:**

```python
class RatesConfig:
    """Load and query provider/model rates from YAML."""
    
    def __init__(self, config_path: str):
        self.config = yaml.safe_load(Path(config_path).read_text())
    
    def get_rates(
        self,
        provider: str,
        canonical_model: str,
    ) -> RatesEntry | None:
        """Return RatesEntry or None if not found."""
        return self.config.get("pricing", {}).get(provider, {}).get(canonical_model)
    
    def has_token_support(self, provider: str, model: str) -> bool:
        """True if provider/model returns per-message token counts."""
        entry = self.get_rates(provider, model)
        return entry and entry.get("token_count_available", True)
    
    def should_apply_norm(
        self,
        provider: str,
        model: str,
        norm_op: str,
    ) -> bool:
        """Check if a token normalization (e.g., subtract_cached) applies."""
        entry = self.get_rates(provider, model)
        if not entry:
            return False
        for norm in entry.get("token_norms", []):
            if norm["op"] == norm_op:
                return norm.get("enabled", False)
        return False
```

**Where computation moves:** Into a `RatesConfig` service. Aggregator passes `provider` and `model` to lookup, gets back a rates entry. The lookup is **declarative and model-independent** (no heuristics in code).

**Normalization strategy:** Token normalization moves back to adapters or to a normalizer stage in the ingestion pipeline. Each adapter's `read()` method checks if a norm should apply and adjusts accordingly.

**Migration path:**
1. Create initial `rates.yaml` with hardcoded current values + LiteLLM fetch logic.
2. Create `RatesConfig` class with lookup methods.
3. Update aggregator to call `rates_config.get_rates(provider, model)` instead of `compute_cost()`.
4. Implement per-provider token normalization in adapters or a separate normalizer pass.
5. Extend `rates.yaml` for new providers as they're added.

**Pros:**
- Fully declarative: adding a new provider is just YAML, no code changes.
- Easy to version-control and audit rates.
- LiteLLM integration becomes a YAML refresh, not a code-level merge.
- Token normalization is explicit in the config (metadata about how a provider counts tokens).

**Cons:**
- YAML parsing and schema validation overhead.
- Requires schema definition and validation library to prevent user errors.
- Heuristic model matching is removed — must have an exhaustive list of known model names per provider, or use LiteLLM as the source of truth.
- Harder to react to unknown models at runtime (can't infer from naming patterns).

---

## Section 4: Recommendation

**Recommend Option B: Pluggable Provider Modules**

**Rationale:**

1. **Scales without ceremony.** Adding Gemini is writing `gemini.py`, not extending an enum or editing YAML.
2. **Encapsulation.** Token normalization logic (the Codex subtraction mess) lives in `OpenAIPricer.normalize_tokens()`, not scattered across adapters and costs.py.
3. **Testability.** Each provider is a unit-testable class with mocked rates and known inputs.
4. **Future-proof.** When LiteLLM rates change or a provider adds a new billing mode (like Cursor's session-level API), the provider module owns the fallback strategy.
5. **Clear migration path.** Start with one provider (Anthropic), extract OpenAI next, then add stubs for others. No big-bang refactor.

**Rough sketch of the refactor:**

```
Phase 1 (Current):
  infra/costs.py (model-family enum + heuristics)
                  ↓
  aggregator.py (calls compute_cost(tokens, model))

Phase 2 (Proposed):
  infra/providers/base.py (ProviderPricer ABC)
  infra/providers/anthropic.py (AnthropicPricer, extracted from costs.py)
  infra/providers/openai.py (OpenAIPricer, with Codex normalization)
  infra/providers/cursor.py (CursorPricer stub; returns None for per-msg cost)
  infra/providers/registry.py (discovery + setup)
  
  services/pricing_service.py (unified entry point, uses providers/)
                  ↓
  aggregator.py (calls pricing_service.compute_cost(tokens, model, provider))
```

**Cost of refactor:** ~600–800 LOC of new code (pricers), ~300–400 LOC refactored from existing costs.py, minimal changes to aggregator call sites (only add `provider=` param).

**Risk mitigation:**
- Implement Option B in a feature branch with full test coverage before merging.
- Keep Option A (status quo with provider param) as fallback if Option B's abstraction proves unwieldy.
- Use deprecation warnings for the old `compute_cost()` signature during transition.

---

## Section 5: codeburn Reference

codeburn (`src/models.ts`) uses a **data-driven + fallback-heuristic hybrid** approach:

- **Pricing table:** `litellm-snapshot.json` (flat map of model name → `[input, output, cacheWrite, cacheRead]` tuples).
- **Aliases:** `BUILTIN_ALIASES` object maps known variants (e.g., `"anthropic--claude-4.6-opus"` → `"claude-opus-4-6"`).
- **Lookup logic:**
  1. Try with provider prefix (e.g., `"anthropic/claude-opus-4-6"`).
  2. Apply alias resolution.
  3. Strip provider prefix and date suffix.
  4. Prefix-match against cached pricing keys.
  5. Return null if all fail.
- **Token-count fields:** Hardcoded; no per-provider variation (no normalization logic).

codeburn's approach is **simpler than StackUnderflow** because it doesn't need to handle token normalization (OpenAI's Codex subtraction, reasoning bundling) — that's handled upstream in aggregation.

**Key difference:** codeburn doesn't care about token counting differences; it assumes tokens are already normalized. StackUnderflow must handle the normalization itself because it owns the full pipeline from adapter → aggregator.

---

## Summary

- **Current state:** Model-keyed, single enum, heuristic identification, Codex token subtraction in adapters.
- **Multi-provider problem:** Name collisions, divergent token conventions, missing per-message tokens (Cursor).
- **Recommendation:** Pluggable provider modules (Option B) for clean separation and testability.
- **Migration:** Phased; extract Anthropic first, then OpenAI, then add new providers as modules.
- **Cross-reference:** codeburn uses simpler data-driven approach; StackUnderflow's complexity comes from owning token normalization.


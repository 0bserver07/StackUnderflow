# Egress signatures

Declarative D1 signatures (Spec 28 §6), one JSON file per agent, following the
`adapters/capabilities.json` pattern: the catalog is data the engine interprets
— new agents are catalog entries, not code. The copies here are compiled into
the `stax` binary; a checkout or `stax audit --signatures <dir>` overrides them.

## The rules

1. **Verified, not guessed.** Every check cites reality: a documented setting,
   the agent's source, or observed behavior. A key nobody verified does not
   ship — mark the agent `pending` instead, and it audits as *unknown, not
   safe* (§8.3).
2. **No signature without its negative.** Every check needs a positive fixture
   (fires) and a negative fixture (stays silent) in
   `rust/crates/stax-audit/tests/` before it merges.
3. **Every finding carries its veto.** A warning without the exact fix line is
   noise; the loader rejects vetoless checks.
4. **Postures are honest.** `at_risk` / `protected` / `unknown` — there is no
   `safe`. Unreadable artifacts and unmodeled values degrade to `unknown`.

## Shipped signatures

| Agent | Status | Verified against |
|---|---|---|
| grok | 3 checks | Spec 28 §0/§3 — the 2026 upload incident, verified on the maintainer's machine (local `[telemetry]` vetoes outrank the remote flag) |
| claude | 1 check | Claude Code monitoring docs (`CLAUDE_CODE_ENABLE_TELEMETRY`, opt-in, default-off) |
| gemini | 1 check | Gemini CLI config docs (`usageStatisticsEnabled`, **default-on** ⇒ `at_risk_when_unset`) |
| codex | 3 checks | OpenAI Codex config reference + source (`[analytics] enabled` **default-on**; `[otel] log_user_prompt`; startup update check). Note `disable_response_storage` was REMOVED in Sep 2025 — `store:false` is now unconditional, so there is no key to audit |
| copilot | 3 checks | GitHub Copilot CLI docs: `remoteExport` (**default true** — prompts, responses and changed-file details sync to your GitHub account) and `remote` (**default "on"**), plus the deprecated gh-copilot `optional_analytics` that survives uninstall |
| cursor | **pending, permanently** | Verified conclusion, not a gap: Privacy Mode is account-side and server-enforced ("tied to your account, not to a specific app"), so no local file answers the question. `telemetry.telemetryLevel` exists via the VS Code lineage but Cursor documents no commitment to honor it — claiming it as a veto would be a lie |

**Locally auditable ≠ everything.** Training opt-outs and public-code-match
policies for Copilot, and Privacy Mode for Cursor, live in web dashboards. D1
reports what a local file can prove and says `unknown` for the rest.

## Check fields

```jsonc
{
  "id": "agent.check_name",        // stable dedupe key, must start with "<agent>."
  "file": "~/.agent/config.toml",  // artifact; ~ resolves against the scan home
  "format": "json | toml-lite | env",
  "key": "section.key",             // dotted path (or VAR name for env files)
  "uploading_when": [true],         // values that mean "configured to upload" → at_risk
  "safe_when": [false],             // values that mean the veto is present → protected
  "at_risk_when_unset": true,       // unset key/file = absent veto (the Grok shape)
  "title": "what fires, in one line",
  "veto": "the exact fix line",     // mandatory
  "severity": "info|low|medium|high|critical"
}
```

Scope notes: D1 reads config files only (process environment variables are not
audited in v0); `toml-lite` covers sections + scalars — an artifact beyond it
degrades the check to `unknown`, never to silence.

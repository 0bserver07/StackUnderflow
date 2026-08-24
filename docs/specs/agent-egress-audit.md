# Spec 28 — Agent Egress Audit: detecting when a coding agent exfiltrates (or is configured to exfiltrate) your code

*Design spec. Product design owned by the maintainer — this is a spec, not an implementation. No code, no schema migration, no version edits.*

> **Status:** proposed. Modelled on the Grok Build CLI upload incident (§0). **Portable by design** — a revamp can lift the signature catalog (§6) and finding schema (§5) independent of the engine that runs them. The two detectors the maintainer asked for map to §4: **config audit** (D1) and **transcript analyzer** (D3); the operational-telemetry detector (D2) is the bridge that actually catches the Grok-shaped case.

---

## 0. Why now — the incident this is modelled on

Grok Build CLI (v0.2.93) packaged local repositories into `before/after_codebase.tar.gz` and uploaded them to xAI's `grok-code-session-traces` GCS bucket. On this machine that was **14 codebases, 99 upload events, ~472 file-blobs, 2026-06-25 → 07-10** — verified before public disclosure. Three properties make it the canonical test case for this spec:

1. **The evidence was not in the transcript.** All 243 upload-decision lines lived in `~/.grok/logs/unified.jsonl` (the operational log). The files StackUnderflow's `GrokAdapter` ingests — `chat_history.jsonl` + siblings (`grok.py:83`) — contained **zero**. StackUnderflow as built was structurally blind.
2. **The agent never "decided" to upload.** It was a deterministic client-side binary action, gated by a *remote* flag — so it would never appear as a tool call even if we parsed transcripts perfectly.
3. **The obvious opt-out was the wrong lever.** The GUI "coding data sharing" toggle governs *training/retention* (writes `auth.json`); it does not stop egress. The real veto is `~/.grok/config.toml` → `[telemetry] trace_upload=false` / `disable_codebase_upload=true`, which outranks the remote flag (config precedence beats remote settings).

This spec closes that blind spot without pretending StackUnderflow can *prevent* the upload (§7).

---

## 1. Goal & scope

### The question it answers
*"Is any coding agent on this machine shipping my code off-box — or configured to — and if so which agent, what left, when, and how do I stop it?"*

### Three signals, one capability

| Detector | Answers | Grok case? | Data source | Posture |
|---|---|:--:|---|---|
| **D1 Config Audit** | "is it *set up* to upload / train on my code?" | ✅ | agent config & auth files | **preventive** — before a session |
| **D2 Egress-Event** | "did it *actually* upload?" | ✅ | agent operational / telemetry logs | **forensic** — after the fact |
| **D3 Transcript Exfil** | "did the *agent* exfil via a command it ran?" | ❌ (client-side) | session transcripts (already ingested) | **forensic** — per session |

The three are independent and independently shippable. D2 is the one that catches the Grok incident; D3 catches the case D2 cannot (an agent that *itself* runs `curl`/`scp`); D1 is the cheapest and the only *preventive* one — it warns before you ever start.

### In scope
- A declarative **egress-signature catalog** (per provider) + an engine that evaluates it against on-disk artifacts.
- A single **`EgressFinding`** record shape all three detectors emit (§5).
- **Surfacing** via the memory CLI, a SessionStart hook, and a dashboard panel (§ surfacing) — advisory only.

### Out of scope
- **Blocking / preventing** egress. StackUnderflow observes; it never sits in the network path. Prevention is the config veto, a firewall, or network policy (§7).
- **Wire-level capture** (pcap / intercepting proxy). The only fully trustless source, but outside the current local-file model. The schema reserves a `wire` detector slot (§6) so a proxy log can be ingested later as a fourth source.
- **Auto-remediation.** D1 may *surface* the fix (e.g. the veto lines); it never applies it (matches Spec 27's "just a nudge, no auto-remediation" invariant).

---

## 2. Grounding — the reuse surface (what already exists)

- **`python-legacy: infra/egress.py`** — `guard_json_body(...)`, `OLLAMA_EMBED_KEYS`, `OLLAMA_CHAT_KEYS`. The outbound allowlist chokepoint for *our own* calls. This spec is its mirror image: detecting *other* agents' egress. Same mental model (an explicit boundary), inverted direction.
- **`adapters/base.py`** — the `SourceAdapter` Protocol (`enumerate` / `read` / `watch_paths`) and `WatchableAdapter` (`:125`). The `EgressDetector` interface (§6) parallels it deliberately, and the Grok adapter already implements `watch_paths()` (`grok.py:143`) — a detector can ride the same watch root for near-real-time firing.
- **`adapters/capabilities.json`** — the existing declarative, per-adapter capability catalog. The egress-signature catalog (§6) follows this exact pattern, so it stays data, not code, and ports cleanly to the revamp.
- **`etl/watcher.py`** — the live-ingest watcher. D2 hooks here: the moment Grok appends `repo_state.upload.start`, the watcher can raise a finding.
- **`hooks/inject.py`, `hooks/recall.py`, `hooks/proactive.py`, `hooks/templates.py`** — the in-session surfacing path (`additionalContext`), token-bounded, silent-on-failure, server-independent. The home for D1's preventive SessionStart warning.
- **`reports/patterns.py`** — `_normalise_command` and `mine_patterns` (per Spec 27's grounding). D3 must normalize a pending/observed command identically to match a signature.
- **`settings.py`** — `_Opt(default, ENV)` descriptor (`:25`), env → file → default. Where maintainer-tunable knobs live (severity floor, allowlists, default-on toggle).

---

## 3. Data-source map (where each signal actually lives)

| Provider | D1 config | D2 ops-log | D3 transcript |
|---|---|---|---|
| **grok** (worked example) | `~/.grok/config.toml` (`[features].telemetry`, `[telemetry].trace_upload`, `.disable_codebase_upload`), `~/.grok/auth.json` (coding-data-sharing) | `~/.grok/logs/unified.jsonl` → `repo_state.upload.*`, `trace.upload.decision` | `~/.grok/sessions/**/events.jsonl` tool calls (already ingested) |
| claude / codex / cursor / copilot / … | *signature TODO per provider* | *signature TODO — many have none* | already ingested; shared D3 rules apply |

**Honest nuance for D1:** on Grok the *effective* upload state is remote-driven, so a local config read cannot know whether uploads are currently on. D1 therefore reports **posture** — "no local veto present → at risk" vs "veto present → protected" — not the live remote value. Confirming an *actual* upload is D2's job. State this in the finding, don't overclaim.

---

## 4. The detectors

### D1 — Config Audit ("notice about training / uploads")
For each provider signature, read the config/auth artifacts and evaluate declared predicates:
- **upload-enabled / no-veto** — e.g. Grok `trace_upload` unset or true **and** `disable_codebase_upload` unset → *at-risk* finding with the exact remediation lines.
- **training/retention opt-in** — e.g. the coding-data-sharing flag set to retain → informational finding, clearly distinguished from egress (the incident's core confusion).
- **permission posture** — auto-approve / yolo / always-approve modes that widen blast radius (context, lower severity).

Output: at most one finding per provider per artifact, deduped by `(provider, signature_id)`. Because D1 needs no session, it is the only detector that can fire **before** work starts (SessionStart hook).

### D2 — Egress-Event Detector (operational / telemetry logs)
Tail/scan the provider's ops log for signature events. Grok signature:
- match `msg == "repo_state.upload.start"` → extract `repo_path`, and the paired `repo_state.upload.enqueued` `gcs_path` / `blobs` / `size_bytes` → one finding per upload, scope = repo + file-count + bytes.
- match `trace.upload.decision` with `uploads_enabled == true` → finding: "uploads currently enabled (source: remote|config)".
- Treat a **missing or schema-changed** log as `unknown`, never `safe` — ops logs are undocumented and unstable.

This is the detector that would have fired on **2026-06-25**, over two weeks before disclosure.

### D3 — Transcript Exfil Analyzer (agent-driven)
A classifier over the tool calls already in the raw layer. Signature families (each with a severity and an allowlist escape hatch):
- **network write** — `curl` with `-T/-d/--data/-F/-X POST|PUT`, `wget --post-*`, `http PUT`.
- **remote copy** — `scp`, `rsync … user@host:`, `sftp`.
- **new git remote → push** — `git remote add` to a non-allowlisted host followed by `push`.
- **tunnel / raw socket** — `nc`, `ncat`, `socat`.
- **encode-then-exfil** — `base64|gzip|tar … | (curl|nc|http)`.
- **cloud/paste CLIs** — `aws s3 cp`, `gcloud storage cp`, `gh gist create <file>`, uploads to `transfer.sh` / `0x0.st` / `file.io` / pastebins.
- **secret→network heuristic** — a read of a secret-shaped path (`.env`, `id_rsa`, `*credentials*`, `.aws/`) followed within *N* tool calls by any network-egress command. Highest severity.

D3 rides the ingested transcript, so it works for **every** provider at once — but by construction it cannot catch client-side uploads (the Grok case). That asymmetry is the reason all three exist.

---

## 5. The finding schema (portable artifact #1)

One shape for all detectors, so surfacing and storage are uniform:

```
EgressFinding:
  provider        : str            # "grok"
  detector        : "config" | "event" | "transcript" | "wire"
  signature_id    : str            # stable key, e.g. "grok.repo_state_upload"
  severity        : "info" | "low" | "medium" | "high" | "critical"
  posture         : "at_risk" | "occurred" | "protected" | "unknown"
  session_id      : str | None     # None for D1
  first_seen_ts   : str
  last_seen_ts    : str
  title           : str            # "Grok uploaded this repo to xAI GCS"
  scope           : {repos?: [str], files?: int, bytes?: int}
  evidence        : {path: str, line?: int, snippet: str}   # reproducible pointer
  remediation     : str | None     # suggested, never applied
  status          : "new" | "acknowledged" | "resolved"
```

Storage is the maintainer's call (a new `egress_findings` mart table, or reuse of the patterns/report surface) — the spec proposes the shape, not the migration. Dedupe key: `(provider, signature_id, session_id?, scope-hash)`.

---

## 6. The signature catalog + detector interface (portable artifact #2)

Keep signatures **declarative**, following `adapters/capabilities.json`, so the catalog is data the revamp carries forward verbatim:

```jsonc
// egress_signatures.json  (illustrative — Grok fully worked, others stubbed)
{
  "grok": {
    "config": {
      "artifact": "~/.grok/config.toml",
      "at_risk_when": "telemetry.trace_upload != false OR telemetry.disable_codebase_upload != true",
      "remediation": "[features] telemetry=false\n[telemetry] trace_upload=false\ndisable_codebase_upload=true"
    },
    "event": {
      "artifact": "~/.grok/logs/unified.jsonl",
      "match": [{"msg": "repo_state.upload.start", "posture": "occurred", "severity": "high"},
                {"msg": "trace.upload.decision", "when": "ctx.uploads_enabled == true", "posture": "at_risk"}]
    }
  },
  "claude":  { "transcript_only": true },
  "cursor":  { "config": { "artifact": "TODO", "at_risk_when": "TODO" } }
}
```

The engine is a small registry mirroring the adapter registry:

```python
class EgressDetector(Protocol):
    provider: str
    def scan(self, ctx: ScanContext) -> Iterable[EgressFinding]: ...
```

Three built-ins — `ConfigDetector`, `EventDetector`, `TranscriptDetector` — each interpret the catalog. New providers = catalog entries, not code. A future `WireDetector` ingests a proxy/pcap log into the same schema.

---

## 7. Surfacing (advisory only)

- **CLI (primary, local, read-only):** `stackunderflow egress [--provider grok] [--json]`. Lists findings; `--json` returns the `stackunderflow.memory/1`-style bounded envelope. Fits the CLI-first ethos.
- **SessionStart hook (D1, preventive):** inject `additionalContext` — *"⚠️ Grok is configured to upload this repo (no local veto). Fix: `disable_codebase_upload=true`."* Reuses `hooks/inject.py`. **Recommend default-on for `critical`/`high` egress findings** — a departure from the default-off nudge convention, justified because this is a security signal, but the maintainer's call.
- **Dashboard panel ("Data-out"):** a route + tab reusing the patterns/CodingHealth surface; the retrospective review/acknowledge home.

**Invariant (from Spec 27):** surfacing never returns `permissionDecision: deny` and never auto-remediates. It informs; the human acts.

---

## 8. Limits — stated plainly

1. **Detection, not prevention.** This raises the alarm and preserves forensics; it cannot stop a byte from leaving. Prevention lives elsewhere (config veto, firewall, network policy). Do not let the dashboard imply otherwise.
2. **Self-reported sources.** D1/D2 read the vendor's own config and logs. A vendor determined to hide an upload simply wouldn't log it — so a clean D1/D2 is *"nothing self-reported,"* not *"nothing happened."* Only the (out-of-scope) `wire` detector is trustless. Say this in the UI.
3. **Per-provider effort, no universal schema.** Every agent's config and ops-log differ; signatures are bespoke and need maintenance. Missing/renamed signature ⇒ `unknown`, never `safe`.
4. **D3 false positives.** Legitimate `curl`/`rsync`/`git push` is common. Severity tiers + a per-repo known-remote allowlist are mandatory, not optional.

### Threat-model coverage
| Threat | D1 | D2 | D3 | Wire (future) |
|---|:--:|:--:|:--:|:--:|
| Vendor client silently uploads repo (Grok) | ⚠ posture | ✅ | ✗ | ✅ |
| Agent runs `curl`/`scp` to exfil | ✗ | ✗ | ✅ | ✅ |
| Config set to train on your code | ✅ | ✗ | ✗ | n/a |
| Vendor uploads *and* omits the log | ✗ | ✗ | ✗ | ✅ |

---

## 9. Phasing

- **Phase 1 (MVP, highest ROI):** D1 + D2 for **Grok only**, surfaced via `stackunderflow egress --json` + SessionStart hook. One signature, rides `etl/watcher.py` and the CLI. This alone would have caught the incident.
- **Phase 2:** D3 transcript analyzer (all providers — transcripts already ingested) + the dashboard panel.
- **Phase 3:** signature catalog for the top-N agents; optional `wire` detector (ingest a local proxy log) for a trustless tier.

## 10. Open questions (maintainer)
- Finding store: new `egress_findings` mart vs. reuse of the report/patterns surface?
- Default-on SessionStart for high-severity findings — accept the break from default-off nudges?
- D3 allowlist model — per-repo config, global settings, or learned-from-history known remotes?
- How to present D1 "posture" honestly when the effective state is remote-driven and only D2 can confirm it?

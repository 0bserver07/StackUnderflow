# StackUnderflow — UI / Perf / Data Audit (2026-06-25)

> Working doc — **untracked on purpose** (like HANDOFF-cost-audit.md). Don't `git add -A`.
> Source: workflow `wf_ed3df9d5-3c8`, 147 agents, ~2h, 7.9M tokens. 121 findings → 107 verified → **63 confirmed** (deduped). Raw JSON was transient in `/tmp`.

Ranked by user impact: perf (slowness) + data (wrong numbers) first, then broken, then ux. All paths relative to repo root.

## WAVE 1 — in flight (6 worktree agents, backend latency + worst data bug)
| agent | owns | findings |
|---|---|---|
| fix-be-overview | queries.py, projects.py | #1 global-stats 3 full scans (~11s)→marts · #16 async-blocking route · #22 no currency · #26 mart Commands=0 · #37 cost tie-out |
| fix-be-data | routes/data.py | #2 mart fast-path skipped for multi-provider (3.1s) · #7 mart stubs show 0/empty · #46 Hourly chart renders nothing (`[]` vs `{}`) |
| fix-be-live | services/live.py, routes/live.py | #3 latency LEAD-scans 333K rows (~4.8s) · #28 rolling_burn full-scan/5s + conn reopen · #35 UTC vs local-tz day buckets |
| fix-be-sessions | routes/sessions.py | #29 /api/jsonl-files N+1 (~3.7K queries) · #17 /api/sessions/compare runs whole pipeline (~3.4s) |
| fix-be-cost | routes/cost.py, commands.py | #19 'Spend by agent' leaks GLOBAL spend (4-25x) · #41 2 currency fields unconverted · #11 cost/tool routes recompute pipeline (1.4-4s) |
| fix-fe-ratebug | charts/ErrorRateChart, InterruptionRateChart | #6 error/interruption rate ×100 too high (2.7%→270%) |

## WAVE 2 — frontend jank (planned; heavy file-overlap, group by file-ownership)
- #5 zero code-splitting — 1.86MB chunk before first paint (vite.config.ts + ProjectDashboard lazy)
- #4 Sessions tab renders ~1,820 cards unvirtualized (SessionsTab.tsx) [+#30 search no-debounce]
- #8 every chart unmemoized → ~14 SVGs re-render on any state change (OverviewTab + charts/*) — "the janky half"
- #9 filter chip blanks whole dashboard to spinner + remounts (ProjectDashboard.tsx) [+#14 2-RTT waterfall]
- #10 Overview/Cost bypass React Query for heavy /api/cost-data (CostTab, OverviewTab) [+#44 Header getProjects ×3]
- #12,#13 Overview fetches 276-project payload, client-side sort + fresh Date() every render (Overview.tsx)
- #15 Markdown bundles full Prism (343 grammars), re-tokenizes on scroll
- #42 Live event stream re-sorts 200 items per SSE row · #49 single root ErrorBoundary blanks whole app

## WAVE 3 — viz data + ux correctness (the remaining 30-odd)
data: #18 dup tool chart · #20 ToolCost cache=0 · #21 DailyCost not currency-converted · #23 TokenStack 7d/30d=last-N-rows · #24 ToolCost ignores date filter · #25 Commands KPI lifetime vs windowed · #32 donut all-time · #33 CommandToolDist ignores filter · #34 Hourly drops cache · #36 Messages search only current page · #38,#39 Overview table/headline window mismatch · #40 CacheRoi magic constants · #45 isErrorMessage substring · #57 model filter scopes only cost
broken/ux: #47 beta TABS inert + drift · #48 CacheRoi top-savers dead code · #50 Overview no loading/error state · #51 currency 'Other' dead-end · #52 nav wipes filters from URL · #53 dup Error Categories panels · #54 TokenUsage unreadable · #55 charts return null (grid reflow) · #56 ModelDist 6 colors · #58 trend axis · #59 dark-theme colors in light mode · #60 local formatters · #61,#62,#63 ToolCost/SessionCost/CacheRoi polish · #27 plan memo · #31 currency flush cache · #43 etl status unindexed

## FOLLOW-UPS surfaced during the fix campaign (NEW, beyond the original 63)
- **ETL mart materialization** (from #2/#7, commit `65787ca`). The mart fast-path now serves tools+cache+overview+daily+models, but `message_types` (User/Assistant/Tool counts), `user_interactions` (Commands, Tools/Cmd, Interruption Rate, Steps/Cmd), `errors.by_category`/`rate`, and `cache.hit_rate` still read **0** on the mart path — no mart carries them (`usage_events` is assistant-only; interactions are interaction-grain; no error mart). Fall-through to `get_project_stats` reintroduces the ~3.1s scan the <100ms perf test forbids. FIX = a mart-materialization track (classifier/interaction counts → session_mart or a new mart). This is the remaining half of the "Overview shows 0" complaint (#7/#26).
- **Frontend first-paint** (from #5, commit `7d158a7`). Entry chunk is 145KB but **recharts (444KB**, Overview home route) + **markdown/syntax-highlighter (~250KB**, always-rendered MetaAgentSidebar→Markdown) stay EAGER, so first-paint only drops ~20%. FIX = lazy-load routes in `App.tsx` + lazy `<Markdown>` in the sidebar/MetaAgentMessageList.

## Integration
Each WAVE-1 agent owns disjoint files → diffs don't conflict; I review + commit per-agent (tests must be green) like Grok (e2b6798) / messages-pagination (2146af1). Stale already-fixed: the 2 "/api/messages re-aggregates 36K" findings (killed by 2146af1).

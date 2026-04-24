# Analytics Fixes — Follow-Up Wave

Follow-up pass on `feat/analytics-expansion`. Fixes user-reported gaps in the polished Cost tab build.

Base: `feat/analytics-expansion`. Every agent branches from the current HEAD of that branch.

**Do NOT modify anything outside your owned files.** File conflicts are the biggest risk — respect ownership strictly.

---

## Wave D — fixes (7 owners, parallel)

### D1 `backend-paginate` — paginate user_interactions
**Files:** `stackunderflow/routes/data.py`, maybe `stackunderflow/stats/aggregator.py`, test files.
**Problem:** `/api/dashboard-data` payload is 2.37 MB on chimera; 1.81 MB is `user_interactions` (the `_command_analysis` output). Dashboard load is slow because the React app has to parse that whole blob even when the Commands tab isn't open.
**Scope:**
- Split `user_interactions` out of `/api/dashboard-data`. Leave only a small summary (counts, averages) behind — enough for Overview stat cards.
- New endpoint `GET /api/commands?log_path=&offset=&limit=&sort=` returning a paginated slice of the full command list. Default limit 50, max 500. Supports `sort=cost|tokens|tools|steps|time` and `order=desc|asc`.
- If `tool_count_distribution` can stay in the summary (it's small), keep it.
- Commands tab fetches via the new endpoint with infinite-scroll or explicit pagination controls — that's for the Wave E theme agents to style; you just ensure the endpoint exists and tests pass.
- Add tests.

**Done when:** `/api/dashboard-data` on chimera < 1 MB; `/api/commands?...&limit=50` returns 200 with a valid paginated response; existing tests pass.

### D2 `nav-consol` — consolidate navigation
**Files:** `stackunderflow-ui/src/components/cost/CommandCostList.tsx`, `OutlierCommandsTable.tsx`, `SessionCostBarChart.tsx`, `SessionEfficiencyTable.tsx`, `RetryAlertsPanel.tsx`.
**Problem:** 5 components each have inline `window.history.pushState` calls that duplicate the `services/navigation.ts` helper. Behavior diverges (some emit `NAV_EVENT`, some don't).
**Scope:**
- In each of the 5 files above, remove the local `pushState`/stub and replace with `openInteraction`, `openSession`, or `setTab` imports from `../../services/navigation`.
- Delete the `TODO(merge)` / TODO-stub comments that were left by Wave-A/B agents.
- Preserve existing props (`onOpen?`, `onSelect?`) — callers take precedence, nav service is the fallback.
- Verify no other component outside these 5 has a direct `pushState` call.

**Done when:** `grep -rn "window.history.pushState" stackunderflow-ui/src/components/` returns only the navigation service location (if any), not the 5 component files.

### D3 `breadcrumb` — in-UI breadcrumb + back button
**Files:** NEW `stackunderflow-ui/src/components/common/Breadcrumb.tsx`, `stackunderflow-ui/src/pages/ProjectDashboard.tsx`.
**Problem:** No in-UI indication of how the user got to a detail view; no way to go back short of browser back.
**Scope:**
- New `<Breadcrumb />` component. Shows the current tab + any active deep-link params (e.g., "Cost · Command detail"). First segment clickable back to the root tab, last segment plain text.
- Accepts a prop `trail: Array<{label: string, onClick?: () => void, href?: string}>`.
- "Back" button component `<BackButton />` at the same module: calls `window.history.back()` on click. Shows IconArrowLeft. Keyboard accessible.
- Wire both into `ProjectDashboard.tsx` — render above the tab content area when a deep-link param (`?session=`, `?interaction=`) is active. Otherwise show just the tab name.
- The breadcrumb itself doesn't manipulate history; clicking a segment just calls the onClick (which may call setTab or clearParam).

**Done when:** Opening a session deep-link shows a breadcrumb at top. Clicking "Sessions" in the breadcrumb clears the session param and returns to the bare tab.

### D4 `url-state` — URL-encoded filter / sort state
**File:** `stackunderflow-ui/src/components/dashboard/CostTab.tsx`.
**Problem:** Going forward/back loses filter state; refresh resets filters.
**Scope:**
- Read initial filter state (`range`, `sessionFilter`, `toolFilter`) from URL params on mount (`getParam('range')`, etc.).
- When filters change, update URL via `setTab('cost', {range, session, tool})` from `../../services/navigation`. Use `history.replaceState` (not push) so back button doesn't trap users in filter micro-states.
- Sort state: if you want to URL-encode it too, use `?sort=cost-desc` pattern — optional; skip if complicates.
- Maintain backwards compat: if no URL params, behavior is identical to now.

**Done when:** Applying a filter updates URL; reloading with that URL restores the filter; browser back undoes a big navigation (tab switch) but does NOT undo each filter change.

### D5 `contrast-fix` — dark-on-dark audit
**Files:** any `.tsx` files in `stackunderflow-ui/src/components/` that contain `text-gray-600`, `text-gray-700`, or other low-contrast pairings.
**Scope:**
- Scan all `.tsx` in the components tree for text-gray-600/700/800/900 classes rendered on dark backgrounds (`bg-gray-800/*`, `bg-gray-900/*`).
- For each occurrence, bump the text to `text-gray-400` or `text-gray-300` (still subtle but WCAG-AA readable).
- Don't change colors that are intentional muting (e.g., disabled chips). Use judgment.
- Do NOT introduce new color tokens. Stick to Tailwind's existing palette.
- Run `grep -rn "text-gray-6\|text-gray-7\|text-gray-8" stackunderflow-ui/src/components/ | grep -v node_modules` and fix each hit that's problematic.

**Done when:** No `text-gray-600` or darker used as body/label text on dark backgrounds. Low-severity hints, borders, and placeholder text may keep `text-gray-500`.

**Constraint:** you will touch many files. Commit with a single commit. Coordinate: Wave E agents might be starting on theme work — they will be reading your contrast fixes first.

### D6 `theme-foundation` — light/dark toggle infrastructure
**Files:** `stackunderflow-ui/tailwind.config.js`, NEW `stackunderflow-ui/src/hooks/useTheme.ts`, NEW `stackunderflow-ui/src/components/common/ThemeToggle.tsx`, edit `stackunderflow-ui/src/App.tsx` or `main.tsx` to wire ThemeProvider-equivalent at root.
**Scope:**
- Enable `darkMode: 'class'` in `tailwind.config.js`.
- `useTheme()` hook: exports `{theme: 'dark'|'light', toggle: () => void, setTheme: (t) => void}`. Persist to `localStorage`. On mount, read from localStorage; default to `'dark'` (current behavior). Apply/remove `'dark'` class on `document.documentElement`.
- `<ThemeToggle />` component: renders a sun/moon icon button (`IconSun` / `IconMoon` from `@tabler/icons-react`), calls `useTheme().toggle()` on click.
- Wire `useTheme` initializer into `App.tsx` (or whatever mounts the app) so the `dark` class is on `<html>` before first paint.
- Do NOT modify any component colors yet — Wave E agents do that. Your job is only the infrastructure.

**Done when:** `useTheme` works, ThemeToggle component renders correctly, the `dark` class toggles on `<html>` when clicked. Page still looks identical (colors not yet theme-aware) — that's expected.

### D7 `header-toggle` — place ThemeToggle in the header
**File:** `stackunderflow-ui/src/components/layout/Header.tsx`.
**Scope:** import `<ThemeToggle />` from D6, render it in the header alongside existing controls. Trivial, but kept isolated to avoid conflicts.
**Done when:** toggle button visible in the app header, clicking it flips `dark` class on `<html>` (verifiable in dev tools).

---

## Wave E — theme application (5 owners, after D merges)

**Shared rule:** every hard-coded color class gets a `dark:` prefix AND a light-mode counterpart. Examples:
- `bg-gray-800` → `bg-white dark:bg-gray-800`
- `text-gray-300` → `text-gray-700 dark:text-gray-300`
- `border-gray-800` → `border-gray-200 dark:border-gray-800`
- `bg-gray-800/50` → `bg-gray-100/50 dark:bg-gray-800/50`

Common pairings (dark → light equivalent):
| Dark  | Light |
|---|---|
| bg-gray-900 | bg-gray-50 |
| bg-gray-800 | bg-white |
| bg-gray-800/50 | bg-gray-100/70 |
| bg-gray-800/30 | bg-gray-100/50 |
| text-gray-100 | text-gray-900 |
| text-gray-200 | text-gray-800 |
| text-gray-300 | text-gray-700 |
| text-gray-400 | text-gray-600 |
| text-gray-500 | text-gray-500 (keep) |
| border-gray-800 | border-gray-200 |
| border-gray-800/50 | border-gray-200/50 |

Accent colors (indigo, emerald, red, amber, pink, cyan) stay the same in both modes — they work.

### E1 `theme-cost` — cost/ components
**Files:** every `.tsx` under `stackunderflow-ui/src/components/cost/` (11 files).
**Scope:** apply `dark:/` prefixes per rules above, in every file. No functional changes.

### E2 `theme-dashboard` — dashboard/ tabs
**Files:** every `.tsx` under `stackunderflow-ui/src/components/dashboard/`.
**Scope:** same. Includes `CostTab.tsx` (that already gets a prop change in D4 — merge order: D4 first, then E2).

### E3 `theme-common-charts-analytics` — supporting components
**Files:** every `.tsx` under `stackunderflow-ui/src/components/common/`, `components/charts/`, and `components/analytics/`.
**Scope:** same.

### E4 `theme-pages-layout` — pages + layout + app shell
**Files:** `stackunderflow-ui/src/pages/*.tsx`, `stackunderflow-ui/src/components/layout/*.tsx`, `stackunderflow-ui/src/App.tsx`, `stackunderflow-ui/src/index.css`.
**Scope:** same. Make sure `index.css` body/root styles work in both modes. Tailwind will do most of the work, but any raw CSS needs `@media (prefers-color-scheme: light)` or `html.light { ... }` equivalent.

### E5 `theme-qa` — smoke test + regression fix
**Files:** any file, surgical only.
**Scope:**
- Start the dev server or run the production build and open the Cost tab in both modes.
- Click through every chart, table, modal, dropdown in light mode. Flag any contrast failures (light-on-light, illegible).
- Commit targeted fixes for each regression.
- Write a brief commit-message report: what was tested, what was broken, what was fixed.

**Done when:** both modes are visually coherent — no illegible text, no invisible borders, no broken icons.

---

## Merge order
1. Wave D: merge in this order to minimize conflicts: D1, D2, D3, D4, D6, D7, then D5 (last — touches the most files).
2. Wave E: merge in any order (each owns a disjoint file set), then E5 last.

Every agent:
- Branch from `feat/analytics-expansion` HEAD
- Commit on their worktree branch
- Do NOT push
- Message team-lead when done
- Mark their task completed via TaskUpdate

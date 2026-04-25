# Beta Features Toggle

Opt-in visibility for heuristic / experimental dashboard tabs.

Base: `feat/beta-features`, branched from `v0.3.1` (main @ `14e4d42`).

## Design

**Default behavior after upgrade:** existing users lose nothing — all tabs still show unless they've manually hidden them.

**What's marked beta (heuristic-based, per `CHANGELOG.md` 0.3.0 notes):**
- `qa` — Q&A extraction (pattern-matching, not NLP)
- `tags` — Auto-tagging (pattern-matching)

Everything else stays stable.

**Two layers of control:**

1. **Global beta toggle** (`suf:beta` in localStorage). Default: `false` (new users don't see beta tabs). Flipping to `true` reveals all beta tabs.
2. **Per-tab override** (`suf:tabs` in localStorage, JSON object like `{"qa": "shown", "tags": "hidden"}`). Overrides the global flag for a specific tab. Lets users keep Q&A visible even with the beta flag off, or hide Cost even though it's stable.

The **first-run default for EXISTING users** is `suf:beta = true` so we don't silently hide tabs they were using. Implementation: if `suf:beta` key is not set when the hook first runs, read any existing bookmark/tag data from localStorage OR prior usage signals to decide; if none, default to `true` and write `suf:beta=true` (so the user has full visibility). We'll treat missing-key as "migrated user, keep everything visible" — new installs get `false` only after a future major version.

*Simplification for v0.3.2:* default to `true` always. New users can go to Settings and hide beta tabs if they want. This avoids the awkward "you upgraded and things vanished" UX.

## API

### `stackunderflow-ui/src/hooks/useBetaFeatures.ts` (new)

```ts
export const BETA_ENABLED_KEY = 'suf:beta'
export const TAB_VISIBILITY_KEY = 'suf:tabs'

export type TabVisibility = 'shown' | 'hidden' | 'default'

export interface BetaFeaturesState {
  betaEnabled: boolean
  tabOverrides: Record<string, TabVisibility>
  setBetaEnabled: (v: boolean) => void
  setTabVisibility: (tabId: string, v: TabVisibility) => void
  isTabVisible: (tabId: string, isBeta: boolean) => boolean
  reset: () => void
}

export function useBetaFeatures(): BetaFeaturesState
```

`isTabVisible(id, isBeta)`:
- If override is `'shown'` → true
- If override is `'hidden'` → false
- If no override and `isBeta === false` → true (stable tabs always visible)
- If no override and `isBeta === true` → return `betaEnabled`

Defaults:
- `BETA_ENABLED_KEY` missing → `true` (don't hide on upgrade)
- `TAB_VISIBILITY_KEY` missing → `{}`

### `stackunderflow-ui/src/components/common/BetaBadge.tsx` (new)

Small inline "BETA" pill. Props: `{className?: string}`. Tailwind styling: `bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300 text-[10px] px-1.5 py-0.5 rounded uppercase tracking-wider font-semibold`.

### `stackunderflow-ui/src/pages/Settings.tsx` (new)

Page at `/settings`. Sections:

1. **Appearance**
   - Theme toggle (sun/moon + label, uses `useTheme`)

2. **Beta features**
   - Description: "Heuristic features that may not be fully reliable yet. Enabling this reveals all beta tabs on project dashboards."
   - Toggle: "Show beta features" (controls `setBetaEnabled`)

3. **Tab visibility**
   - List of every tab with checkboxes or dropdown (Shown / Hidden / Default). Explain that Default follows the beta toggle for beta tabs, and always shows for stable ones.
   - Each row shows a BetaBadge if the tab is beta.

4. **Reset**
   - Button: "Reset all settings to defaults" → `reset()` call + page reload.

Layout: full-width in the existing app shell. Use same page styling as `pages/Overview.tsx`. Back button at top linking to "/".

### `stackunderflow-ui/src/pages/ProjectDashboard.tsx` (edit)

- Add `beta?: boolean` to the TABS array entries:
  - `{ id: 'qa', ..., beta: true }`
  - `{ id: 'tags', ..., beta: true }`
- Use `useBetaFeatures().isTabVisible(tab.id, tab.beta ?? false)` to filter TABS before rendering the bar.
- Render `<BetaBadge>` inline with the label of beta tabs in the tab bar.
- If the currently-active tab gets hidden (e.g., user toggles off), fall back to `overview`.

### `stackunderflow-ui/src/App.tsx` (edit)

- Add import: `import Settings from './pages/Settings'`
- Add route: `<Route path="/settings" element={<Settings />} />`

### `stackunderflow-ui/src/components/layout/Header.tsx` (edit)

- Add a settings gear icon linking to `/settings`. Position: right side, before the theme toggle.
- Use `IconSettings` from `@tabler/icons-react`.

## Task split — 4 agents

### G1 `beta-foundation` (parallel, new files only)
- `hooks/useBetaFeatures.ts`
- `components/common/BetaBadge.tsx`

### G2 `beta-settings` (parallel)
- `pages/Settings.tsx` (new)
- `components/layout/Header.tsx` (add gear Link → `/settings`)
- Optimistically imports `useBetaFeatures` from G1's path; if missing at commit, inline a stub with TODO.

### G3 `beta-dashboard` (parallel)
- `pages/ProjectDashboard.tsx` (beta flag on tabs, filter, badge render, active-tab fallback)
- `App.tsx` (new `/settings` route)
- Optimistically imports `useBetaFeatures` + `BetaBadge` from G1's paths.

### G4 `beta-qa` (sequential, after G1-G3 merge)
- Smoke test: settings page loads, toggle hides/shows tabs, per-tab overrides work, reload persists, no regressions in existing tabs, theme toggle still works.
- Update `CHANGELOG.md` `[Unreleased]` section with the feature.
- Fix any regressions surgically.

## Acceptance
- `npm run typecheck` + `npm run build` pass after each merge.
- `pytest tests/` stays at 420 passing.
- Manual test (via final-qa agent): toggle off → Q&A and Tags disappear. Toggle on → they reappear. Per-tab "hidden" override hides a stable tab.
- `localStorage['suf:beta']` + `localStorage['suf:tabs']` are the only new keys written.

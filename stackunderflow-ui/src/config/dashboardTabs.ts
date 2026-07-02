// Canonical dashboard tab catalogue — the SINGLE source of truth for tab ids,
// labels, order, icons, and beta flags.
//
// Consumed by:
//   - pages/ProjectDashboard.tsx — renders the tab bar + gates visibility
//   - pages/Settings.tsx         — the "Tab visibility" list
//
// Kept in its own module (rather than exported from ProjectDashboard) so the
// two route bundles can stay independently code-split: Settings importing this
// must NOT drag in the whole ProjectDashboard chunk. `isBeta` is exactly what
// the beta-features toggle gates.

import {
  IconLayoutDashboard,
  IconFolders,
  IconCurrencyDollar,
  IconTerminal2,
  IconMessageCircle,
  IconSearch,
  IconHelpCircle,
  IconBookmark,
  IconTag,
  IconScale,
  IconGitBranch,
  IconGitFork,
  IconHierarchy3,
  IconHistory,
  IconWallet,
  IconActivityHeartbeat,
  IconBinaryTree2,
} from '@tabler/icons-react'

export type Tab = {
  id: string
  label: string
  icon: typeof IconLayoutDashboard
  isBeta?: boolean
}

export const TABS: readonly Tab[] = [
  { id: 'overview', label: 'Overview', icon: IconLayoutDashboard },
  { id: 'sessions', label: 'Sessions', icon: IconFolders },
  // Agents + Playback are heuristic, still-maturing views — flagged beta so
  // the "Show beta features" toggle (Settings) actually gates them. Without
  // `isBeta` the toggle was a no-op for these two tabs.
  { id: 'agents', label: 'Agents', icon: IconHierarchy3, isBeta: true },
  // Playback slots between Sessions and Cost (same band as Agents). Both
  // tabs handle the empty-data case via their own EmptyState components —
  // see PlaybackTab.tsx and AgentsTab.tsx.
  { id: 'playback', label: 'Playback', icon: IconHistory, isBeta: true },
  { id: 'cost', label: 'Cost', icon: IconCurrencyDollar },
  // Cost-intelligence tab (audit #7p2) — spend budgets + cross-provider
  // what-if repricing. Slots right after Cost since it consumes the same
  // /api/budgets + /api/whatif cost surfaces. Beta while the candidate set
  // and projection heuristics settle.
  { id: 'budgets', label: 'Budgets', icon: IconWallet, isBeta: true },
  // v0.6.0 follow-up tabs — per spec brief, Compare/Yield slot between Cost
  // and Commands. Both call dedicated /api/compare and /api/yield routes.
  { id: 'compare', label: 'Compare', icon: IconScale },
  { id: 'yield', label: 'Yield', icon: IconGitBranch, isBeta: true },
  // Fork / sidechain economics — prices the conversation DAG that
  // `is_sidechain` + `parent_uuid` already capture (subagent spend + branches
  // started then dropped). Calls the dedicated /api/forks route. Beta while the
  // abandonment heuristic settles.
  { id: 'forks', label: 'Forks', icon: IconGitFork, isBeta: true },
  // Cross-session pattern / failure mining (campaign #6) — recurring file
  // failures, error signatures + resolution hints, command failure clusters.
  // Calls the dedicated /api/patterns route. Beta while attribution settles.
  { id: 'health', label: 'Health', icon: IconActivityHeartbeat, isBeta: true },
  // Worktree intelligence (campaign #8) — live read-only git scan of the
  // project's worktrees: prune verdicts, attributed sessions + cost, and
  // copyable prune-command PREVIEWS (the tool never runs them). Calls the
  // dedicated /api/worktrees route. Beta while the verdict heuristics settle.
  { id: 'worktrees', label: 'Worktrees', icon: IconBinaryTree2, isBeta: true },
  { id: 'commands', label: 'Commands', icon: IconTerminal2 },
  { id: 'messages', label: 'Messages', icon: IconMessageCircle },
  { id: 'search', label: 'Search', icon: IconSearch },
  { id: 'qa', label: 'Q&A', icon: IconHelpCircle, isBeta: true },
  { id: 'bookmarks', label: 'Bookmarks', icon: IconBookmark },
  { id: 'tags', label: 'Tags', icon: IconTag, isBeta: true },
] as const

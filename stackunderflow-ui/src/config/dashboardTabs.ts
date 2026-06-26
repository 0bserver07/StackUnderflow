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
  IconHierarchy3,
  IconHistory,
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
  { id: 'agents', label: 'Agents', icon: IconHierarchy3 },
  // Playback slots between Sessions and Cost (same band as Agents). Both
  // tabs handle the empty-data case via their own EmptyState components —
  // see PlaybackTab.tsx and AgentsTab.tsx.
  { id: 'playback', label: 'Playback', icon: IconHistory },
  { id: 'cost', label: 'Cost', icon: IconCurrencyDollar },
  // v0.6.0 follow-up tabs — per spec brief, Compare/Yield slot between Cost
  // and Commands. Both call dedicated /api/compare and /api/yield routes.
  { id: 'compare', label: 'Compare', icon: IconScale },
  { id: 'yield', label: 'Yield', icon: IconGitBranch, isBeta: true },
  { id: 'commands', label: 'Commands', icon: IconTerminal2 },
  { id: 'messages', label: 'Messages', icon: IconMessageCircle },
  { id: 'search', label: 'Search', icon: IconSearch },
  { id: 'qa', label: 'Q&A', icon: IconHelpCircle, isBeta: true },
  { id: 'bookmarks', label: 'Bookmarks', icon: IconBookmark },
  { id: 'tags', label: 'Tags', icon: IconTag, isBeta: true },
] as const

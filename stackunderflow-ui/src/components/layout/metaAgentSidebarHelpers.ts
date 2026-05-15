// Pure helpers extracted from ``MetaAgentSidebar.tsx`` so they can be
// tested under Node's TypeScript loader (which doesn't process .tsx).
// The .tsx component imports + re-exports the names that callers use.

export const META_AGENT_SIDEBAR_STORAGE_KEY = 'stackunderflow_metaAgentSidebar'

export type SidebarState = 'collapsed' | 'expanded' | 'hidden'

export function _readPersisted(): SidebarState | null {
  try {
    const raw = localStorage.getItem(META_AGENT_SIDEBAR_STORAGE_KEY)
    if (raw === 'collapsed' || raw === 'expanded' || raw === 'hidden') return raw
  } catch {
    // ignore
  }
  return null
}

export function _writePersisted(value: SidebarState) {
  try {
    localStorage.setItem(META_AGENT_SIDEBAR_STORAGE_KEY, value)
  } catch {
    // ignore
  }
}

// Default state — viewport-aware. ``>= 1280px`` opens expanded;
// ``>= 768px`` defaults to collapsed (icon rail); below that we hide.
export function _resolveInitialState(
  persisted: SidebarState | null,
  viewportWidth: number,
): SidebarState {
  // Hidden breakpoint always wins on small screens; the user can still
  // open the overlay from the header.
  if (viewportWidth < 768) return 'hidden'
  if (persisted) return persisted
  if (viewportWidth >= 1280) return 'expanded'
  return 'collapsed'
}

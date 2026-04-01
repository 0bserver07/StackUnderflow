/**
 * Project name display modes:
 *  - name: short project name (last meaningful path segment)
 *  - path: full slug converted to path
 *  - anon: "Project 1", "Project 2", etc.
 */

export type NameMode = 'name' | 'path' | 'anon'

const STORAGE_KEY = 'su-name-mode'

export function getNameMode(): NameMode {
  const stored = localStorage.getItem(STORAGE_KEY)
  if (stored === 'name' || stored === 'path' || stored === 'anon') return stored
  return 'name'
}

export function setNameMode(mode: NameMode): void {
  localStorage.setItem(STORAGE_KEY, mode)
  window.dispatchEvent(new Event('namemode-changed'))
}

export function formatProjectName(slug: string, index?: number, mode?: NameMode): string {
  const m = mode ?? getNameMode()

  if (m === 'anon') return `Project ${(index ?? 0) + 1}`

  if (m === 'path') {
    // convert slug to readable path
    if (slug.startsWith('-')) {
      return '/' + slug.slice(1).replace(/-/g, '/')
    }
    return slug.replace(/-/g, '/')
  }

  // 'name' mode — extract the project name from the slug
  const raw = slug.startsWith('-') ? slug.slice(1) : slug
  const parts = raw.split('-')

  // find the last path-like segment (year/month/dev etc) and take everything after
  let cutoff = -1
  for (let i = parts.length - 1; i >= 0; i--) {
    if (/^(year|dev|jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)\d{0,2}$/i.test(parts[i] ?? '')) {
      cutoff = i
      break
    }
  }

  if (cutoff >= 0 && cutoff < parts.length - 1) {
    return parts.slice(cutoff + 1).join('-')
  }

  // fallback: last 2 parts
  return parts.slice(-2).join('-')
}

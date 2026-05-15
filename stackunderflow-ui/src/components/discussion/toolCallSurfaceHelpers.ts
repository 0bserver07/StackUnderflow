// Pure helpers extracted from ``ToolCallSurface.tsx`` so they can be
// unit-tested under Node's TypeScript loader (which doesn't process
// .tsx files). The .tsx component re-exports these symbols so other
// imports keep working.

import type { MetaAgentToolInvocation } from '../../types/metaAgent'

// Hard cap on the textual length of the rendered JSON. Aligns with the
// backend's 4 KB result cap so the surface never paints multiple
// screens of text even if the truncator failed to trim something.
export const _MAX_JSON_CHARS = 4096

export function _formatJson(value: unknown): string {
  try {
    const text = JSON.stringify(value, null, 2)
    if (text.length <= _MAX_JSON_CHARS) return text
    return text.slice(0, _MAX_JSON_CHARS) + '\n... [truncated for display]'
  } catch {
    return String(value)
  }
}

export function buildToolStatusLabel(invocation: MetaAgentToolInvocation): string {
  if (!invocation.result) return 'running…'
  if (invocation.result.ok) return `ok · ${invocation.result.duration_ms}ms`
  return `error · ${invocation.result.duration_ms}ms`
}

export function buildToolSummary(invocation: MetaAgentToolInvocation): string {
  if (!invocation.result) return ''
  const data = invocation.result.data
  if (!data || typeof data !== 'object') return ''
  // Surface a tiny one-line summary. Discovery tools all surface a
  // ``count`` field; the cost tool surfaces ``total_cost_usd``; the
  // project-summary tool surfaces ``sessions`` and ``cost_usd``. Pick
  // whatever's there.
  const d = data as Record<string, unknown>
  if (typeof d.count === 'number') return `${d.count} matches`
  if (typeof d.file_count === 'number') return `${d.file_count} files touched`
  if (typeof d.total_cost_usd === 'number') {
    return `$${(d.total_cost_usd as number).toFixed(2)} total`
  }
  if (typeof d.sessions === 'number' && typeof d.cost_usd === 'number') {
    return `${d.sessions} sessions · $${(d.cost_usd as number).toFixed(2)}`
  }
  if (typeof d.error === 'string') return d.error
  return ''
}

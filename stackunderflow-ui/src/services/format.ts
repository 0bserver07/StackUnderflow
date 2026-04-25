/**
 * Single source of truth for currency / number / token formatting in the UI.
 *
 * Prior to this module, `formatCost` was duplicated 11 times across cost/,
 * dashboard/, analytics/, and pages/ — most copies were missing the
 * thousands-separator branch (so `$5,421` rendered as `$5421`) and a couple
 * were stuck on `toFixed(4)` always (so $5,421.03 rendered as `$5421.0345`).
 *
 * Use these everywhere a number meets a UI surface.
 */

/**
 * Format a USD amount.
 *
 * Policy:
 * - exactly 0       → `$0`
 * - 0 < |x| < 0.01  → 4-decimal precision so sub-cent values stay visible
 * - 0.01 ≤ |x| < 1k → 2 decimals
 * - |x| ≥ 1000      → 2 decimals with locale thousands separators
 *
 * Negative values get a leading `-` (preserved through all branches).
 */
export function formatCost(cost: number): string {
  if (!Number.isFinite(cost)) return '$0'
  if (cost === 0) return '$0'
  const sign = cost < 0 ? '-' : ''
  const abs = Math.abs(cost)
  if (abs < 0.01) return `${sign}$${abs.toFixed(4)}`
  if (abs >= 1000) {
    return `${sign}$${abs.toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    })}`
  }
  return `${sign}$${abs.toFixed(2)}`
}

/**
 * Format an arbitrary count with k/M/B suffixes. Useful for token totals,
 * message counts, and any large-magnitude integer.
 */
export function formatNumber(n: number): string {
  if (!Number.isFinite(n)) return '0'
  const abs = Math.abs(n)
  if (abs >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`
  if (abs >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (abs >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return n.toLocaleString()
}

/**
 * Token-specific shortcut. Identical policy to `formatNumber` today; kept as
 * its own export so future per-domain tweaks (e.g. dropping decimals on whole
 * thousands) only need to touch one site.
 */
export function formatTokens(n: number): string {
  return formatNumber(n)
}

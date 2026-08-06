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

import type { CurrencyInfo } from '../types/api'

/**
 * Format a monetary amount.
 *
 * The amount is always passed in the *display* currency — backend routes
 * pre-convert the USD figure when ``currency.rate_from_usd != 1``, so the
 * formatter only needs the right symbol and the standard precision rules.
 *
 * Policy:
 * - exactly 0       → `<symbol>0`
 * - 0 < |x| < 0.01  → 4-decimal precision so sub-cent values stay visible
 * - 0.01 ≤ |x| < 1k → 2 decimals
 * - |x| ≥ 1000      → 2 decimals with locale thousands separators
 *
 * Negative values get a leading `-` (preserved through all branches).
 *
 * @param cost The amount to render, *already in the active currency*.
 * @param currency Optional currency block (typically from `useCurrency()` /
 *   the `/api/dashboard-data` payload). When omitted, falls back to USD with
 *   the `$` symbol so legacy callers keep rendering correctly.
 */
export function formatCost(cost: number, currency?: CurrencyInfo | null): string {
  const symbol = currency?.symbol ?? '$'
  if (!Number.isFinite(cost)) return `${symbol}0`
  if (cost === 0) return `${symbol}0`
  const sign = cost < 0 ? '-' : ''
  const abs = Math.abs(cost)
  if (abs < 0.01) return `${sign}${symbol}${abs.toFixed(4)}`
  if (abs >= 1000) {
    return `${sign}${symbol}${abs.toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    })}`
  }
  return `${sign}${symbol}${abs.toFixed(2)}`
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

/**
 * Compact, human-readable display name for a model id.
 *
 * The store keeps the raw `(provider, model)` pair the adapter saw — Anthropic
 * emits `claude-opus-4-7`, Cursor emits `claude-4.5-sonnet-thinking`, Gemini
 * emits `gemini-2.5-pro-preview-05-06`, etc. Rendered as-is, those land in
 * 25-40 char strings and overflow every table column on a multi-provider
 * store. This normalizer maps the families we know about to a short label
 * (`Opus 4.7`, `Sonnet 4.5 (thinking)`, `Gemini 2.5 Pro Preview`) and falls
 * back to a title-cased prettifier otherwise — never throws on an
 * unrecognized id, never strips information beyond a trailing date suffix.
 *
 * Callers should pass the original id as a `title` attribute so the full
 * string remains discoverable on hover.
 */
export function formatModelName(modelId: string): string {
  if (!modelId) return ''

  // Strip a trailing -YYYYMMDD date stamp Anthropic / Gemini sometimes append.
  // e.g. `claude-opus-4-5-20251101` → `claude-opus-4-5`.
  let id = modelId.replace(/-\d{8}$/, '')

  // Strip Gemini's -MM-DD preview date (e.g. `-preview-05-06` → `-preview`).
  id = id.replace(/-(\d{2})-(\d{2})$/, '')

  // ── Anthropic families ────────────────────────────────────────────────
  // Native:  claude-opus-4-7, claude-sonnet-4-6, claude-haiku-4-5
  // Cursor:  claude-4.5-sonnet, claude-4.5-sonnet-thinking, claude-3.5-haiku
  // (`!` after match groups: TypeScript's `noUncheckedIndexedAccess` widens
  // capture groups to `string | undefined`; a successful match guarantees the
  // numbered groups in the patterns below are present.)
  const native = id.match(/^claude-(opus|sonnet|haiku)-(\d+)-(\d+)$/i)
  if (native) {
    return `${cap(native[1]!)} ${native[2]!}.${native[3]!}`
  }
  const cursorClaude = id.match(/^claude-(\d+(?:\.\d+)?)-(opus|sonnet|haiku)(?:-(.+))?$/i)
  if (cursorClaude) {
    const version = cursorClaude[1]!
    const family = cursorClaude[2]!
    const suffix = cursorClaude[3]
    const tail = suffix ? ` (${suffix.replace(/-/g, ' ')})` : ''
    return `${cap(family)} ${version}${tail}`
  }

  // ── GLM ───────────────────────────────────────────────────────────────
  const glm = id.match(/^glm-(\d+(?:\.\d+)?)$/i)
  if (glm) return `GLM ${glm[1]!}`

  // ── OpenAI / Codex ────────────────────────────────────────────────────
  // gpt-5 → GPT-5; gpt-5-codex → GPT-5 Codex; gpt-4o → GPT-4o
  const gpt = id.match(/^gpt-(\d+(?:\.\d+)?[a-z]?)(?:-(.+))?$/i)
  if (gpt) {
    const version = gpt[1]!
    const suffix = gpt[2]
    return suffix ? `GPT-${version} ${suffix.split('-').map(cap).join(' ')}` : `GPT-${version}`
  }

  // ── Gemini ────────────────────────────────────────────────────────────
  // gemini-2.5-pro, gemini-2.5-pro-preview, gemini-3-flash-preview, gemini-3.1-pro-preview
  const gemini = id.match(/^gemini-(\d+(?:\.\d+)?)-(.+)$/i)
  if (gemini) {
    const version = gemini[1]!
    const rest = gemini[2]!
    const parts = rest.split('-').map(cap).join(' ')
    return `Gemini ${version} ${parts}`
  }

  // ── Cursor / Cline auto-pickers and Composer ──────────────────────────
  if (/^cursor-auto$/i.test(id)) return 'Cursor Auto'
  if (/^cursor-fast$/i.test(id)) return 'Cursor Fast'
  if (/^cline-auto$/i.test(id)) return 'Cline Auto'
  const composer = id.match(/^composer-(\d+)$/i)
  if (composer) return `Composer ${composer[1]!}`

  // ── Synthetic / unknown — pass through so nothing surprises the caller
  return id
}

function cap(word: string): string {
  if (!word) return word
  return word.charAt(0).toUpperCase() + word.slice(1).toLowerCase()
}

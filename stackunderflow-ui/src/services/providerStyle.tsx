/**
 * Provider colour map — the single source of truth for which Tailwind
 * `Badge` colour each ingest provider gets across the dashboard.
 *
 * Used by `ProviderChip`, the Compare table, the Cost-by-provider card,
 * and any other surface that wants to render a per-agent visual tag.
 *
 * Spec (from the v0.6.1 multi-provider polish issue):
 *   claude  → indigo (rendered as 'blue' since Badge doesn't ship indigo)
 *   codex   → emerald (rendered as 'green')
 *   cursor  → cyan (rendered as 'blue' — Badge has no cyan)
 *   cline   → violet (rendered as 'purple')
 *   gemini  → orange
 *   droid   → rose (rendered as 'red')
 *   qwen    → teal (rendered as 'green')
 *   kilocode/roocode → purple
 *   default unknown → gray
 *
 * The mapping deliberately collapses onto Badge's 7-colour palette
 * (`blue | green | yellow | red | purple | orange | gray`). The intent
 * column is preserved as comments so a future palette expansion can swap
 * them without re-deriving the design choice.
 */

import type { ReactNode } from 'react'
import Badge from '../components/common/Badge'

export type ProviderColor = 'blue' | 'green' | 'yellow' | 'red' | 'purple' | 'orange' | 'gray'

const PROVIDER_COLORS: Record<string, ProviderColor> = {
  // Anthropic native ingest (Claude Code).
  claude: 'blue',
  anthropic: 'blue',
  // OpenAI Codex CLI.
  codex: 'green',
  openai: 'green',
  // Cursor IDE (vscdb).
  cursor: 'purple',
  // Cline (VS Code extension, globalStorage).
  cline: 'orange',
  // Google Gemini CLI.
  gemini: 'orange',
  // Factory.ai droid CLI.
  droid: 'red',
  // Alibaba Qwen Coder CLI.
  qwen: 'green',
  // Forks of Cline.
  kilocode: 'purple',
  roocode: 'purple',
  // OpenCode + opencodex variants.
  opencode: 'yellow',
  // Continue.dev.
  continue: 'yellow',
  // Cursor's standalone agent CLI.
  cursor_agent: 'purple',
  // GitHub Copilot CLI.
  copilot: 'green',
  // Codeium CLI.
  codeium: 'yellow',
  // Kiro / Kiro Studio.
  kiro: 'red',
  // OpenClaw + Pi + Hermes (promoted default-on adapters).
  openclaw: 'blue',
  pi: 'green',
  hermes: 'purple',
}

// Wire-name → display-name normalisation (e.g. "anthropic" → "claude" so
// the chip reads like the CLI brand the user actually launched).
const PROVIDER_LABELS: Record<string, string> = {
  anthropic: 'claude',
  openai: 'codex',
}

export function getProviderColor(provider: string | null | undefined): ProviderColor {
  const raw = (provider ?? '').toLowerCase().trim()
  return PROVIDER_COLORS[raw] ?? 'gray'
}

export function getProviderLabel(provider: string | null | undefined): string {
  const raw = (provider ?? '').toLowerCase().trim()
  if (!raw) return 'unknown'
  return PROVIDER_LABELS[raw] ?? raw
}

/**
 * Cheap model-id shortener used until PR B's `formatModelName` lands. Strips
 * trailing `-YYYYMMDD` date suffixes (e.g. `claude-opus-4-5-20251101`
 * → `claude-opus-4-5`) and `-preview-MM-DD` Cursor-style suffixes
 * (e.g. `gemini-2.5-pro-preview-05-06` → `gemini-2.5-pro`).
 *
 * Callers SHOULD pair this with a `title={model}` tooltip so the original
 * id is still discoverable.
 */
export function shortenModelId(model: string | null | undefined): string {
  if (!model) return ''
  return model
    .replace(/-\d{8}$/, '')
    .replace(/-preview-\d{2}-\d{2}$/, '')
    .replace(/-experimental-\d{2}-\d{2}$/, '')
}

interface ProviderModelLabelProps {
  provider: string | null | undefined
  model: string | null | undefined
  /** When true, render only the chip (no model id text). */
  chipOnly?: boolean
  /** Override the displayed model id (e.g. when callers already aliased it). */
  modelOverride?: ReactNode
}

/**
 * Two-element layout used in dense tables: provider chip + truncated model
 * id with the full id available via the title attribute. Replaces the bare
 * `<span>{row.model}</span>` rendering across Compare/Sessions/Messages so
 * the same model used by two different agents reads at a glance.
 */
export function ProviderModelLabel({ provider, model, chipOnly = false, modelOverride }: ProviderModelLabelProps) {
  const color = getProviderColor(provider)
  const label = getProviderLabel(provider)
  const display = modelOverride ?? shortenModelId(model)
  return (
    <span className="inline-flex items-center gap-1.5 min-w-0">
      <Badge color={color} size="sm">{label}</Badge>
      {!chipOnly && model && (
        <span
          className="font-mono text-xs text-gray-700 dark:text-gray-300 truncate"
          title={model}
        >
          {display}
        </span>
      )}
    </span>
  )
}

import Badge from './Badge'
import { getProviderColor, getProviderLabel } from '../../services/providerStyle'

// Multi-provider polish (spec.md §6 Step 5).
// Renders a small chip showing which provider a session/project came from.
// Backend gap: as of wave2/foundation neither /api/jsonl-files nor
// /api/projects emits `provider`, so callers will pass `undefined` and we
// fall back to "unknown" (gray).
//
// v0.6.1 follow-up: the colour palette + label normalisation now lives in
// `services/providerStyle.ts` so the Compare table, the Cost-by-provider
// card, and the existing chip all stay in lock-step.

interface ProviderChipProps {
  provider: string | null | undefined
  size?: 'sm' | 'md'
}

export default function ProviderChip({ provider, size = 'sm' }: ProviderChipProps) {
  const color = getProviderColor(provider)
  const label = getProviderLabel(provider)
  return (
    <Badge color={color} size={size}>
      {label}
    </Badge>
  )
}

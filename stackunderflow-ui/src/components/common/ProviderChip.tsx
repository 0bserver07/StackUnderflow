import Badge from './Badge'

// Multi-provider polish (spec.md §6 Step 5).
// Renders a small chip showing which provider a session/project came from.
// Backend gap: as of wave2/foundation neither /api/jsonl-files nor
// /api/projects emits `provider`, so callers will pass `undefined` and we
// fall back to "unknown" (gray).

type BadgeColor = 'blue' | 'green' | 'yellow' | 'red' | 'purple' | 'orange' | 'gray'

const PROVIDER_COLORS: Record<string, BadgeColor> = {
  claude: 'blue',
  anthropic: 'blue',
  codex: 'green',
  openai: 'green',
  cursor: 'purple',
  cline: 'orange',
}

// Normalise the wire provider value (e.g. "anthropic", "claude") to a label
// that matches the spec's color hints: claude, codex, cursor, cline.
const PROVIDER_LABELS: Record<string, string> = {
  anthropic: 'claude',
  openai: 'codex',
}

interface ProviderChipProps {
  provider: string | null | undefined
  size?: 'sm' | 'md'
}

export default function ProviderChip({ provider, size = 'sm' }: ProviderChipProps) {
  const raw = (provider ?? '').toLowerCase().trim()
  const label = PROVIDER_LABELS[raw] ?? (raw || 'unknown')
  const color: BadgeColor = PROVIDER_COLORS[raw] ?? 'gray'
  return (
    <Badge color={color} size={size}>
      {label}
    </Badge>
  )
}

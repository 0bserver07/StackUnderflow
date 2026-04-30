// Multi-provider polish (spec.md §6 Step 5 + §3.1).
// Renders a small `≈` glyph with a tooltip explaining that the cost is
// derived from estimated tokens — currently triggered by Cursor sessions
// where the provider does not surface per-message token counts.
//
// Backend gap: as of wave2/foundation the aggregator does not yet propagate
// `cost_source` through to SessionCost / CommandCost output records. This
// component is a no-op when the prop is anything other than "estimated", so
// it lights up automatically once the backend wires it through.

interface EstimatedCostMarkerProps {
  costSource?: 'estimated' | 'actual' | null
  /** Optional override for the tooltip copy. */
  title?: string
}

const DEFAULT_TITLE =
  'Estimated cost — provider does not surface per-message tokens'

export default function EstimatedCostMarker({
  costSource,
  title = DEFAULT_TITLE,
}: EstimatedCostMarkerProps) {
  if (costSource !== 'estimated') return null
  return (
    <span
      aria-label="estimated cost"
      title={title}
      className="text-amber-500 dark:text-amber-400 mr-0.5 cursor-help"
    >
      ≈
    </span>
  )
}

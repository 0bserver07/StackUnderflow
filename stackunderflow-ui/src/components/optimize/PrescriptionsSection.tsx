import { useQuery } from '@tanstack/react-query'
import { IconPrescription } from '@tabler/icons-react'
import { getPrescriptions } from '../../services/api'
import RoutingRecCard from './RoutingRecCard'
import ClaudeMdPreviewCard from './ClaudeMdPreviewCard'

// ---------------------------------------------------------------------------
// PrescriptionsSection — campaign #7. Turns the Optimize surface's findings
// into actions: model-routing recommendation cards (with estimated monthly
// $ deltas) and slimmer-CLAUDE.md previews (diff + copy/download).
//
// Mounted below the OptimizeFindingsPanel card. Everything here is
// advisory and read-only: the server computes previews as pure functions
// and never writes user files; "apply" is copy/download client-side.
//
// With no explicit project param the endpoint scopes to the active project
// (server-side `deps.current_log_path`), matching /api/forks — so this
// component needs no plumbing from the dashboard shell.
// ---------------------------------------------------------------------------

export default function PrescriptionsSection() {
  const { data, isLoading, error } = useQuery({
    queryKey: ['optimize', 'prescriptions'],
    queryFn: () => getPrescriptions(),
    staleTime: 5 * 60_000,
  })

  // Supplementary panel: hide while loading/errored rather than flashing a
  // spinner above the primary stats (same philosophy as the findings panel).
  if (isLoading || error || !data) return null

  const recs = data.routing?.recommendations ?? []
  const previews = data.claudemd_previews ?? []
  if (recs.length === 0 && previews.length === 0) return null

  const caveats = data.routing?.caveats ?? []

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <IconPrescription size={16} className="text-gray-500" />
        <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200">Prescriptions</h3>
        <span className="text-xs text-gray-500">
          {recs.length + previews.length} suggested action
          {recs.length + previews.length === 1 ? '' : 's'} · {data.scope}
        </span>
      </div>

      {recs.length > 0 && (
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-3">
          {recs.map((rec, i) => (
            <RoutingRecCard
              key={`${rec.rec_id}-${rec.from_model}-${i}`}
              rec={rec}
              currency={data.currency}
            />
          ))}
        </div>
      )}

      {previews.map((p, i) => (
        <ClaudeMdPreviewCard
          key={`${p.file_label}-${i}`}
          preview={p}
          currency={data.currency}
        />
      ))}

      {recs.length > 0 && caveats.length > 0 && (
        <ul className="space-y-0.5">
          {caveats.map((c, i) => (
            <li key={i} className="text-[11px] text-gray-400 dark:text-gray-500">
              {c}
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

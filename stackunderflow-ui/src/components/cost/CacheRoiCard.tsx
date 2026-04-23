import { IconDatabase, IconCircleCheck, IconCircleDashed } from '@tabler/icons-react'
import type { CacheStats } from '../../types/api'

interface CacheRoiCardProps {
  cache: CacheStats | null | undefined
}

function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return n.toLocaleString()
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

/**
 * Hero card for cache ROI — the marquee metric for "am I getting my money's worth?".
 * ROI % is computed from tokens_saved vs tokens written into the cache.
 */
export default function CacheRoiCard({ cache }: CacheRoiCardProps) {
  if (!cache || cache.total_created === 0) {
    return (
      <div className="bg-gradient-to-br from-indigo-900/30 to-gray-900/30 rounded-lg p-6 border border-indigo-900/40">
        <div className="flex items-center gap-2 mb-3">
          <IconDatabase size={18} className="text-indigo-400" />
          <span className="text-xs text-gray-400 uppercase tracking-wider">Cache ROI</span>
        </div>
        <div className="text-gray-500 text-sm">No cache activity yet</div>
      </div>
    )
  }

  const tokensSaved = cache.tokens_saved ?? 0
  const tokensCreated = cache.total_created ?? 0
  const roiPct = tokensCreated > 0 ? (tokensSaved / tokensCreated) * 100 : 0
  const costSaved = cache.cost_saved_base_units ?? 0
  const breakEven = cache.break_even_achieved === true
  const hitRate = cache.hit_rate ?? 0

  return (
    <div className="bg-gradient-to-br from-indigo-900/40 to-gray-900/40 rounded-lg p-6 border border-indigo-900/50">
      <div className="flex items-center gap-2 mb-3">
        <IconDatabase size={18} className="text-indigo-400" />
        <span className="text-xs text-gray-400 uppercase tracking-wider">Cache ROI</span>
        <div className="flex-1" />
        {breakEven ? (
          <span className="inline-flex items-center gap-1 text-[10px] text-green-300 bg-green-900/40 border border-green-800 rounded-full px-2 py-0.5">
            <IconCircleCheck size={10} /> break-even
          </span>
        ) : (
          <span className="inline-flex items-center gap-1 text-[10px] text-amber-300 bg-amber-900/30 border border-amber-800 rounded-full px-2 py-0.5">
            <IconCircleDashed size={10} /> below break-even
          </span>
        )}
      </div>

      <div className="text-5xl font-bold text-indigo-300 leading-none">
        {roiPct.toFixed(0)}
        <span className="text-2xl text-indigo-400/80 align-top ml-1">%</span>
      </div>
      <div className="text-xs text-gray-500 mt-1">return on cache writes</div>

      <div className="grid grid-cols-3 gap-4 mt-5 pt-4 border-t border-indigo-900/30">
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Tokens Saved</div>
          <div className="text-lg font-semibold text-gray-100 mt-0.5">{formatNumber(tokensSaved)}</div>
        </div>
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Cost Saved</div>
          <div className="text-lg font-semibold text-green-300 mt-0.5">{formatCost(costSaved)}</div>
        </div>
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Hit Rate</div>
          <div className="text-lg font-semibold text-gray-100 mt-0.5">{hitRate.toFixed(1)}%</div>
        </div>
      </div>
    </div>
  )
}

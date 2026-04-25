interface BetaBadgeProps {
  className?: string
}

const DEFAULT_CLASSES =
  'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300 text-[10px] px-1.5 py-0.5 rounded uppercase tracking-wider font-semibold'

/**
 * Small inline "BETA" pill used to flag experimental/heuristic surfaces.
 *
 * Consumers may pass an extra `className` to tweak spacing or layout; it is
 * appended after the defaults so Tailwind's later-wins ordering lets callers
 * override specific utilities when needed.
 */
export default function BetaBadge({ className }: BetaBadgeProps) {
  const classes = className ? `${DEFAULT_CLASSES} ${className}` : DEFAULT_CLASSES
  return <span className={classes}>BETA</span>
}

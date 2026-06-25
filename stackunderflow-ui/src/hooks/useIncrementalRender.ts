import { useCallback, useEffect, useRef, useState } from 'react'

export interface IncrementalRenderOptions {
  /** Total number of items available to render. */
  total: number
  /** How many items to render on first paint / after a reset. */
  initialCount?: number
  /** How many items to append each time the sentinel scrolls into view. */
  step?: number
  /**
   * When this string changes the window collapses back to `initialCount`.
   * Pass a key derived from sort / filter / search state so changing any of
   * them returns the user to the top of a fresh list instead of leaving a
   * stale, deep window in place.
   */
  resetKey?: string
}

export interface IncrementalRenderResult {
  /** Number of items to render now (`items.slice(0, visibleCount)`). */
  visibleCount: number
  /** Attach to an element rendered just below the list. */
  sentinelRef: React.RefObject<HTMLDivElement>
  /** True while more items remain beyond the current window. */
  hasMore: boolean
  /**
   * Grow the window so at least `count` items are rendered (never shrinks it).
   * Use to reveal a deep-linked / scrolled-to item that would otherwise sit
   * past the current window.
   */
  revealUpTo: (count: number) => void
}

/**
 * Incremental ("windowed") rendering without a virtualization dependency.
 *
 * Renders an initial batch and appends more as a sentinel element near the
 * bottom of the list scrolls into view (IntersectionObserver, with a generous
 * `rootMargin` so the next batch is ready before the user reaches the end).
 * This keeps the familiar infinite-scroll feel while avoiding the cost of
 * mounting thousands of rows up front.
 *
 *   const { visibleCount, sentinelRef, hasMore } = useIncrementalRender({
 *     total: items.length,
 *     resetKey: `${sort}|${filter}`,
 *   })
 *   items.slice(0, visibleCount).map(...)
 *   {hasMore && <div ref={sentinelRef} />}
 *
 * Each appended batch is far taller than `rootMargin`, so adding a batch pushes
 * the sentinel back out of view and the next scroll re-triggers the observer.
 */
export function useIncrementalRender({
  total,
  initialCount = 60,
  step = 60,
  resetKey = '',
}: IncrementalRenderOptions): IncrementalRenderResult {
  const [visibleCount, setVisibleCount] = useState(initialCount)
  const sentinelRef = useRef<HTMLDivElement>(null)

  // Collapse the window whenever the list identity changes (sort/filter/search).
  useEffect(() => {
    setVisibleCount(initialCount)
  }, [resetKey, initialCount])

  const clamped = Math.min(visibleCount, total)
  const hasMore = clamped < total

  useEffect(() => {
    if (!hasMore) return
    const el = sentinelRef.current
    if (!el) return
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setVisibleCount((c) => Math.min(total, c + step))
        }
      },
      { rootMargin: '400px 0px' },
    )
    observer.observe(el)
    return () => observer.disconnect()
  }, [hasMore, total, step])

  const revealUpTo = useCallback((count: number) => {
    setVisibleCount((c) => (count > c ? count : c))
  }, [])

  return { visibleCount: clamped, sentinelRef, hasMore, revealUpTo }
}

// useEventStream — React hook over `connectLiveStream`.
//
// Owns the EventSource lifecycle: opens on mount, closes on unmount,
// keeps a rolling buffer of the last 100 events / tool_calls in state,
// and surfaces the most recent burn-tick + watcher snapshot. Consumers
// just render the returned dictionary.
//
// The 100-row cap matches the spec ("last 100 events"). New rows
// arrive at the head of each list so the UI doesn't have to reverse
// before render.

import { useEffect, useRef, useState } from 'react'
import { connectLiveStream } from '../services/live'
import type {
  LiveBurnSnapshot,
  LiveEventRowPayload,
  LiveToolCallRowPayload,
  LiveWatcherStatus,
} from '../types/api'

const ROLLING_BUFFER_LIMIT = 100

export interface LiveStreamState {
  /** Has the SSE handshake completed? `false` between mount and the
   *  first `ready` event; flips to `true` and stays. */
  connected: boolean
  /** Rolling buffer of recent `usage_events` rows (newest first). */
  events: LiveEventRowPayload[]
  /** Rolling buffer of recent `message_tool_mart` rows (newest first). */
  toolCalls: LiveToolCallRowPayload[]
  /** Most recent burn-rate snapshot (5s cadence). `null` until first tick. */
  burn: LiveBurnSnapshot | null
  /** Watcher state (echoed from the `ready` event). `null` pre-handshake. */
  watcher: LiveWatcherStatus | null
  /** Connection error count (bumps on every transient disconnect). */
  errorCount: number
}

const INITIAL_STATE: LiveStreamState = {
  connected: false,
  events: [],
  toolCalls: [],
  burn: null,
  watcher: null,
  errorCount: 0,
}

export function useEventStream(enabled: boolean = true): LiveStreamState {
  const [state, setState] = useState<LiveStreamState>(INITIAL_STATE)
  // Hold the live state in a ref so the SSE callbacks don't have to
  // close over a stale snapshot (state setters with the functional
  // form work too, but the ref keeps each handler O(1) and lets us
  // batch all four updates into one setState if we want to later).
  const stateRef = useRef<LiveStreamState>(INITIAL_STATE)

  useEffect(() => {
    if (!enabled) return
    if (typeof EventSource === 'undefined') return

    const handle = connectLiveStream({
      onReady: (payload) => {
        setState((prev) => {
          const next = { ...prev, connected: true, watcher: payload.watcher }
          stateRef.current = next
          return next
        })
      },
      onEvent: (payload) => {
        setState((prev) => {
          const events = [payload, ...prev.events].slice(0, ROLLING_BUFFER_LIMIT)
          const next = { ...prev, events }
          stateRef.current = next
          return next
        })
      },
      onToolCall: (payload) => {
        setState((prev) => {
          const toolCalls = [payload, ...prev.toolCalls].slice(
            0, ROLLING_BUFFER_LIMIT,
          )
          const next = { ...prev, toolCalls }
          stateRef.current = next
          return next
        })
      },
      onBurnTick: (payload) => {
        setState((prev) => {
          const next = { ...prev, burn: payload }
          stateRef.current = next
          return next
        })
      },
      onError: () => {
        setState((prev) => {
          const next = { ...prev, errorCount: prev.errorCount + 1 }
          stateRef.current = next
          return next
        })
      },
    })

    return () => handle.close()
  }, [enabled])

  return state
}

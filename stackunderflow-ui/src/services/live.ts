// Live observability fetcher + EventSource wrapper — Spec 13.
//
// Two surfaces mirror the backend:
//
//   • `getLiveStats()` — one-shot snapshot for the initial render.
//   • `connectLiveStream(handlers)` — opens the SSE stream, dispatches
//     typed handlers per event, and returns a `close()` function the
//     caller invokes on unmount so the connection drops cleanly.
//
// The hook layer (`useEventStream`) is in `hooks/useEventStream.ts`
// so React-specific concerns stay out of this module — keeps the
// service pure-JS and trivially testable with a stubbed `EventSource`
// (see tests/services/live.test.ts).

import type {
  LiveBurnSnapshot,
  LiveEventRowPayload,
  LiveStatsResponse,
  LiveToolCallRowPayload,
  LiveWatermarks,
  LiveWatcherStatus,
} from '../types/api'

const BASE = '/api'

export async function getLiveStats(): Promise<LiveStatsResponse> {
  const res = await fetch(`${BASE}/live/stats`)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

// ---------------------------------------------------------------------------
// EventSource wrapper. Browsers expose `addEventListener(name, …)` for the
// SSE `event:` names — we use the same dispatch on this side rather than
// parsing the JSON's inner `type` field. A single stream serves all four
// payloads (ready / event / tool_call / burn_tick).
// ---------------------------------------------------------------------------

export interface LiveStreamHandlers {
  /** Initial seed: current watermarks + watcher state at connect time. */
  onReady?: (payload: {
    watermarks: LiveWatermarks
    watcher: LiveWatcherStatus
    burn_interval_seconds: number
  }) => void
  /** A new `usage_events` row landed (cost / token grain). */
  onEvent?: (payload: LiveEventRowPayload, ts: string) => void
  /** A new `message_tool_mart` row landed (per-tool-call grain). */
  onToolCall?: (payload: LiveToolCallRowPayload, ts: string) => void
  /** Periodic 5s burn-rate tick. */
  onBurnTick?: (payload: LiveBurnSnapshot, ts: string) => void
  /** Connection error (network drop, server crash). The browser will
   *  auto-reconnect on its own timer; this callback is purely
   *  informational so the UI can render a transient banner. */
  onError?: (event: Event) => void
}

export interface LiveStreamHandle {
  /** Close the stream. Idempotent. */
  close: () => void
  /** Underlying source — exposed for tests / debug only. */
  source: EventSource
}

/** Open the SSE stream and dispatch typed callbacks per event name.
 *
 *  The caller MUST invoke `handle.close()` on unmount (or whenever the
 *  stream is no longer needed) so the server cleans up its async
 *  generator. The browser will otherwise reconnect on its own timer
 *  after a tab is hidden / suspended, which over time leaks slot on
 *  the server side. */
export function connectLiveStream(
  handlers: LiveStreamHandlers,
  url: string = `${BASE}/live/stream`,
): LiveStreamHandle {
  const source = new EventSource(url)

  const dispatch = <T,>(
    cb: ((p: T, ts: string) => void) | ((p: T) => void) | undefined,
    ev: MessageEvent<string>,
    withTs: boolean,
  ) => {
    if (!cb) return
    let parsed: { type?: string; ts?: string; payload?: T } = {}
    try {
      parsed = JSON.parse(ev.data) as typeof parsed
    } catch {
      // Malformed JSON — drop silently rather than crashing the listener.
      return
    }
    if (parsed.payload === undefined) return
    if (withTs) {
      ;(cb as (p: T, ts: string) => void)(parsed.payload, parsed.ts ?? '')
    } else {
      ;(cb as (p: T) => void)(parsed.payload)
    }
  }

  source.addEventListener('ready', (ev) =>
    dispatch(handlers.onReady, ev as MessageEvent<string>, false),
  )
  source.addEventListener('event', (ev) =>
    dispatch(handlers.onEvent, ev as MessageEvent<string>, true),
  )
  source.addEventListener('tool_call', (ev) =>
    dispatch(handlers.onToolCall, ev as MessageEvent<string>, true),
  )
  source.addEventListener('burn_tick', (ev) =>
    dispatch(handlers.onBurnTick, ev as MessageEvent<string>, true),
  )

  if (handlers.onError) {
    source.addEventListener('error', handlers.onError)
  }

  let closed = false
  return {
    source,
    close: () => {
      if (closed) return
      closed = true
      source.close()
    },
  }
}

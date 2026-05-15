// Inline collapsed surface that renders a meta-agent tool invocation.
//
// Default state: ``[tool] tool_name · 124ms · ok`` on one line. Click
// to expand a ``<details>`` block that shows the raw args + result as
// pretty-printed JSON. Long values inside the JSON get a 4 KB hard cut
// so a noisy ``data`` payload can't render multiple screens of text.
//
// We render this above the assistant bubble that follows the tool call
// in the chat. ``status`` distinguishes "still executing" (no result
// yet) from a finished call.
//
// Pure helpers (label / summary / JSON formatter) live in the sibling
// ``toolCallSurface.ts`` module so Node's test runner can import them
// without choking on .tsx; we re-export them here for callers that
// already import from this file.

import { useState } from 'react'
import {
  IconChevronRight,
  IconChevronDown,
  IconTool,
  IconCircleCheck,
  IconAlertTriangle,
  IconLoader2,
} from '@tabler/icons-react'
import type { MetaAgentToolInvocation } from '../../types/metaAgent'
import { _formatJson, buildToolStatusLabel, buildToolSummary } from './toolCallSurfaceHelpers'

export { buildToolStatusLabel, buildToolSummary }

interface ToolCallSurfaceProps {
  invocation: MetaAgentToolInvocation
}

export default function ToolCallSurface({ invocation }: ToolCallSurfaceProps) {
  const [open, setOpen] = useState(false)
  const status = buildToolStatusLabel(invocation)
  const summary = buildToolSummary(invocation)
  const ok = invocation.result?.ok
  const running = !invocation.result

  return (
    <div
      data-testid="meta-tool-call"
      data-tool-name={invocation.name}
      data-tool-status={running ? 'running' : ok ? 'ok' : 'error'}
      className="my-2 border border-gray-200 dark:border-gray-800 bg-gray-50 dark:bg-gray-900/50 rounded-lg text-xs"
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-2 w-full px-2.5 py-1.5 text-left hover:bg-gray-100 dark:hover:bg-gray-800/50 rounded-lg"
      >
        {open ? (
          <IconChevronDown size={12} className="text-gray-500 shrink-0" />
        ) : (
          <IconChevronRight size={12} className="text-gray-500 shrink-0" />
        )}
        <span className="px-1.5 py-0.5 bg-indigo-100 dark:bg-indigo-900/40 text-indigo-700 dark:text-indigo-300 rounded inline-flex items-center gap-1 shrink-0">
          <IconTool size={10} />
          tool
        </span>
        <span className="font-mono text-gray-800 dark:text-gray-200 truncate">{invocation.name}</span>
        <span className="flex-1" />
        {summary && (
          <span className="text-gray-500 dark:text-gray-400 truncate hidden sm:inline">{summary}</span>
        )}
        <span className="inline-flex items-center gap-1 text-gray-500 dark:text-gray-400 shrink-0">
          {running && <IconLoader2 size={12} className="animate-spin" />}
          {!running && ok && <IconCircleCheck size={12} className="text-emerald-500" />}
          {!running && !ok && <IconAlertTriangle size={12} className="text-amber-500" />}
          {status}
        </span>
      </button>
      {open && (
        <div className="px-3 pb-3 pt-1 space-y-2 border-t border-gray-200 dark:border-gray-800">
          <div>
            <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">args</div>
            <pre
              data-testid="meta-tool-args"
              className="bg-white dark:bg-gray-950 border border-gray-200 dark:border-gray-800 rounded p-2 overflow-auto text-[11px] text-gray-700 dark:text-gray-300 max-h-48"
            >
              {_formatJson(invocation.args)}
            </pre>
          </div>
          {invocation.result && (
            <div>
              <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">result</div>
              <pre
                data-testid="meta-tool-result"
                className="bg-white dark:bg-gray-950 border border-gray-200 dark:border-gray-800 rounded p-2 overflow-auto text-[11px] text-gray-700 dark:text-gray-300 max-h-64"
              >
                {_formatJson(invocation.result.data)}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

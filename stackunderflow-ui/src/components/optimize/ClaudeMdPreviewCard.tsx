import { useState } from 'react'
import {
  IconCheck,
  IconChevronDown,
  IconChevronRight,
  IconCopy,
  IconDownload,
  IconFileText,
} from '@tabler/icons-react'
import type { ClaudeMdPreview, CurrencyInfo } from '../../types/api'
import { formatCost, formatNumber } from '../../services/format'

// ---------------------------------------------------------------------------
// ClaudeMdPreviewCard — the slimmer-CLAUDE.md preview: a colored unified-diff
// view, per-rule rationale, and estimated savings.
//
// APPLY IS CLIENT-SIDE ONLY. The server never writes user files — the two
// actions here are copy-to-clipboard and a Blob download of the slimmed
// text. Whether (and where) to save it is entirely the user's call.
// ---------------------------------------------------------------------------

interface Props {
  preview: ClaudeMdPreview
  currency?: CurrencyInfo | null
}

function diffLineClass(line: string): string {
  if (line.startsWith('+++') || line.startsWith('---')) {
    return 'text-gray-500 dark:text-gray-400 font-semibold'
  }
  if (line.startsWith('@@')) return 'text-indigo-600 dark:text-indigo-400'
  if (line.startsWith('+')) {
    return 'text-emerald-700 dark:text-emerald-400 bg-emerald-50 dark:bg-emerald-900/20'
  }
  if (line.startsWith('-')) {
    return 'text-rose-700 dark:text-rose-400 bg-rose-50 dark:bg-rose-900/20'
  }
  return 'text-gray-600 dark:text-gray-400'
}

function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false)
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      // Clipboard unavailable (permissions/insecure context) — stay silent;
      // the download button is the fallback.
    }
  }
  return (
    <button
      type="button"
      onClick={onCopy}
      className="inline-flex items-center gap-1 px-2 py-1 text-[11px] rounded border border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-50 dark:hover:bg-gray-800"
    >
      {copied ? <IconCheck size={12} className="text-emerald-500" /> : <IconCopy size={12} />}
      {copied ? 'Copied' : label}
    </button>
  )
}

function downloadText(filename: string, text: string) {
  const blob = new Blob([text], { type: 'text/markdown' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

export default function ClaudeMdPreviewCard({ preview, currency }: Props) {
  const [showDiff, setShowDiff] = useState(false)
  const diffLines = preview.preview_diff.split('\n')

  return (
    <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900">
      <div className="px-4 py-3 flex items-start justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2 min-w-0">
          <IconFileText size={16} className="flex-shrink-0 text-gray-500" />
          <div className="min-w-0">
            <div className="text-sm font-medium text-gray-900 dark:text-gray-100 truncate">
              Slimmer {preview.source_path ?? preview.file_label}
            </div>
            <div className="text-[11px] text-gray-500 tabular-nums">
              ~{formatNumber(preview.original_tokens)} → ~{formatNumber(preview.slimmed_tokens)}{' '}
              tokens · saves ~{formatNumber(preview.tokens_saved)} tokens/session
            </div>
          </div>
        </div>
        <div className="text-right flex-shrink-0">
          <div className="text-sm font-semibold text-emerald-600 dark:text-emerald-400 tabular-nums">
            ~{formatCost(preview.estimated_savings_usd_monthly, currency)}/mo
          </div>
          <div className="text-[11px] text-gray-500">
            at {preview.sessions_per_month} sessions/mo (estimate)
          </div>
        </div>
      </div>

      {preview.rationale.length > 0 && (
        <ul className="px-4 pb-2 space-y-1">
          {preview.rationale.map((r, i) => (
            <li key={`${r.rule}-${i}`} className="text-xs text-gray-600 dark:text-gray-400 flex gap-2">
              <span className="text-gray-400 tabular-nums flex-shrink-0 w-24 text-right">
                −{formatNumber(r.tokens_saved)} tok
              </span>
              <span>{r.summary}</span>
            </li>
          ))}
        </ul>
      )}

      <div className="px-4 py-2.5 border-t border-gray-100 dark:border-gray-800 flex items-center gap-2 flex-wrap">
        <button
          type="button"
          onClick={() => setShowDiff(!showDiff)}
          className="inline-flex items-center gap-1 text-[11px] text-indigo-600 dark:text-indigo-400 hover:underline"
          aria-expanded={showDiff}
        >
          {showDiff ? <IconChevronDown size={12} /> : <IconChevronRight size={12} />}
          {showDiff ? 'Hide diff' : 'Show diff'}
        </button>
        <div className="flex-1" />
        <CopyButton text={preview.slimmed_text} label="Copy slimmed CLAUDE.md" />
        <CopyButton text={preview.preview_diff} label="Copy diff" />
        <button
          type="button"
          onClick={() => downloadText('CLAUDE.slim.md', preview.slimmed_text)}
          className="inline-flex items-center gap-1 px-2 py-1 text-[11px] rounded border border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-50 dark:hover:bg-gray-800"
        >
          <IconDownload size={12} />
          Download
        </button>
      </div>

      {showDiff && (
        <pre className="mx-4 mb-3 p-3 rounded bg-gray-50 dark:bg-gray-950 border border-gray-100 dark:border-gray-800 overflow-x-auto text-[11px] leading-relaxed">
          {diffLines.map((line, i) => (
            <div key={i} className={diffLineClass(line)}>
              {line || ' '}
            </div>
          ))}
        </pre>
      )}

      <div className="px-4 pb-3 text-[11px] text-gray-400 dark:text-gray-500">
        Preview only — nothing is written for you. Review, then copy or download to apply.
      </div>
    </div>
  )
}

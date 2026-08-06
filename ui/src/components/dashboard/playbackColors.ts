/**
 * Shared tool → colour mapping for the Playback tab (scrubber ticks, event
 * list dots, detail badge). Tailwind class fragments only — no runtime CSS.
 *
 * Spec: .notes/specs/10-playback-timeline.md
 */

export interface ToolAccent {
  /** Background colour for a scrubber tick / list dot (e.g. `bg-blue-500`). */
  dot: string
  /** Chip classes (bg + text) for a small pill rendering the tool name. */
  chip: string
}

const PALETTE: Record<string, ToolAccent> = {
  read: { dot: 'bg-sky-500', chip: 'bg-sky-100 text-sky-800 dark:bg-sky-900/40 dark:text-sky-300' },
  notebookread: { dot: 'bg-sky-500', chip: 'bg-sky-100 text-sky-800 dark:bg-sky-900/40 dark:text-sky-300' },
  edit: { dot: 'bg-amber-500', chip: 'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300' },
  multiedit: { dot: 'bg-amber-500', chip: 'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300' },
  notebookedit: { dot: 'bg-amber-500', chip: 'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300' },
  write: { dot: 'bg-emerald-500', chip: 'bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300' },
  bash: { dot: 'bg-violet-500', chip: 'bg-violet-100 text-violet-800 dark:bg-violet-900/40 dark:text-violet-300' },
  bashoutput: { dot: 'bg-violet-500', chip: 'bg-violet-100 text-violet-800 dark:bg-violet-900/40 dark:text-violet-300' },
  glob: { dot: 'bg-cyan-500', chip: 'bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300' },
  grep: { dot: 'bg-cyan-500', chip: 'bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300' },
  ls: { dot: 'bg-cyan-500', chip: 'bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300' },
  task: { dot: 'bg-pink-500', chip: 'bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-300' },
  agent: { dot: 'bg-pink-500', chip: 'bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-300' },
  webfetch: { dot: 'bg-indigo-500', chip: 'bg-indigo-100 text-indigo-800 dark:bg-indigo-900/40 dark:text-indigo-300' },
  websearch: { dot: 'bg-indigo-500', chip: 'bg-indigo-100 text-indigo-800 dark:bg-indigo-900/40 dark:text-indigo-300' },
  todowrite: { dot: 'bg-teal-500', chip: 'bg-teal-100 text-teal-800 dark:bg-teal-900/40 dark:text-teal-300' },
}

const FALLBACK: ToolAccent = {
  dot: 'bg-gray-400',
  chip: 'bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300',
}

export function toolAccent(toolName: string): ToolAccent {
  if (!toolName) return FALLBACK
  const key = toolName.toLowerCase()
  if (key in PALETTE) return PALETTE[key]!
  // mcp__server__tool — colour all MCP calls the same.
  if (key.startsWith('mcp__')) {
    return { dot: 'bg-fuchsia-500', chip: 'bg-fuchsia-100 text-fuchsia-800 dark:bg-fuchsia-900/40 dark:text-fuchsia-300' }
  }
  return FALLBACK
}

/** The filter-chip groups shown above the timeline (in order). "All" is implicit. */
export const FILTER_CHIP_TOOLS = ['Read', 'Edit', 'Write', 'Bash', 'Glob', 'Grep', 'Task'] as const

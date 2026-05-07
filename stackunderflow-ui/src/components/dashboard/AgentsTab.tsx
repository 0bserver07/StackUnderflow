/**
 * Agents tab — surfaces Claude Code parallel-agent topology.
 *
 * Two-pane layout:
 *   - Left rail: list of recent agent-team activity. Click a team to load
 *     the dependency graph; the lead session is highlighted, with each
 *     spawned sub-agent indented underneath.
 *   - Right pane: when an agent is selected, show its first/last user
 *     prompts, message count, cost, and model.
 *
 * URL state: `?session=<lead>&agent=<sub>` so the view is shareable
 * and back/forward-button navigable. Keyboard nav (j/k or ↑/↓) cycles
 * through siblings in the active team's agents list.
 *
 * Spec: docs/specs/agent-teams.md
 */

import { useEffect, useMemo, useState, useCallback, useRef } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconHierarchy3,
  IconRobot,
  IconUser,
  IconClock,
  IconHash,
  IconMessage,
  IconChevronRight,
} from '@tabler/icons-react'

import {
  listAgentTeams,
  getAgentTeam,
  readAgentTeamSelection,
  writeAgentTeamSelection,
} from '../../services/api'
import { formatCost, formatModelName, formatNumber } from '../../services/format'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import type { AgentTeamMember } from '../../types/api'

// ── helpers ────────────────────────────────────────────────────────────────

function fmtTs(iso: string | null): string {
  if (!iso) return '—'
  try {
    return new Date(iso).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    })
  } catch {
    return iso
  }
}

function shortId(id: string | null | undefined, n = 8): string {
  if (!id) return '—'
  return id.length > n ? `${id.slice(0, n)}…` : id
}

// ── component ──────────────────────────────────────────────────────────────

export default function AgentsTab() {
  // Read initial selection from URL so a refresh / shared link restores
  // the open team + selected agent.
  const initialSelection = readAgentTeamSelection(
    typeof window !== 'undefined' ? window.location.search : '',
  )
  const [selectedSession, setSelectedSession] = useState<string | null>(
    initialSelection.session,
  )
  const [selectedAgent, setSelectedAgent] = useState<string | null>(
    initialSelection.agent,
  )

  // Reflect state changes back to the URL via replaceState so the back
  // button doesn't fill up with intermediate selections — the whole
  // tab navigation is treated as one history entry.
  useEffect(() => {
    if (typeof window === 'undefined') return
    const next = writeAgentTeamSelection(window.location.search, {
      session: selectedSession,
      agent: selectedAgent,
    })
    const url = new URL(window.location.href)
    // Preserve every non-(session|agent) query param the parent may
    // have set (e.g. tab=agents). Only swap our two keys.
    const merged = new URLSearchParams(url.search)
    if (selectedSession) merged.set('session', selectedSession)
    else merged.delete('session')
    if (selectedAgent) merged.set('agent', selectedAgent)
    else merged.delete('agent')
    const mergedSearch = merged.toString()
    const target = `${url.pathname}${mergedSearch ? `?${mergedSearch}` : ''}${url.hash}`
    const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
    if (target !== current) {
      window.history.replaceState({}, '', target)
    }
    // mark `next` as used so the lint pass keeps the helper imported.
    void next
  }, [selectedSession, selectedAgent])

  const teamsQuery = useQuery({
    queryKey: ['agent-teams', 'list'],
    queryFn: () => listAgentTeams(50),
  })

  const graphQuery = useQuery({
    queryKey: ['agent-teams', 'graph', selectedSession],
    queryFn: () => getAgentTeam(selectedSession!),
    enabled: !!selectedSession,
  })

  const teams = teamsQuery.data?.teams ?? []
  const graph = graphQuery.data ?? null

  // If the user opens the tab with an unknown ?session= param (or the
  // backing session was deleted between sessions), fall back to the
  // first available team rather than rendering an error pane.
  useEffect(() => {
    if (!selectedSession && teams.length > 0) {
      setSelectedSession(teams[0]!.session_id)
    }
  }, [teams, selectedSession])

  // Resolve the active agent (lead vs sub) from the graph + URL state.
  // When `selectedAgent === null`, the lead pane renders. When it points
  // at a sub-agent's session_id, the sub pane renders. Anything else
  // (stale id) falls back to the lead.
  const activeMember: AgentTeamMember | null = useMemo(() => {
    if (!graph) return null
    if (!selectedAgent) return graph.lead
    return graph.agents.find(a => a.session_id === selectedAgent) ?? graph.lead
  }, [graph, selectedAgent])

  // Keyboard nav (j/k or ↑/↓) between siblings in the agents list.
  const containerRef = useRef<HTMLDivElement | null>(null)
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>) => {
      if (!graph) return
      const all: AgentTeamMember[] = [graph.lead, ...graph.agents]
      const idx = all.findIndex(m =>
        selectedAgent ? m.session_id === selectedAgent : m.is_lead,
      )
      if (idx < 0) return
      let next = idx
      if (e.key === 'j' || e.key === 'ArrowDown') next = Math.min(all.length - 1, idx + 1)
      else if (e.key === 'k' || e.key === 'ArrowUp') next = Math.max(0, idx - 1)
      else return
      e.preventDefault()
      const target = all[next]!
      setSelectedAgent(target.is_lead ? null : target.session_id)
    },
    [graph, selectedAgent],
  )

  // ── render ───────────────────────────────────────────────────────────

  if (teamsQuery.isLoading) {
    return <LoadingSpinner message="Loading agent teams..." />
  }

  if (teamsQuery.error) {
    return (
      <div className="p-4 text-sm text-red-600 dark:text-red-400">
        Failed to load agent teams:{' '}
        {teamsQuery.error instanceof Error ? teamsQuery.error.message : 'Unknown error'}
      </div>
    )
  }

  if (teams.length === 0) {
    return (
      <EmptyState
        icon={<IconHierarchy3 size={40} />}
        title="No agent teams yet"
        description="When Claude Code spawns parallel sub-agents (via the TeamCreate tool), the dependency graph will show up here."
      />
    )
  }

  return (
    <div
      ref={containerRef}
      onKeyDown={handleKeyDown}
      tabIndex={0}
      className="grid grid-cols-1 lg:grid-cols-12 gap-4 outline-none"
      data-testid="agents-tab"
    >
      {/* Left rail — team picker + dependency tree */}
      <aside className="lg:col-span-5 xl:col-span-4 space-y-3">
        <div className="text-xs uppercase tracking-wider text-gray-500 dark:text-gray-400 px-1">
          Recent agent teams
        </div>
        <div className="space-y-1">
          {teams.map(t => {
            const isActive = t.session_id === selectedSession
            return (
              <button
                key={t.session_id}
                onClick={() => {
                  setSelectedSession(t.session_id)
                  setSelectedAgent(null)
                }}
                className={`w-full text-left px-3 py-2 rounded-md border transition-colors ${
                  isActive
                    ? 'border-indigo-400 bg-indigo-50 dark:bg-indigo-900/20'
                    : 'border-gray-200 dark:border-gray-800 hover:border-gray-300 dark:hover:border-gray-700'
                }`}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium text-sm text-gray-800 dark:text-gray-200 truncate">
                    {t.team_name ?? t.project_display_name}
                  </span>
                  <span className="text-xs text-gray-500 flex items-center gap-1 flex-shrink-0">
                    <IconRobot size={12} />
                    {t.agent_count}
                  </span>
                </div>
                <div className="text-xs text-gray-500 mt-0.5 truncate">
                  Lead {shortId(t.session_id)} · {fmtTs(t.last_ts)}
                </div>
                <div className="text-xs text-gray-500 mt-0.5">
                  {formatNumber(t.lead_message_count)} lead msgs ·{' '}
                  {formatNumber(t.sub_agent_message_count)} sub msgs
                </div>
              </button>
            )
          })}
        </div>

        {/* Dependency tree for the selected team */}
        {selectedSession && graph && (
          <div className="pt-2">
            <div className="text-xs uppercase tracking-wider text-gray-500 dark:text-gray-400 px-1">
              Agents ({graph.agents.length + 1})
            </div>
            <div className="mt-2 space-y-1" role="tree">
              <AgentRow
                member={graph.lead}
                isSelected={!selectedAgent}
                indent={0}
                onClick={() => setSelectedAgent(null)}
              />
              {graph.agents.map(agent => (
                <AgentRow
                  key={agent.session_id}
                  member={agent}
                  isSelected={selectedAgent === agent.session_id}
                  indent={1}
                  onClick={() => setSelectedAgent(agent.session_id)}
                />
              ))}
            </div>
          </div>
        )}
        {selectedSession && graphQuery.isLoading && (
          <div className="text-xs text-gray-500 px-1">Loading graph…</div>
        )}
      </aside>

      {/* Right pane — agent detail */}
      <section className="lg:col-span-7 xl:col-span-8">
        {!selectedSession ? (
          <EmptyState
            title="Pick a team on the left"
            description="Each team is a top-level Claude Code session that spawned parallel sub-agents."
          />
        ) : graphQuery.error ? (
          <div className="p-4 text-sm text-red-600 dark:text-red-400">
            Failed to load team graph:{' '}
            {graphQuery.error instanceof Error
              ? graphQuery.error.message
              : 'Unknown error'}
          </div>
        ) : !graph ? (
          <LoadingSpinner message="Loading dependency graph..." />
        ) : (
          <AgentDetailPane
            graph={graph}
            member={activeMember!}
            onOpenTranscript={() => {
              // Reuse the Sessions tab's deep-link contract: switch to the
              // sessions tab with ?session= pre-filled. The Sessions tab
              // already knows how to resolve a session_id to its file.
              if (typeof window === 'undefined') return
              const url = new URL(window.location.href)
              url.searchParams.set('tab', 'sessions')
              url.searchParams.set('session', activeMember!.session_id)
              window.history.pushState({}, '', `${url.pathname}${url.search}`)
              // Inform listeners (ProjectDashboard) so the tab swap takes
              // effect without a reload.
              window.dispatchEvent(
                new CustomEvent('stackunderflow:nav', {
                  detail: { tab: 'sessions', session: activeMember!.session_id },
                }),
              )
            }}
          />
        )}
      </section>
    </div>
  )
}

// ── sub-components ─────────────────────────────────────────────────────────

interface AgentRowProps {
  member: AgentTeamMember
  isSelected: boolean
  indent: number
  onClick: () => void
}

function AgentRow({ member, isSelected, indent, onClick }: AgentRowProps) {
  return (
    <button
      onClick={onClick}
      role="treeitem"
      aria-selected={isSelected}
      className={`w-full text-left flex items-center gap-2 px-3 py-2 rounded-md border transition-colors ${
        isSelected
          ? 'border-indigo-400 bg-indigo-50 dark:bg-indigo-900/20'
          : 'border-transparent hover:bg-gray-100 dark:hover:bg-gray-800/50'
      }`}
      style={{ paddingLeft: `${0.75 + indent * 1}rem` }}
      data-agent-id={member.session_id}
    >
      {indent > 0 && (
        <IconChevronRight size={12} className="text-gray-400 flex-shrink-0" />
      )}
      {member.is_lead ? (
        <IconUser size={14} className="text-indigo-500 flex-shrink-0" />
      ) : (
        <IconRobot size={14} className="text-gray-500 flex-shrink-0" />
      )}
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium text-gray-800 dark:text-gray-200 truncate">
          {member.is_lead ? 'team-lead' : member.agent_name ?? shortId(member.agent_id)}
        </div>
        <div className="text-xs text-gray-500 truncate">
          {formatNumber(member.message_count)} msgs · {formatCost(member.cost_usd)}
        </div>
      </div>
    </button>
  )
}

interface AgentDetailPaneProps {
  graph: { team_name: string | null; project_display_name: string }
  member: AgentTeamMember
  onOpenTranscript: () => void
}

function AgentDetailPane({ graph, member, onOpenTranscript }: AgentDetailPaneProps) {
  return (
    <div className="space-y-4">
      <header className="flex items-start justify-between gap-3 border-b border-gray-200 dark:border-gray-800 pb-3">
        <div className="min-w-0">
          <div className="text-xs uppercase tracking-wider text-gray-500">
            {graph.team_name ?? 'Untitled team'} · {graph.project_display_name}
          </div>
          <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-100 mt-0.5">
            {member.is_lead
              ? 'Team lead'
              : member.agent_name ?? shortId(member.agent_id)}
          </h2>
          <div className="text-xs text-gray-500 mt-0.5">
            session {shortId(member.session_id, 12)}
            {member.parent_session_id && (
              <> · spawned by {shortId(member.parent_session_id, 12)}</>
            )}
          </div>
        </div>
        <button
          onClick={onOpenTranscript}
          className="flex-shrink-0 text-xs px-3 py-1.5 rounded border border-gray-300 dark:border-gray-700 hover:border-gray-400 dark:hover:border-gray-600"
        >
          Open full transcript →
        </button>
      </header>

      <div className="grid grid-cols-2 sm:grid-cols-4 gap-2">
        <Stat
          icon={<IconMessage size={14} />}
          label="Messages"
          value={formatNumber(member.message_count)}
        />
        <Stat
          icon={<IconHash size={14} />}
          label="Model"
          value={member.model ? formatModelName(member.model) : '—'}
        />
        <Stat
          icon={<IconClock size={14} />}
          label="First seen"
          value={fmtTs(member.first_ts)}
        />
        <Stat
          icon={<IconClock size={14} />}
          label="Last seen"
          value={fmtTs(member.last_ts)}
        />
      </div>

      <div className="rounded-md border border-gray-200 dark:border-gray-800 p-3">
        <div className="text-xs uppercase tracking-wider text-gray-500 mb-1">
          First user prompt
        </div>
        <div className="text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap break-words">
          {member.first_user_prompt ?? <span className="italic text-gray-500">(no user message recorded)</span>}
        </div>
      </div>

      <div className="rounded-md border border-gray-200 dark:border-gray-800 p-3">
        <div className="text-xs uppercase tracking-wider text-gray-500 mb-1">
          Cost
        </div>
        <div className="text-sm text-gray-800 dark:text-gray-200">
          {formatCost(member.cost_usd)} (computed from per-message tokens)
        </div>
      </div>
    </div>
  )
}

interface StatProps {
  icon: React.ReactNode
  label: string
  value: string
}

function Stat({ icon, label, value }: StatProps) {
  return (
    <div className="rounded-md border border-gray-200 dark:border-gray-800 px-3 py-2">
      <div className="flex items-center gap-1.5 text-xs text-gray-500">
        {icon}
        {label}
      </div>
      <div className="text-sm font-medium text-gray-800 dark:text-gray-200 mt-0.5 truncate">
        {value}
      </div>
    </div>
  )
}

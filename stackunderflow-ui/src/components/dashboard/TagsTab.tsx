import { useState } from 'react';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  IconArrowLeft,
  IconRefresh,
  IconSearch,
  IconTag,
  IconHash,
  IconUsers,
} from '@tabler/icons-react';
import { getTagCloud, browseTag, reindexTags } from '../../services/api';
import LoadingSpinner from '../common/LoadingSpinner';
import EmptyState from '../common/EmptyState';

type View = { kind: 'cloud' } | { kind: 'browse'; tag: string };

function getTagSizeClass(count: number, maxCount: number): string {
  if (maxCount === 0) return 'text-xs';
  const ratio = count / maxCount;
  if (ratio > 0.8) return 'text-xl';
  if (ratio > 0.6) return 'text-lg';
  if (ratio > 0.4) return 'text-base';
  if (ratio > 0.2) return 'text-sm';
  return 'text-xs';
}

const CATEGORY_CLASSES: Record<string, string> = {
  intent: 'bg-amber-500/20 text-amber-300 border-amber-500/30 hover:bg-amber-500/30',
  language: 'bg-blue-500/20 text-blue-300 border-blue-500/30 hover:bg-blue-500/30',
  framework: 'bg-emerald-500/20 text-emerald-300 border-emerald-500/30 hover:bg-emerald-500/30',
  tool: 'bg-purple-500/20 text-purple-300 border-purple-500/30 hover:bg-purple-500/30',
  topic: 'bg-indigo-500/20 text-indigo-300 border-indigo-500/30 hover:bg-indigo-500/30',
}

const DEFAULT_TAG_CLASSES = 'bg-indigo-500/20 text-indigo-300 border-indigo-500/30 hover:bg-indigo-500/30'

function tagColorClasses(category: string): string {
  return CATEGORY_CLASSES[category] ?? DEFAULT_TAG_CLASSES
}

const LEGEND: Array<{ category: string; label: string; dot: string }> = [
  { category: 'intent', label: 'Intent', dot: 'bg-amber-500' },
  { category: 'language', label: 'Language', dot: 'bg-blue-500' },
  { category: 'framework', label: 'Framework', dot: 'bg-emerald-500' },
  { category: 'topic', label: 'Topic', dot: 'bg-indigo-500' },
  { category: 'tool', label: 'Tool', dot: 'bg-purple-500' },
]

function formatTimestamp(ts: string): string {
  try {
    const d = new Date(ts);
    return d.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      year: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return ts;
  }
}

export default function TagsTab() {
  const queryClient = useQueryClient();
  const [view, setView] = useState<View>({ kind: 'cloud' });
  const [search, setSearch] = useState('');

  // ── Tag Cloud Query ──
  const cloudQuery = useQuery({
    queryKey: ['tagCloud'],
    queryFn: getTagCloud,
  });

  // ── Tag Browse Query ──
  const selectedTag = view.kind === 'browse' ? view.tag : null;
  const browseQuery = useQuery({
    queryKey: ['tagBrowse', selectedTag],
    queryFn: () => browseTag(selectedTag!),
    enabled: !!selectedTag,
  });

  // ── Reindex Mutation ──
  const reindexMutation = useMutation({
    mutationFn: reindexTags,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['tagCloud'] });
    },
  });

  // ── Cloud View ──
  if (view.kind === 'cloud') {
    if (cloudQuery.isLoading) return <LoadingSpinner message="Loading tags..." />;
    if (cloudQuery.isError)
      return (
        <div className="text-red-400 p-4">
          Failed to load tags: {(cloudQuery.error as Error).message}
        </div>
      );

    const data = cloudQuery.data!;
    const maxCount = data.tags.reduce((m, t) => Math.max(m, t.count), 0);

    const filtered = data.tags.filter((t) =>
      t.name.toLowerCase().includes(search.toLowerCase()),
    );

    return (
      <div className="space-y-6">
        {/* Header */}
        <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
          <div className="flex items-center gap-6">
            <div className="flex items-center gap-2 text-sm text-zinc-400">
              <IconHash size={16} />
              <span>
                <span className="text-zinc-100 font-medium">{data.tags.length}</span> tags
              </span>
            </div>
            <div className="flex items-center gap-2 text-sm text-zinc-400">
              <IconUsers size={16} />
              <span>
                <span className="text-zinc-100 font-medium">{data.total_sessions}</span>{' '}
                sessions
              </span>
            </div>
          </div>

          <div className="flex items-center gap-3">
            {/* Search */}
            <div className="relative">
              <IconSearch
                size={16}
                className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-500"
              />
              <input
                type="text"
                placeholder="Filter tags..."
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                className="pl-9 pr-3 py-1.5 bg-zinc-800 border border-zinc-700 rounded-lg text-sm text-zinc-200 placeholder-zinc-500 focus:outline-none focus:border-zinc-500 w-52"
              />
            </div>

            {/* Reindex */}
            <button
              onClick={() => reindexMutation.mutate()}
              disabled={reindexMutation.isPending}
              className="flex items-center gap-1.5 px-3 py-1.5 bg-zinc-800 border border-zinc-700 rounded-lg text-sm text-zinc-300 hover:bg-zinc-700 disabled:opacity-50 transition-colors"
            >
              <IconRefresh
                size={16}
                className={reindexMutation.isPending ? 'animate-spin' : ''}
              />
              {reindexMutation.isPending ? 'Reindexing...' : 'Reindex'}
            </button>
          </div>
        </div>

        {/* Legend */}
        <div className="flex items-center gap-4 text-xs text-zinc-500 flex-wrap">
          {LEGEND.map((l) => (
            <span key={l.category} className="flex items-center gap-1.5">
              <span className={`w-2.5 h-2.5 rounded-full ${l.dot}`} />
              {l.label}
            </span>
          ))}
        </div>

        {/* Tag Cloud */}
        {filtered.length === 0 ? (
          <EmptyState
            title="No tags found"
            description={search ? 'Try a different filter.' : 'No tags available yet.'}
          />
        ) : (
          <div className="flex flex-wrap gap-2 p-4 bg-zinc-900/50 border border-zinc-800 rounded-xl">
            {filtered.map((tag) => (
              <button
                key={tag.name}
                onClick={() => setView({ kind: 'browse', tag: tag.name })}
                className={`inline-flex items-center gap-1 px-2.5 py-1 border rounded-full cursor-pointer transition-colors ${tagColorClasses(tag.category)} ${getTagSizeClass(tag.count, maxCount)}`}
              >
                <IconTag size={12} />
                {tag.name}
                <span className="text-[10px] opacity-70 ml-0.5">({tag.count})</span>
              </button>
            ))}
          </div>
        )}

        {reindexMutation.isSuccess && (
          <div className="text-green-400 text-sm">Reindex completed successfully.</div>
        )}
        {reindexMutation.isError && (
          <div className="text-red-400 text-sm">
            Reindex failed: {(reindexMutation.error as Error).message}
          </div>
        )}
      </div>
    );
  }

  // ── Browse View ──
  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center gap-3">
        <button
          onClick={() => setView({ kind: 'cloud' })}
          className="flex items-center gap-1.5 px-3 py-1.5 bg-zinc-800 border border-zinc-700 rounded-lg text-sm text-zinc-300 hover:bg-zinc-700 transition-colors"
        >
          <IconArrowLeft size={16} />
          Back
        </button>
        <h3 className="text-lg font-medium text-zinc-100 flex items-center gap-2">
          <IconTag size={20} />
          {view.tag}
        </h3>
      </div>

      {/* Sessions */}
      {browseQuery.isLoading && <LoadingSpinner message="Loading sessions..." />}
      {browseQuery.isError && (
        <div className="text-red-400 p-4">
          Failed to load sessions: {(browseQuery.error as Error).message}
        </div>
      )}
      {browseQuery.isSuccess && (
        <>
          <p className="text-sm text-zinc-400">
            <span className="text-zinc-100 font-medium">{browseQuery.data.count}</span>{' '}
            session{browseQuery.data.count !== 1 && 's'} tagged with{' '}
            <span className="text-indigo-300 font-medium">{browseQuery.data.tag}</span>
          </p>

          {browseQuery.data.sessions.length === 0 ? (
            <EmptyState title="No sessions" description="No sessions have this tag." />
          ) : (
            <div className="space-y-2">
              {browseQuery.data.sessions.map((session) => (
                <div
                  key={session.session_id}
                  className="p-4 bg-zinc-900/60 border border-zinc-800 rounded-lg hover:border-zinc-700 transition-colors"
                >
                  <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-2">
                    <span className="text-sm font-mono text-zinc-200 truncate">
                      {session.session_id}
                    </span>
                    <div className="flex items-center gap-2 text-xs text-zinc-500">
                      {session.source.map((s) => (
                        <span key={s} className="px-2 py-0.5 bg-zinc-800 rounded text-zinc-400">
                          {s}
                        </span>
                      ))}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}

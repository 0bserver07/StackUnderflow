import { IconRefresh } from '@tabler/icons-react'
import type { OllamaModel } from '../../types/chat'

interface ModelSelectorProps {
  models: OllamaModel[]
  currentModel: string
  onSelectModel: (model: string) => void
  onRefresh: () => void
}

function formatSize(bytes: number): string {
  const gb = bytes / (1024 * 1024 * 1024)
  if (gb >= 1) return `${gb.toFixed(1)} GB`
  const mb = bytes / (1024 * 1024)
  return `${mb.toFixed(0)} MB`
}

export default function ModelSelector({ models, currentModel, onSelectModel, onRefresh }: ModelSelectorProps) {
  return (
    <div className="flex items-center gap-2 px-3 py-2 border-b border-gray-200 dark:border-gray-800">
      <label className="text-xs text-gray-500 shrink-0">Model:</label>
      <select
        value={currentModel}
        onChange={(e) => onSelectModel(e.target.value)}
        className="flex-1 bg-white dark:bg-gray-800 text-sm text-gray-700 dark:text-gray-300 border border-gray-200 dark:border-gray-700 rounded px-2 py-1 focus:outline-none focus:border-blue-500 min-w-0"
      >
        {models.length === 0 && (
          <option value="">No models available</option>
        )}
        {models.map((model) => (
          <option key={model.name} value={model.name}>
            {model.name} ({formatSize(model.size)})
          </option>
        ))}
      </select>
      <button
        onClick={onRefresh}
        className="p-1 text-gray-500 hover:text-gray-700 dark:hover:text-gray-300 transition-colors"
        title="Refresh model list"
      >
        <IconRefresh size={16} />
      </button>
    </div>
  )
}

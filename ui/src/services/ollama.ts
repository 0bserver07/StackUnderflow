import type { OllamaChatRequest, OllamaChatResponse, OllamaModel } from '../types/chat'

function getApiUrl(path: string): string {
  // In both dev and production, use the proxy path
  return `/ollama-api${path}`
}

export const ollamaApi = {
  async chat(
    request: OllamaChatRequest,
    onChunk: (chunk: OllamaChatResponse) => void,
    signal?: AbortSignal
  ): Promise<void> {
    const response = await fetch(getApiUrl('/chat'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ...request, stream: true }),
      signal,
    })

    if (!response.ok) {
      const errorText = await response.text().catch(() => '')
      throw new Error(`Chat failed: ${response.status} ${response.statusText}${errorText ? ` - ${errorText}` : ''}`)
    }

    const reader = response.body?.getReader()
    if (!reader) throw new Error('No response body')

    const decoder = new TextDecoder()
    let buffer = ''

    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) break

        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop() || ''

        for (const line of lines) {
          if (line.trim()) {
            try {
              const chunk: OllamaChatResponse = JSON.parse(line)
              onChunk(chunk)
            } catch {
              // Continue on parse errors
            }
          }
        }
      }

      if (buffer.trim()) {
        try {
          const chunk: OllamaChatResponse = JSON.parse(buffer)
          onChunk(chunk)
        } catch {
          // Ignore final parse errors
        }
      }
    } finally {
      reader.releaseLock()
    }
  },

  async listModels(): Promise<OllamaModel[]> {
    try {
      const response = await fetch(getApiUrl('/tags'))
      if (!response.ok) {
        return []
      }
      const data = await response.json()
      return data.models || []
    } catch {
      // Ollama not running — return empty list silently
      return []
    }
  },
}

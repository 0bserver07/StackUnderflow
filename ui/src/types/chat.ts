export type ChatRole = 'user' | 'assistant' | 'system'

export interface ChatMessage {
  id: string
  role: ChatRole
  content: string
  timestamp: Date
}

export interface ChatSession {
  id: string
  contextId: string
  contextLabel: string
  createdAt: Date
  updatedAt: Date
  messages: ChatMessage[]
}

export interface OllamaModel {
  name: string
  modified_at: string
  size: number
}

export interface OllamaChatMessage {
  role: ChatRole
  content: string
}

export interface OllamaChatRequest {
  model: string
  messages: OllamaChatMessage[]
  stream?: boolean
}

export interface OllamaChatPartResponse {
  model: string
  created_at: string
  message: OllamaChatMessage
  done: false
}

export interface OllamaChatCompletedResponse {
  model: string
  created_at: string
  message: OllamaChatMessage
  done: true
  total_duration: number
  eval_count: number
  prompt_eval_count: number
}

export type OllamaChatResponse = OllamaChatPartResponse | OllamaChatCompletedResponse

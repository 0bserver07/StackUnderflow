// Run with: node --test tests/services/format.test.ts
// (Node 22+ strips TypeScript types automatically; no test runner dep needed.)

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { formatModelName } from '../../src/services/format.ts'

// ---------------------------------------------------------------------------
// Each row in the v0.6.1+ spec table for `formatModelName()`.
// ---------------------------------------------------------------------------

const cases: Array<[input: string, expected: string]> = [
  // Anthropic native (claude-<family>-<major>-<minor>[-YYYYMMDD])
  ['claude-opus-4-7', 'Opus 4.7'],
  ['claude-opus-4-6', 'Opus 4.6'],
  ['claude-opus-4-5-20251101', 'Opus 4.5'],
  ['claude-sonnet-4-6', 'Sonnet 4.6'],
  ['claude-sonnet-4-5-20250929', 'Sonnet 4.5'],
  ['claude-haiku-4-5-20251001', 'Haiku 4.5'],

  // Cursor's claude rephrase (claude-<version>-<family>[-suffix])
  ['claude-4.5-sonnet-thinking', 'Sonnet 4.5 (thinking)'],

  // OpenAI / Codex
  ['gpt-5-codex', 'GPT-5 Codex'],
  ['gpt-5', 'GPT-5'],

  // Cursor / Cline auto-pickers and Composer
  ['cursor-auto', 'Cursor Auto'],
  ['cline-auto', 'Cline Auto'],
  ['composer-1', 'Composer 1'],

  // Gemini
  ['gemini-2.5-pro', 'Gemini 2.5 Pro'],
  ['gemini-2.5-pro-preview-05-06', 'Gemini 2.5 Pro Preview'],
  ['gemini-3-pro-preview', 'Gemini 3 Pro Preview'],
  ['gemini-3-flash-preview', 'Gemini 3 Flash Preview'],
  ['gemini-3.1-pro-preview', 'Gemini 3.1 Pro Preview'],
  ['gemini-2.5-flash', 'Gemini 2.5 Flash'],

  // Zhipu GLM
  ['glm-5', 'GLM 5'],
  ['glm-5.1', 'GLM 5.1'],

  // Defensive passthroughs — never throw, never invent.
  ['<synthetic>', '<synthetic>'],
  ['something-unknown-xyz', 'something-unknown-xyz'],
]

for (const [input, expected] of cases) {
  test(`formatModelName(${JSON.stringify(input)}) === ${JSON.stringify(expected)}`, () => {
    assert.equal(formatModelName(input), expected)
  })
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

test('empty string returns empty string', () => {
  assert.equal(formatModelName(''), '')
})

test('idempotent on already-pretty output (no double-format hazard)', () => {
  // The function is a one-way prettifier — feeding its own output back in
  // should not blow up; it should pass through unchanged via the fallback.
  const pretty = formatModelName('claude-opus-4-7')
  assert.equal(formatModelName(pretty), pretty)
})

test('cursor fast and unknown composer numbers', () => {
  assert.equal(formatModelName('cursor-fast'), 'Cursor Fast')
  assert.equal(formatModelName('composer-2'), 'Composer 2')
})

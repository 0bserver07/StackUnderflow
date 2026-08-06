// Run with: node --test tests/services/cost-data-query.test.ts
// (Node 22+ strips TypeScript types automatically; no test runner dep needed.)

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { buildCostDataQuery } from '../../src/services/api.ts'

// ---------------------------------------------------------------------------
// buildCostDataQuery — the /api/cost-data param contract (#57 models, #24 range)
// ---------------------------------------------------------------------------

test('no filters → empty string (URL stays bare for the common case)', () => {
  assert.equal(buildCostDataQuery([]), '')
  assert.equal(buildCostDataQuery([], 'all'), '')
  assert.equal(buildCostDataQuery([], undefined), '')
})

test('#57: models become repeated, lowercased, trimmed model= params', () => {
  assert.equal(buildCostDataQuery(['Claude-Opus-4-8']), '?model=claude-opus-4-8')
  assert.equal(
    buildCostDataQuery(['opus-4-8', '  Haiku-4-5  ']),
    '?model=opus-4-8&model=haiku-4-5',
  )
})

test('#57: blank / whitespace-only model entries are dropped', () => {
  assert.equal(buildCostDataQuery(['', '   ']), '')
  assert.equal(buildCostDataQuery(['', 'opus-4-8']), '?model=opus-4-8')
})

test('#24: windowed ranges emit range=; the all default is omitted', () => {
  assert.equal(buildCostDataQuery([], '7d'), '?range=7d')
  assert.equal(buildCostDataQuery([], '30d'), '?range=30d')
  assert.equal(buildCostDataQuery([], 'all'), '')
})

test('#24 + #57: models and range compose', () => {
  assert.equal(
    buildCostDataQuery(['opus-4-8'], '7d'),
    '?model=opus-4-8&range=7d',
  )
})

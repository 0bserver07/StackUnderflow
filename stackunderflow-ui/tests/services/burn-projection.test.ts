// Run with: node --test tests/services/burn-projection.test.ts
//
// Burn-projector v2 type contract — locks the shape of `PlanProjection`
// the dashboard reads from `/api/plan`. The tests don't exercise the
// React component (jsdom-based component tests live elsewhere); they
// pin the type-discriminator strings + the optional `projection` field
// so the backend can't silently change the wire format without the
// type-check breaking on a known surface.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import type { PlanProjection, PlanResponse } from '../../src/types/api.ts'

// ── projection-method enum ──────────────────────────────────────────────────

test('PlanProjection.projection_method allows linear and weighted-7d', () => {
  const linear: PlanProjection['projection_method'] = 'linear'
  const weighted: PlanProjection['projection_method'] = 'weighted-7d'
  assert.equal(linear, 'linear')
  assert.equal(weighted, 'weighted-7d')
})

// ── shape: required vs nullable fields ─────────────────────────────────────

test('PlanProjection accepts a fully-populated forecast', () => {
  const sample: PlanProjection = {
    projected_month_end_usd: 42.5,
    projection_method: 'weighted-7d',
    daily_burn_usd: 1.42,
    days_to_limit: 7,
    thresholds: [50, 75, 90],
    crossed_threshold: 75,
    alert: 'Crossed 75% of plan budget',
  }
  assert.equal(sample.projection_method, 'weighted-7d')
  assert.equal(sample.crossed_threshold, 75)
  assert.equal(sample.thresholds.length, 3)
  assert.equal(sample.alert, 'Crossed 75% of plan budget')
})

test('PlanProjection accepts no-alert/no-crossing nulls', () => {
  // Quiet path: no alert and no threshold crossed.
  const sample: PlanProjection = {
    projected_month_end_usd: 5.0,
    projection_method: 'linear',
    daily_burn_usd: 0.0,
    days_to_limit: null,
    thresholds: [50, 75, 90],
    crossed_threshold: null,
    alert: null,
  }
  assert.equal(sample.alert, null)
  assert.equal(sample.crossed_threshold, null)
  assert.equal(sample.days_to_limit, null)
})

// ── PlanResponse stays backward-compatible ─────────────────────────────────

test('PlanResponse.projection is optional (older servers omit it)', () => {
  // Older server response (pre-burn-projector-v2) — no `projection` key.
  const legacy: PlanResponse = {
    plan: { name: 'claude-pro', monthly_usd: 20, reset_day: 1 },
    usage: {
      used: 5,
      budget: 20,
      remaining: 15,
      pct: 25,
      projected: 25,
      status: 'ok',
      period_start: '2026-05-01',
      period_end: '2026-05-31',
      days_so_far: 15,
      days_in_period: 31,
    },
  }
  // Reading `legacy.projection` as undefined (optional field) is the test.
  assert.equal(legacy.projection, undefined)

  // New server response — `projection` is present and shaped.
  const fresh: PlanResponse = {
    ...legacy,
    projection: {
      projected_month_end_usd: 25,
      projection_method: 'weighted-7d',
      daily_burn_usd: 1,
      days_to_limit: null,
      thresholds: [50, 75, 90],
      crossed_threshold: null,
      alert: null,
    },
  }
  assert.equal(fresh.projection?.projection_method, 'weighted-7d')
})

test('PlanResponse.projection can be explicitly null (no plan set)', () => {
  // The "no plan configured" branch returns nulls everywhere.
  const empty: PlanResponse = {
    plan: null,
    usage: null,
    projection: null,
  }
  assert.equal(empty.projection, null)
})

// ── threshold list semantics ───────────────────────────────────────────────

test('PlanProjection.thresholds round-trips integer percentages', () => {
  const sample: PlanProjection = {
    projected_month_end_usd: 0,
    projection_method: 'linear',
    daily_burn_usd: 0,
    days_to_limit: null,
    thresholds: [60, 80, 95],
    crossed_threshold: null,
    alert: null,
  }
  assert.deepEqual(sample.thresholds, [60, 80, 95])
})

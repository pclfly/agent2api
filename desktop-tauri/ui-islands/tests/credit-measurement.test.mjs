import test from 'node:test'
import assert from 'node:assert/strict'
import { creditReading } from '../src/islands/credit-measurement.ts'

test('unknown is distinct from measured zero', () => {
  assert.equal(creditReading({}).text, '—')
  assert.equal(creditReading({ credit: null }).text, '—')
  assert.equal(creditReading({ credit: NaN }).text, '—')
  assert.equal(creditReading({ credit: 0, creditRequests: 2, success: 2 }).text, '0')
})
test('estimated and partial totals are visibly marked', () => {
  assert.equal(creditReading({ credit: 0.123456, creditEstimated: true, creditRequests: 1, success: 3 }).text, '≈0.123456*')
  assert.equal(creditReading({ credit: 12.5, creditRequests: 3, success: 3 }).text, '12.5')
  assert.equal(creditReading({ credit: 100, creditRequests: 1, success: 1 }).text, '100')
  assert.equal(creditReading({ credit: 1 }).partial, true)
})

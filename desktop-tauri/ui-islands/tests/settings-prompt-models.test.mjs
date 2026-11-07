import test from 'node:test'
import assert from 'node:assert/strict'
import { readPromptModelScope, promptModelOptions, isPromptModelSelected, togglePromptModel } from '../src/islands/settings-prompt-models.ts'

test('old configs default to all including new models', () => {
  assert.equal(readPromptModelScope(undefined), null)
  assert.equal(readPromptModelScope(null), null)
  assert.equal(isPromptModelSelected(null, 'new-model'), true)
})

test('empty scope selects none, malformed data does not expand access', () => {
  assert.deepEqual(readPromptModelScope([]), [])
  assert.deepEqual(readPromptModelScope('all'), [])
  assert.deepEqual(readPromptModelScope([42]), [])
  assert.equal(isPromptModelSelected([], 'model-a'), false)
})

test('deselecting one from all freezes an explicit whitelist', () => {
  const picked = togglePromptModel(null, ['model-a', 'model-b'], 'model-a', false)
  assert.deepEqual(picked, ['model-b'])
  assert.equal(isPromptModelSelected(picked, 'new-model'), false)
  assert.deepEqual(togglePromptModel(picked, ['model-a', 'model-b'], 'model-a', true), ['model-b', 'model-a'])
})

test('saved unknown selections survive a catalog refresh and search', () => {
  const picked = ['missing-model', 'model-a']
  const options = promptModelOptions(['model-a', 'model-b'], picked)
  assert.deepEqual(options, ['model-a', 'model-b', 'missing-model'])
  const filtered = options.filter(id => id.includes('model-b'))
  const next = togglePromptModel(picked, options, filtered[0], true)
  assert.deepEqual(next, ['missing-model', 'model-a', 'model-b'])
  assert.deepEqual(picked, ['missing-model', 'model-a'])
})

test('IDs are trimmed and case-insensitively deduplicated', () => {
  assert.deepEqual(readPromptModelScope([' A ', 'a', 'B']), ['A', 'B'])
  assert.equal(isPromptModelSelected(['A'], 'a'), true)
  assert.deepEqual(promptModelOptions(['A', 'a'], ['a']), ['A'])
})

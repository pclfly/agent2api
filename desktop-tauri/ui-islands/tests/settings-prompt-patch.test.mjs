import test from 'node:test'
import assert from 'node:assert/strict'
import { providerPatch } from '../src/islands/settings-prompt-patch.ts'

const globalPrompt = { mode: 'append', file: '', text: 'Global engineering instructions', source: 'inline' }

test('configured provider keeps its inline text when mode or file changes', () => {
  const item = { mode: 'custom', file: '', text: 'Provider-specific engineering instructions', source: 'inline', configured: true }
  assert.equal(providerPatch(globalPrompt, item).promptText, item.text)
  assert.equal({ ...providerPatch(globalPrompt, item), promptMode: 'append' }.promptText, item.text)
  assert.equal({ ...providerPatch(globalPrompt, item), promptFile: 'other.md' }.promptText, item.text)
})

test('passthrough retains stored inline text for a later mode switch', () => {
  const item = { mode: 'passthrough', file: '', text: 'Saved but inactive text', source: 'none', configured: true }
  assert.equal(providerPatch(globalPrompt, item).promptText, item.text)
  const global = { ...item }
  assert.equal(providerPatch(global, { ...item, configured: false }).promptText, item.text)
})

test('inherited inline text is copied only when creating an override', () => {
  const item = { ...globalPrompt, configured: false }
  assert.deepEqual(providerPatch(globalPrompt, item), {
    promptMode: 'append', promptFile: '', promptText: globalPrompt.text,
  })
})

test('file and builtin text do not become inline overrides', () => {
  for (const source of ['file', 'builtin']) {
    for (const configured of [true, false]) {
      const item = { mode: 'custom', file: source === 'file' ? 'prompt.md' : '', text: 'Resolved display text', source, configured }
      assert.equal(providerPatch(item, item).promptText, '')
      assert.equal(providerPatch(item, item).promptFile, item.file)
    }
  }
})

test('explicit clearing still overrides the preserved text', () => {
  const item = { ...globalPrompt, configured: true }
  assert.equal({ ...providerPatch(globalPrompt, item), promptText: '' }.promptText, '')
})

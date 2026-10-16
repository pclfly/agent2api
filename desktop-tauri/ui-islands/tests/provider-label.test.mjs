import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'
import { PROVIDER_LABELS_CHANGED, providerOptionsForDisplay, resolveProviderLabel } from '../src/islands/provider-label.ts'

const id = 'custom-4ac4467f6bd8'

test('raw ID and blank backend labels never override the directory name', () => {
  const lookup = key => key === id ? 'MixAI' : key
  for (const label of [id, '', '  ', undefined, 'Old MixAI']) {
    assert.equal(resolveProviderLabel(id, label, lookup), 'MixAI')
  }
  assert.equal(resolveProviderLabel(id, 'Backend MixAI', key => key), 'Backend MixAI')
  assert.equal(resolveProviderLabel(id, id), id)
  assert.equal(resolveProviderLabel('', 'orphan label', lookup), '')
  assert.equal(resolveProviderLabel('workbuddy', '', () => 'WorkBuddy'), 'WorkBuddy')
})

test('selected missing provider keeps its ID while obtaining its display name', () => {
  const original = [{ value: 'workbuddy', label: 'WorkBuddy' }]
  const displayed = providerOptionsForDisplay(original, id, key => key === id ? 'MixAI' : key)
  assert.deepEqual(displayed, [{ value: id, label: 'MixAI' }, ...original])
  assert.deepEqual(original, [{ value: 'workbuddy', label: 'WorkBuddy' }])
  assert.equal(providerOptionsForDisplay([{ value: id, label: id }], id, () => 'MixAI').length, 1)
})

test('directory arrival, rename and deletion notify cached request views without log refetches', async () => {
  const source = await fs.readFile(new URL('../../ui/providers.js', import.meta.url), 'utf8')
  let complete
  let list = null
  const calls = []
  const window = new EventTarget()
  window.wbApp = { getState: () => ({ accounts: { providers: [{ id: 'workbuddy', label: 'WorkBuddy', count: 0 }] } }) }
  window.__TAURI_INTERNALS__ = { invoke: async (_, { request }) => {
    calls.push(request.path)
    if (list) return { providers: list }
    return new Promise(resolve => { complete = resolve })
  } }
  vm.runInNewContext(source, { window, Event, console, workbuddyDesktop: {} })
  const snapshots = []
  const lookup = key => window.wbProviders.labelOf(key)
  const repaint = () => snapshots.push(providerOptionsForDisplay([{ value: id, label: id }], id, lookup)[0])
  window.addEventListener(PROVIDER_LABELS_CHANGED, repaint)
  assert.equal(resolveProviderLabel(id, id, lookup), id)
  complete({ providers: [{ id, name: 'MixAI' }] })
  await window.wbProviders.refreshCustom()
  assert.equal(resolveProviderLabel('workbuddy', '', lookup), 'WorkBuddy')
  assert.deepEqual(snapshots.at(-1), { value: id, label: 'MixAI' })
  list = [{ id, name: 'MixAI renamed' }]
  await window.wbProviders.refreshCustom()
  assert.deepEqual(snapshots.at(-1), { value: id, label: 'MixAI renamed' })
  await window.wbProviders.refreshCustom()
  assert.equal(snapshots.length, 2, 'unchanged names must not generate spurious renders')
  list = []
  await window.wbProviders.refreshCustom()
  assert.deepEqual(snapshots.at(-1), { value: id, label: id })
  assert.ok(calls.every(path => path === '/api/custom-providers'))
  window.removeEventListener(PROVIDER_LABELS_CHANGED, repaint)
})

test('builtin fallback and main-state arrival notify only effective name changes', async () => {
  const source = await fs.readFile(new URL('../../ui/providers.js', import.meta.url), 'utf8')
  let state = null
  let finish
  let stateCalls = 0
  const window = new EventTarget()
  window.wbApp = { getState: () => state }
  window.__TAURI_INTERNALS__ = { invoke: async () => ({ providers: [] }) }
  const workbuddyDesktop = { getState: () => {
    stateCalls++
    return new Promise(resolve => { finish = resolve })
  } }
  vm.runInNewContext(source, { window, Event, console, workbuddyDesktop })
  const snapshots = []
  window.addEventListener(PROVIDER_LABELS_CHANGED, () => snapshots.push(window.wbProviders.labelOf('qwenwork')))
  const loading = window.wbProviders.load()
  assert.equal(window.wbProviders.labelOf('qwenwork'), 'qwenwork')
  finish({ accounts: { providers: [{ id: 'qwenwork', label: 'QwenWork', count: 0 }] } })
  await loading
  assert.deepEqual(snapshots, ['QwenWork'])
  state = { accounts: { providers: [{ id: 'qwenwork', label: 'QwenWork updated', count: 1 }] } }
  window.wbProviders.syncFromAppState()
  assert.deepEqual(snapshots, ['QwenWork', 'QwenWork updated'])
  state.accounts.providers[0].count = 9
  await window.wbProviders.load()
  assert.equal(snapshots.length, 2, 'counts and identical names do not notify')
  assert.equal(stateCalls, 1, 'main-state updates do not fetch logs or refetch state')
  const app = await fs.readFile(new URL('../../ui/app.js', import.meta.url), 'utf8')
  assert.match(app, /state = next;\s*window\.wbProviders\?\.syncFromAppState\?\.\(\)/)
  assert.match(app, /state = \{ \.\.\.\(state \|\| \{\}\), \.\.\.next \};\s*window\.wbProviders\?\.syncFromAppState\?\.\(\)/)
})

test('request target uses provider first and accountName second, with no storage migration', async () => {
  const source = await fs.readFile(new URL('../src/islands/requests-page.tsx', import.meta.url), 'utf8')
  const target = source.slice(source.indexOf("case 'target':"), source.indexOf("case 'retry':"))
  assert.ok(target.includes('providerLabelOf(id, fromBackend)'))
  assert.ok(target.indexOf("className='req-provider'") < target.indexOf("className='req-acct'"))
  assert.ok(target.includes('{entry.accountName}</span>'))
  const fixture = { provider: id, providerLabel: id, accountName: 'MixAI-gpt' }
  assert.deepEqual([resolveProviderLabel(fixture.provider, fixture.providerLabel, () => 'MixAI'), fixture.accountName], ['MixAI', 'MixAI-gpt'])
})

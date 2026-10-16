import test from 'node:test'
import assert from 'node:assert/strict'
import vm from 'node:vm'
import fs from 'node:fs/promises'
import { build } from 'esbuild'

async function load(name, window = {}) {
  const result = await build({
    entryPoints: [new URL(`../src/islands/${name}.ts`, import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')],
    bundle: true, write: false, format: 'iife', globalName: 'module', platform: 'browser',
  })
  const context = { window, console, setTimeout: () => 0 }
  vm.runInNewContext(result.outputFiles[0].text, context)
  return context.module
}

test('proxy effective state does not masquerade as upstream disable', async () => {
  const state = await load('proxies-state')
  const base = { source: 'clash', enabled: false, usable: false }
  assert.equal(state.isMirrorOffItem({ ...base, sourceEnabled: true, localDisabled: true }), false)
  assert.equal(state.isMirrorOffItem({ ...base, sourceEnabled: true, disabled: true }), false)
  assert.equal(state.isMirrorOffItem({ ...base, sourceEnabled: false, localDisabled: true }), true)
  assert.match(state.enableHint({ ...base, sourceEnabled: false, localDisabled: true }), /Clash Verge/)
  assert.match(state.enableHint({ ...base, sourceEnabled: true, disabled: true }), /本页禁用/)
  assert.equal(state.isDisabledSubscriptionItem({ ...base, source: 'subscription' }), true)
})

test('Web update controls require explicit self-update capability, desktop remains supported', async () => {
  const window = { workbuddyDesktop: { platform: 'web' } }
  const update = await load('update-shared', window)
  for (const info of [null, {}, { selfUpdate: false }, { selfUpdate: 'true' }]) {
    assert.equal(update.canManageUpdate(info), false)
  }
  assert.equal(update.canManageUpdate({ selfUpdate: true }), true)
  window.workbuddyDesktop.platform = 'windows'
  assert.equal(update.canManageUpdate({ selfUpdate: false }), true)
})

test('model rail counts use supplied data and keep edition selections', async () => {
  const model = await load('models-panel-state', { wbProviders: { all: () => [] } })
  const data = { models: [
    { id: 'm', provider: 'workbuddy', edition: 'cn', providerLabel: '国内' },
    { id: 'm', provider: 'workbuddy', edition: 'intl', providerLabel: '国际' },
  ], mappings: [] }
  const counts = model.builtinRailItems(data)
  assert.equal(counts.size, 2)
  assert.equal(counts.get('workbuddy\u0001cn').n, 1)
  assert.equal(model.resolveProvider({ provider: 'workbuddy\u0001intl', data }), 'workbuddy\u0001intl')
  assert.equal(model.resolveProvider({ provider: 'missing', data }), 'all')
})

test('local/file subscriptions replace content in place before saving metadata', async () => {
  const calls = []
  const window = { workbuddyDesktop: {
    replaceSubscriptionFile: async payload => { calls.push(['replace', payload]); return { items: [] } },
    saveSubscription: async payload => { calls.push(['save', payload]); return { items: [] } },
  } }
  const sub = await load('proxies-sub-state', window)
  assert.equal(sub.isLocalSubscription({ kind: 'file' }), true)
  assert.equal(sub.isLocalSubscription({ kind: 'local' }), true)
  await sub.saveSubscription({ id: 'sub_existing', kind: 'file' }, {
    name: 'renamed', enabled: false, kind: 'local', contentBase64: 'eA==', fileName: 'x.yaml', sourcePath: 'D:/x.yaml',
  })
  assert.equal(calls[0][0], 'replace')
  assert.equal(calls[0][1].id, 'sub_existing')
  assert.equal(calls[0][1].contentBase64, 'eA==')
  assert.equal(calls[0][1].sourcePath, 'D:/x.yaml')
  assert.equal(calls[1][0], 'save')
  assert.equal(calls[1][1].id, 'sub_existing')
  assert.equal(calls[1][1].enabled, false)
  assert.equal('contentBase64' in calls[1][1], false)
  calls.length = 0
  await sub.saveSubscription({ id: 'sub_existing', kind: 'local' }, { name: 'unchanged', enabled: true })
  assert.equal(calls.length, 1)
  assert.equal(calls[0][0], 'save')
  assert.equal('sourcePath' in calls[0][1], false)
})

test('subscription UI retains native path import, upload contract and halted polling gate', async () => {
  const state = await fs.readFile(new URL('../src/islands/proxies-sub-state.ts', import.meta.url), 'utf8')
  const view = await fs.readFile(new URL('../src/islands/proxies-sub-view.tsx', import.meta.url), 'utf8')
  assert.ok(state.includes('!kernel.installed || kernel.halted'))
  assert.ok(view.includes('importSubscriptionFile?.()'))
  assert.ok(view.includes('setSourcePath(imported.path'))
  assert.ok(view.includes('contentBase64'))
  assert.ok(!view.includes('contentB64'))
})

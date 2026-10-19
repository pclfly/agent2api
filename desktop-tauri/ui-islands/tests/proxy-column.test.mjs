import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'
import ts from 'typescript'

async function functionsFrom(file, names) {
  const source = await fs.readFile(new URL(file, import.meta.url), 'utf8')
  const ast = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  return ast.statements.filter(node => ts.isFunctionDeclaration(node) && names.includes(node.name?.text))
    .map(node => node.getText(ast)).join('\n')
}
const source = await functionsFrom('../src/islands/accounts-shared.ts', ['accountProxyField'])
  + '\n' + await functionsFrom('../src/islands/accounts-data.ts', ['applyProxyPick'])
const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }).outputText
function fixture(account) {
  const calls = [], errors = []
  let refreshed = 0, opened = 0
  const ctx = { exports: {}, t: (value, params = {}) => value.replace(/\{(\w+)\}/g, (match, key) => params[key] ?? match), POOL_VALUE_PREFIX: 'pool:',
    PROXY_CUSTOM_EDIT: '__proxy_custom_edit__', PROXY_CUSTOM_CURRENT: '__proxy_custom__',
    PROXY_ROTATE_ALL: '__proxy_rotate_all__',
    findAccount: () => account, openSettingsDialog: () => { opened++ },
    toast: (text, kind) => { if (kind === 'err') errors.push(text) }, errorMessage: e => e.message,
    shared: () => ({ workbuddyDesktop: { updateAccount: async (...args) => { calls.push(args); return {} } },
      wbApp: { refresh: async () => { refreshed++ } } }),
  }
  vm.runInNewContext(compiled, ctx)
  return { ...ctx.exports, calls, errors, refreshed: () => refreshed, opened: () => opened }
}
const chain = { proxy: { config: { source: 'custom', host: 'vault.example', port: 14322 } },
  frontProxy: { config: { source: 'pool', proxyId: 'first' }, label: '本地节点' } }
const plain = value => JSON.parse(JSON.stringify(value))

test('display source is first hop for chains, ordinary proxy for single hop and direct', () => {
  const f = fixture(chain)
  assert.equal(f.accountProxyField(chain), 'frontProxy')
  assert.equal(f.accountProxyField({ proxy: chain.proxy, frontProxy: null }), 'proxy')
  assert.equal(f.accountProxyField({}), 'proxy')
  assert.equal(f.accountProxyField({ frontProxy: { source: 'custom' } }), 'frontProxy')
  assert.equal(f.accountProxyField({ ...chain, frontProxy: { ...chain.frontProxy, error: 'missing node' } }), 'frontProxy')
})

test('inline chain selection updates only first hop without overwriting Vault', async () => {
  const f = fixture(chain)
  await f.applyProxyPick('acct', 'pool:new-first', 'pool:first')
  assert.deepEqual(plain(f.calls), [['acct', { frontProxy: { source: 'pool', proxyId: 'new-first' } }]])
  assert.equal(f.refreshed(), 1)
  assert.equal(chain.proxy.config.host, 'vault.example')
})

test('removing first hop leaves second hop and single hop direct still clears proxy', async () => {
  const f = fixture(chain)
  await f.applyProxyPick('acct', '', 'pool:first')
  assert.deepEqual(plain(f.calls), [['acct', { frontProxy: null }]])
  const single = fixture({ proxy: chain.proxy })
  await single.applyProxyPick('single', '', '__proxy_custom__')
  assert.deepEqual(plain(single.calls), [['single', { proxy: null }]])
})

test('unchanged and edit options do not write, missing accounts fail without writing', async () => {
  const f = fixture(chain)
  await f.applyProxyPick('acct', 'pool:first', 'pool:first')
  await f.applyProxyPick('acct', '__proxy_custom__', '')
  await f.applyProxyPick('acct', '__proxy_custom_edit__', '')
  assert.equal(f.calls.length, 0)
  assert.equal(f.opened(), 1)
  const missing = fixture(null)
  await missing.applyProxyPick('missing', 'pool:new', '')
  assert.equal(missing.calls.length, 0)
  assert.equal(missing.errors.length, 1)
})

test('inline rotation uses the compatible all-candidates payload for any provider', async () => {
  for (const provider of ['workbuddy', 'zcode', 'custom-vertex', 'custom-opencode-go']) {
    const f = fixture({ provider })
    await f.applyProxyPick('acct', '__proxy_rotate_all__', '')
    assert.deepEqual(plain(f.calls), [['acct', { proxy: { source: 'rotate', all: true, failureMode: 'smart' } }]])
    assert.equal(f.refreshed(), 1)
  }
})

test('existing selected rotation is not rewritten and first-hop chain rotation is rejected', async () => {
  const f = fixture({ proxy: { config: { source: 'rotate', all: false, proxyIds: ['b', 'a'], failureMode: 'all_errors' } } })
  await f.applyProxyPick('acct', '__proxy_custom__', '__proxy_custom__')
  assert.equal(f.calls.length, 0)
  const chained = fixture(chain)
  await chained.applyProxyPick('acct', '__proxy_rotate_all__', 'pool:first')
  assert.equal(chained.calls.length, 0)
  assert.match(chained.errors[0], /第一跳不支持轮询/)
})

test('proxy cell exposes sticky failover and a full-settings entry', async () => {
  const source = await functionsFrom('../src/islands/accounts-panels.tsx', ['ProxyCell'])
  assert.match(source, /if \(!chained\) items.push\(\{ value: PROXY_ROTATE_ALL/)
  assert.match(source, /轮询全部节点（故障切换）/)
  assert.match(source, /代理设置（自定义／轮询）/)
  assert.match(source, /粘性使用当前节点/)
  assert.match(source, /proxy\?\.config\?\.failureMode === 'all_errors'/)
  assert.match(source, /智能切换（推荐）/)
  assert.match(source, /所有上游错误都切换/)
  assert.match(source, /尝试预算与节点冷却/)
  assert.match(source, /不重放成功响应后的流/)
  assert.doesNotMatch(source, /每条请求换一个出口|逐请求轮换|超时或上游报错（含 HTTP 500）/)
})

test('proxy cell uses shared first-hop source for labels, selected value and error', async () => {
  const source = await functionsFrom('../src/islands/accounts-panels.tsx', ['ProxyCell'])
  assert.match(source, /const field = accountProxyField\(account\)/)
  assert.match(source, /const proxy = account\[field\]/)
  assert.match(source, /const broken = proxy\?\.error/)
  assert.match(source, /移除第一跳（仅用第二跳）/)
  assert.match(source, /此处改选只修改第一跳/)
})

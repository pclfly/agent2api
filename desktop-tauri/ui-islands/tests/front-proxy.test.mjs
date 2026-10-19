import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'
import ts from 'typescript'

const source = await fs.readFile(new URL('../src/islands/accounts-dialogs.tsx', import.meta.url), 'utf8')
const ast = ts.createSourceFile('accounts-dialogs.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
const names = ['draftOfProxy', 'readProxyDraft', 'accountRouteDrafts', 'readAccountProxyPatch']
const snippet = ast.statements.filter(node => ts.isFunctionDeclaration(node) && names.includes(node.name?.text))
  .map(node => node.getText(ast)).join('\n')
const sandbox = { exports: {}, t: key => key }
vm.runInNewContext(ts.transpileModule(snippet, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, sandbox)
const { draftOfProxy, accountRouteDrafts, readAccountProxyPatch } = sandbox.exports
const plain = value => JSON.parse(JSON.stringify(value))
const vault = { config: { source: 'custom', protocol: 'http', host: 'vault.example', port: 14322, username: '', password: '' } }
const local = { config: { source: 'pool', proxyId: 'subscription-node-stable-id' } }

test('single hop occupies first slot and second hop is disabled by default', () => {
  const draft = accountRouteDrafts({ proxy: local })
  assert.equal(draft.first.proxyId, local.config.proxyId)
  assert.equal(draft.second.mode, 'none')
  assert.equal(draft.secondEnabled, false)
  assert.deepEqual(plain(readAccountProxyPatch(draft.first, draft.second, true)), { proxy: local.config, frontProxy: null })
})

test('existing two-hop account preserves local then Vault without reversing traffic', () => {
  const draft = accountRouteDrafts({ proxy: vault, frontProxy: local })
  assert.equal(draft.first.proxyId, local.config.proxyId)
  assert.equal(draft.second.host, 'vault.example')
  assert.equal(draft.secondEnabled, true)
  assert.deepEqual(plain(readAccountProxyPatch(draft.first, draft.second, true, true)), {
    proxy: vault.config, frontProxy: local.config,
  })
})

test('disabling second hop retains first as sole exit and ignores stale second draft', () => {
  assert.deepEqual(plain(readAccountProxyPatch(draftOfProxy(local), { ...draftOfProxy(vault), port: '' }, true, false)), {
    proxy: local.config, frontProxy: null,
  })
})

test('old direct accounts stay direct and built-ins do not submit frontProxy', () => {
  const draft = accountRouteDrafts(undefined)
  assert.equal(draft.secondEnabled, false)
  assert.deepEqual(plain(readAccountProxyPatch(draft.first, draft.second, true)), { proxy: null, frontProxy: null })
  assert.deepEqual(plain(readAccountProxyPatch(draftOfProxy(local), draftOfProxy(vault), false)), { proxy: local.config })
})

test('enabled chain requires both hops and rejects invalid input', () => {
  assert.throws(() => readAccountProxyPatch(draftOfProxy(local), draftOfProxy(null), true, true), /同时配置/)
  assert.throws(() => readAccountProxyPatch(draftOfProxy(null), draftOfProxy(vault), true, true), /同时配置/)
  assert.throws(() => readAccountProxyPatch({ ...draftOfProxy(local), proxyId: '' }, draftOfProxy(vault), true, true), /选择/)
  assert.throws(() => readAccountProxyPatch(draftOfProxy(local), { ...draftOfProxy(vault), port: '70000' }, true, true), /端口/)
})

test('chain rejects SOCKS and first-hop rotation but single hop still supports SOCKS', () => {
  const socks = { ...draftOfProxy(vault), protocol: 'socks5' }
  assert.throws(() => readAccountProxyPatch(socks, draftOfProxy(vault), true, true), /HTTP CONNECT/)
  assert.throws(() => readAccountProxyPatch(draftOfProxy(local), socks, true, true), /HTTP CONNECT/)
  assert.throws(() => readAccountProxyPatch({ ...draftOfProxy(local), mode: 'rotate' }, draftOfProxy(vault), true, true), /不支持轮询/)
  assert.equal(readAccountProxyPatch(socks, draftOfProxy(null), true).proxy.protocol, 'socks5')
})

test('account form uses one context-aware route test and explicit TLS setting', () => {
  assert.match(source, /accountRouteDrafts\(account\)\.secondEnabled/)
  assert.match(source, /testProxy\?\.\(\{ accountId: id, \.\.\.route \}\)/)
  assert.match(source, /测试当前链路/)
  assert.match(source, /showTest=\{false\}/)
  assert.match(source, /insecureTls: providerInsecureTls/)
  assert.match(source, /请先保存提供商协议、地址或 TLS 设置/)
})

for (const [name, path] of [
  ['web', '../../src-tauri/server/src/web_shim.rs'],
  ['desktop', '../../src-tauri/src/bridge.rs'],
]) {
  test(`${name} bridge forwards account context and both draft proxies, preserving null`, async () => {
    const code = await fs.readFile(new URL(path, import.meta.url), 'utf8')
    const start = code.indexOf('testProxy:')
    const end = code.indexOf('\n    },', start)
    const method = code.slice(start, end + 7).replace(/,\s*$/, '')
    const calls = []
    const ctx = { call: (...args) => { calls.push(args) } }
    vm.runInNewContext(`this.bridge = ({${method}})`, ctx)
    ctx.bridge.testProxy({ accountId: 'acct', proxy: vault.config, frontProxy: null, insecureTls: true })
    assert.deepEqual(plain(calls.pop()), ['POST', '/api/proxies/test', { accountId: 'acct', proxy: vault.config, frontProxy: null }])
    ctx.bridge.testProxy({ proxy: null })
    assert.deepEqual(plain(calls.pop()), ['POST', '/api/proxies/test', { proxy: null }])
  })
}

test('provider update bridge preserves explicit TLS true and false without changing absent fields', async () => {
  const code = await fs.readFile(new URL('../../ui/custom-provider-ui.js', import.meta.url), 'utf8')
  const calls = []
  const ctx = {
    window: { wbProviders: { customRequest: async (...args) => calls.push(args), refreshCustom: async () => [] } },
    wbApp: { esc: value => value, toast: () => {}, refresh: async () => {} },
    wbI18n: { t: value => value },
  }
  vm.runInNewContext(code, ctx)
  for (const insecureTls of [true, false, undefined]) {
    await ctx.window.wbCustomProvidersUi.update({ id: 'custom-test', name: 'test', protocol: 'vertex', baseUrl: 'https://example.com', insecureTls })
    const body = calls.pop()[2]
    assert.equal(body.insecureTls, insecureTls)
    assert.equal(Object.hasOwn(body, 'insecureTls'), insecureTls !== undefined)
  }
})

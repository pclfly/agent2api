import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'
import ts from 'typescript'

const dialogs = await fs.readFile(new URL('../src/islands/accounts-dialogs.tsx', import.meta.url), 'utf8')
const domain = await fs.readFile(new URL('../src/islands/accounts-domain.ts', import.meta.url), 'utf8')
function functionsFrom(source, names) {
  const ast = ts.createSourceFile('source.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  return ast.statements.filter(node => ts.isFunctionDeclaration(node) && names.includes(node.name?.text))
    .map(node => node.getText(ast)).join('\n')
}
const sandbox = { exports: {}, t: key => key }
vm.runInNewContext(ts.transpileModule(
  functionsFrom(dialogs, ['draftOfProxy', 'readProxyDraft', 'readAccountProxyPatch'])
    + '\n' + functionsFrom(domain, ['supportsProxyRotate']),
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
).outputText, sandbox)
const { draftOfProxy, readProxyDraft, readAccountProxyPatch, supportsProxyRotate } = sandbox.exports
const plain = value => JSON.parse(JSON.stringify(value))

test('all providers support rotation without a loaded custom-provider catalog', () => {
  for (const provider of ['workbuddy', 'catpaw', 'zcode', 'custom-vertex', 'custom-zen', 'custom-go']) {
    assert.equal(supportsProxyRotate({ id: 'account', provider }), true)
  }
  assert.equal(supportsProxyRotate(null), false)
  assert.equal(supportsProxyRotate(undefined), false)
})

test('legacy rotate configs retain all/default and selected candidate order', () => {
  for (const config of [
    { source: 'rotate' },
    { source: 'rotate', all: true, proxyIds: ['ignored'] },
    { source: 'rotate', all: false, proxyIds: ['second', 'first'] },
  ]) {
    const draft = draftOfProxy({ config })
    const expected = { ...(config.all === false ? config : { source: 'rotate', all: true }), failureMode: 'smart' }
    assert.equal(draft.failureMode, 'smart')
    assert.deepEqual(plain(readProxyDraft(draft)), expected)
    assert.deepEqual(plain(readAccountProxyPatch(draft, draftOfProxy(null), false)), { proxy: expected })
  }
  assert.deepEqual(plain(readProxyDraft(draftOfProxy({ source: 'rotate' }))), { source: 'rotate', all: true, failureMode: 'smart' })
})

test('both failure modes round-trip all and selected candidates through edit and batch readers', () => {
  assert.equal(draftOfProxy(null).failureMode, 'smart')
  for (const failureMode of ['smart', 'all_errors']) {
    for (const all of [true, false]) {
      const config = { source: 'rotate', all, ...(all ? {} : { proxyIds: ['b', 'a'] }), failureMode }
      const draft = draftOfProxy({ config })
      assert.equal(draft.failureMode, failureMode)
      assert.deepEqual(plain(readProxyDraft(draft)), config)
      assert.deepEqual(plain(readAccountProxyPatch(draft, draftOfProxy(null), false)), { proxy: config })
      assert.deepEqual(plain(readAccountProxyPatch(draftOfProxy({ config: { source: 'pool', proxyId: 'fixed' } }), draft, true, true)), {
        frontProxy: { source: 'pool', proxyId: 'fixed' }, proxy: config,
      })
      assert.throws(() => readAccountProxyPatch(draft, draftOfProxy({ config: { source: 'pool', proxyId: 'fixed' } }), true, true), /第一跳不支持轮询/)
    }
  }
})

test('mode switching preserves the failure strategy without leaking it into fixed proxy payloads', () => {
  const draft = draftOfProxy({ config: { source: 'rotate', all: true, failureMode: 'all_errors' } })
  draft.mode = 'pool'
  draft.proxyId = 'fixed'
  assert.deepEqual(plain(readProxyDraft(draft)), { source: 'pool', proxyId: 'fixed' })
  draft.mode = 'rotate'
  assert.equal(readProxyDraft(draft).failureMode, 'all_errors')
  draft.failureMode = undefined
  assert.equal(readProxyDraft(draft).failureMode, 'smart')
  assert.equal(draftOfProxy({ config: { source: 'rotate', failureMode: 'unknown' } }).failureMode, 'smart')
})

test('shared form exposes both strategy choices and batch submits the shared reader result', () => {
  const form = functionsFrom(dialogs, ['ProxyForm'])
  assert.match(form, /Select value=\{draft.failureMode\}/)
  assert.match(form, /SelectItem value='smart'/)
  assert.match(form, /SelectItem value='all_errors'/)
  assert.match(form, /智能切换（推荐）/)
  assert.match(form, /所有上游错误都切换/)
  assert.match(form, /onValueChange=\{value => set\(\{ failureMode:/)
  const batch = functionsFrom(dialogs, ['BatchDialog'])
  assert.match(batch, /proxy = readProxyDraft\(proxyDraft\)/)
  assert.match(batch, /payload.proxy = proxy/)
  assert.match(batch, /ProxyForm draft=\{proxyDraft\} onChange=\{setProxyDraft\}/)
})

test('empty selected candidates are rejected and payload does not alias draft IDs', () => {
  const draft = draftOfProxy({ config: { source: 'rotate', all: false, proxyIds: [] } })
  assert.throws(() => readProxyDraft(draft), /至少要选择一条代理/)
  draft.rotateIds.push('one')
  const result = readProxyDraft(draft)
  draft.rotateIds.push('two')
  assert.deepEqual(plain(result.proxyIds), ['one'])
})

test('fixed first hop can retain rotation on second hop', () => {
  const first = draftOfProxy({ config: { source: 'pool', proxyId: 'fixed' } })
  const second = draftOfProxy({ config: { source: 'rotate', all: false, proxyIds: ['a', 'b'] } })
  assert.deepEqual(plain(readAccountProxyPatch(first, second, true, true)), {
    frontProxy: { source: 'pool', proxyId: 'fixed' },
    proxy: { source: 'rotate', all: false, proxyIds: ['a', 'b'], failureMode: 'smart' },
  })
})

test('account edit and mixed-provider batch use unrestricted rotation capability', () => {
  assert.match(dialogs, /const allowRotate = supportsProxyRotate\(target\)/)
  assert.match(dialogs, /accounts.length > 0 && accounts.every\(account => supportsProxyRotate\(account\)\)/)
  assert.match(functionsFrom(dialogs, ['ProxyForm']), /allowRotate = true/)
  assert.doesNotMatch(dialogs, /isOpencodeZenProvider/)
})

test('rotation copy explains sticky errors, port fallback, in-flight requests and replay limits', () => {
  const form = functionsFrom(dialogs, ['ProxyForm'])
  for (const text of [
    '轮询（故障切换）', '粘性使用当前节点', 'HTTP 500', '下一个已分配端口',
    '未分配端口的节点一次临时开放一个', '失败则释放并尝试下一个',
    '切换节点不会主动关闭其他在途请求正在使用的端口', '不会无损重放已输出内容',
    '确定性的参数或鉴权错误不遍历节点', 'HTTP 502–504', 'HTTP 403/429',
    '尝试预算与节点冷却', '不重放成功响应后的流', '不保证遍历全部节点',
  ]) assert.ok(form.includes(text), text)
  assert.doesNotMatch(dialogs + domain, /轮询（换 IP）|逐请求轮换|每条请求换一个出口|按 IP 限流|只有 OpenCode Zen|超时或上游报错（含 HTTP 500）/)
})

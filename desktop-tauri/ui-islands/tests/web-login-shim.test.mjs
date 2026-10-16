import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'
import { build } from 'esbuild'

const source = await fs.readFile(new URL('../../src-tauri/server/src/web_shim.rs', import.meta.url), 'utf8')
const scriptStart = source.indexOf('r#"(function') + 3
const script = source.slice(scriptStart, source.indexOf('"#', scriptStart))
const tick = async () => { for (let i = 0; i < 25; i++) await Promise.resolve() }
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r }); return { promise, resolve } }
function fixture(handler) {
  const calls = [], timers = [], popups = [], toasts = []
  let now = 0
  const window = { location: new URL('http://localhost:8080/'), open: () => {
    const popup = { closed: false, location: { href: 'about:blank' }, close() { this.closed = true } }
    popups.push(popup)
    return popup
  } }
  const nodes = []
  const element = tag => {
    const e = { tag, style:{}, children:[], listeners:{}, value:'', appendChild(n){this.children.push(n)},
      remove(){this.removed=true}, focus(){}, addEventListener(k,f){this.listeners[k]=f},
      querySelector(){return element('input')} }
    nodes.push(e); return e
  }
  const document = { getElementById:id=>nodes.find(n=>n.id===id&&!n.removed)||null, createElement:element, body:element('body') }
  window.wbApp = { toast: message => toasts.push(message) }
  const context = { window, AbortController, document, console, URL, URLSearchParams, Set, Error,
    localStorage: { getItem: () => null }, Date: { now: () => now },
    setTimeout: (fn, ms) => { timers.push({ fn, ms }); return timers.length }, clearTimeout: () => {},
    fetch: async (path, options) => {
      const request = { path, method: options.method, body: options.body ? JSON.parse(options.body) : null }
      calls.push(request)
      const result = await handler(request)
      if (result && result.httpStatus) return { status: result.httpStatus, text: async () => JSON.stringify({ success: false, error: 'synthetic authorization failure' }) }
      return { status: 200, text: async () => JSON.stringify({ success: true, data: result }) }
    },
  }
  vm.runInNewContext(script, context)
  return { bridge: window.workbuddyDesktop, context, calls, popups, toasts, nodes, async cleanupTimer() {
    const index = timers.findIndex(timer => timer.ms === 2000); assert.ok(index >= 0);
    timers.splice(index, 1)[0].fn(); await tick()
  }, async timer(advance = 3000) {
    const index = timers.findIndex(timer => timer.ms === 3000)
    assert.ok(index >= 0, 'expected login polling timer')
    const [timer] = timers.splice(index, 1); now += advance; timer.fn(); await tick()
  } }
}
test('Web update check preserves disabled and enabled capabilities without starting installation', async () => {
  for (const selfUpdate of [false, true]) {
    const info = { currentVersion: '3.0.6', latestVersion: '3.0.7', hasUpdate: true,
      selfUpdate, asset: { url: 'https://example.invalid/desktop.exe' },
      serverAsset: { url: 'https://example.invalid/server.tar.gz' }, notes: 'release notes' }
    const f = fixture(() => info)
    assert.deepEqual(JSON.parse(JSON.stringify(await f.bridge.checkUpdate())), info)
    assert.deepEqual(f.calls.map(call => call.path), ['/api/update/check'])
  }
})

test('Web bridge retains proxy compatibility commands and forwards replacement file metadata', async () => {
  const f = fixture(() => ({ items: [] }))
  await f.bridge.setProxyPoolItemEnabled({ id: 'px', enabled: false })
  await f.bridge.setProxyPoolItemDisabled('px', true)
  await f.bridge.getUsableProxyPool()
  const payload = { id: 'sub', contentBase64: 'eA==', fileName: 'x.yaml', sourcePath: '' }
  await f.bridge.replaceSubscriptionFile(payload)
  assert.deepEqual(f.calls.map(call => call.path), [
    '/api/proxies/pool/enable', '/api/proxies/pool/set-disabled',
    '/api/proxies/pool/usable', '/api/subscriptions/replace-file',
  ])
  assert.deepEqual(f.calls.at(-1).body, payload)
})

test('Raccoon rejects unsupported callback origins before creating a login task', async () => {
  const f = fixture(() => assert.fail('invalid callback origin must not reach backend'))
  f.context.window.location = new URL('https://panel.invalid/')
  const flow = f.bridge.startLogin('cn', 'external', 'raccoon')
  const rejected = assert.rejects(flow, /http:\/\//)
  await tick()
  assert.equal(f.popups.length, 0)
  assert.equal(f.calls.length, 0)
  const button = f.nodes.find(node => node.tag === 'button')
  assert.ok(button)
  button.listeners.click()
  await rejected
})

const started = state => ({ state, authUrl: 'https://gateway.qwenwork.ai/device/selectAccounts?challenge=synthetic' })

test('QwenWork configuration exposes web, manual and Windows snapshot modes', async () => {
  const result = await build({ entryPoints: [new URL('../src/islands/add-account-configs.ts', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')], bundle: true, write: false, format: 'iife', globalName: 'configs', platform: 'browser' })
  const context = { window: { workbuddyDesktop: { platform: 'windows' } } }
  vm.runInNewContext(result.outputFiles[0].text, context)
  const config = context.configs.configOf('qwenwork')
  assert.equal(config.provider, 'qwenwork')
  assert.deepEqual(Array.from(context.configs.methodsOf(config, 'intl')), ['web', 'manual', 'desktop'])
  assert.equal(config.webLogin.edition, 'intl')
  assert.deepEqual(Array.from(config.webLogin.modes, m => m.value), ['external'])
  context.window.workbuddyDesktop.platform = 'web'
  assert.deepEqual(Array.from(context.configs.methodsOf(config, 'intl')), ['web', 'manual'])
  const shell = await fs.readFile(new URL('../../src-tauri/src/login.rs', import.meta.url), 'utf8')
  assert.ok(shell.includes('"qwenwork" => Ok("qwenwork")'))
  assert.ok(shell.includes('(mode == "external" || provider == "qwenwork")'))
})

test('web shim opens official page and completes after pending without exposing credentials', async () => {
  let waits = 0
  const f = fixture(r => r.path.endsWith('/start') ? started('first') : { done: ++waits > 1, pending: waits === 1, session: { provider: 'qwenwork', nickname: 'Example' } })
  const result = f.bridge.startLogin('intl', 'external', 'qwenwork')
  await tick()
  assert.equal((await f.bridge.getLoginState()).active, true)
  assert.equal(f.popups[0].opener, null)
  assert.match(f.popups[0].location.href, /^https:\/\/gateway.qwenwork.ai\/device\/selectAccounts/)
  await f.timer(); await f.timer()
  assert.equal((await result).provider, 'qwenwork')
  assert.equal((await f.bridge.getLoginState()).active, false)
  assert.ok(f.popups[0].closed)
  assert.equal(f.calls.filter(r => r.path.endsWith('/cancel')).length, 0)
})

test('cancel before start returns does not navigate and cancels late state without clearing a new flow', async () => {
  const late = deferred(); let starts = 0
  const f = fixture(r => r.path.endsWith('/start') ? (++starts === 1 ? late.promise : started('second')) : r.path.endsWith('/cancel') ? { canceled: true } : { pending: true })
  const first = f.bridge.startLogin('intl', 'external', 'qwenwork'); await tick()
  await assert.rejects(f.bridge.startLogin('intl', 'external', 'qwenwork'), /完成或取消/)
  assert.equal(starts, 1, 'duplicate login must not send /start')
  await f.bridge.cancelLogin()
  assert.equal((await first).canceled, true)
  const second = f.bridge.startLogin('intl', 'external', 'qwenwork'); await tick()
  late.resolve(started('first')); await tick()
  assert.equal(f.popups[0].location.href, 'about:blank')
  assert.ok(!f.popups[1].closed)
  assert.equal((await f.bridge.getLoginState()).active, true)
  assert.deepEqual(f.calls.filter(r => r.path.endsWith('/cancel')).map(r => r.body.state), ['first'])
  await f.bridge.cancelLogin(); assert.equal((await second).canceled, true)
  assert.deepEqual(f.calls.filter(r => r.path.endsWith('/cancel')).map(r => r.body.state), ['first', 'second'])
})

test('cancel during /wait ignores a late success and never returns a successful login', async () => {
  const late = deferred()
  const f = fixture(r => r.path.endsWith('/start') ? started('first') : r.path.endsWith('/cancel') ? { canceled: true } : late.promise)
  const result = f.bridge.startLogin('intl', 'external', 'qwenwork'); await tick(); await f.timer()
  await f.bridge.cancelLogin()
  assert.equal((await result).canceled, true)
  late.resolve({ done: true, session: { provider: 'qwenwork' } }); await tick()
  assert.equal((await f.bridge.getLoginState()).active, false)
  assert.equal(f.calls.filter(r => r.path.endsWith('/cancel')).length, 1)
})

test('failed and timed-out web login cancel the backend and reset visible state', async () => {
  for (const timeout of [false, true]) {
    const f = fixture(r => r.path.endsWith('/start') ? started('first') : r.path.endsWith('/cancel') ? {} : timeout ? { pending: true } : { done: true, error: 'authorization denied' })
    const result = f.bridge.startLogin('intl', 'external', 'qwenwork')
    const rejected = assert.rejects(result, timeout ? /超时/ : /authorization denied/)
    await tick(); await f.timer(timeout ? 300001 : 3000); await rejected
    assert.equal((await f.bridge.getLoginState()).active, false)
    assert.equal(f.calls.filter(r => r.path.endsWith('/cancel')).length, 1)
  }
})


test('cancel cleanup is bounded and releases the real controller busy lock on pending, reject and 401', async () => {
  const engine = await fs.readFile(new URL('../../ui/web-login.js', import.meta.url), 'utf8')
  for (const failure of ['pending', 'reject', '401']) {
    const f = fixture(r => r.path.endsWith('/start') ? started('first') : r.path.endsWith('/cancel')
      ? failure === 'pending' ? new Promise(() => {}) : failure === 'reject' ? Promise.reject(new Error('offline')) : { httpStatus: 401 }
      : { pending: true })
    vm.runInNewContext("var busy=false;function releaseBusy(){busy=false;}var wbI18n={t:(k,p)=>p?String(p.message||k):k};", f.context)
    vm.runInNewContext(engine, f.context)
    const controller = f.context.window.wbWebLogin.create({ provider:'qwenwork', buttonId:'absent', cancelId:'absent', hintId:'absent', start:()=>f.bridge.startLogin('intl','external','qwenwork'), onSuccess:()=>assert.fail('canceled flow succeeded') })
    const first = controller.start(); await tick()
    const cancel = controller.cancel(); await tick()
    await first
    assert.equal(f.context.busy, false, 'cleanup cannot retain controller busy lock')
    if (failure === 'pending') await f.cleanupTimer()
    await cancel
    assert.match(f.toasts.at(-1), /未确认服务端取消/)
    assert.equal(f.calls.filter(r=>r.path.endsWith('/cancel')).length, 1)
    const retry = controller.start(); await tick()
    assert.equal(f.calls.filter(r=>r.path.endsWith('/start')).length, 2)
    const secondCancel = controller.cancel(); await tick(); await retry
    if (failure === 'pending') await f.cleanupTimer()
    await secondCancel
  }
})

test('AutoClaw concurrency cleanup cancels only the rejected pre-created task', async () => {
  const f = fixture(r => r.path.endsWith('/start') ? started('active') : r.path.endsWith('/cancel') ? { canceled:true } : { pending:true })
  const active = f.bridge.startLogin('intl','external','qwenwork'); await tick()
  await assert.rejects(f.bridge.startAutoclawOauth('autoclaw-intl','synthetic',''), /完成或取消/)
  await assert.rejects(f.bridge.startAutoclawOauthLogin('rejected','https://example.invalid/','external'), /完成或取消/)
  await tick()
  assert.deepEqual(f.calls.filter(r=>r.path.endsWith('/cancel')).map(r=>r.body.state), ['rejected'])
  assert.equal((await f.bridge.getLoginState()).active,true)
  assert.equal(f.popups[0].closed,false)
  await f.bridge.cancelLogin(); await active
})

test('AutoClaw normal flow preserves the ok/session contract', async () => {
  const f = fixture(() => ({done:true,session:{provider:'autoclaw-intl'}}))
  const result = f.bridge.startAutoclawOauthLogin('auto','https://example.invalid/','external'); await tick(); await f.timer()
  assert.equal((await result).ok,true)
  assert.equal((await result).session.provider,'autoclaw-intl')
  assert.equal(f.calls.filter(r=>r.path.endsWith('/cancel')).length,0)
})


test('Raccoon URL rewriting and manual callback waiting survive normal completion and cancellation', async () => {
  const raccoon = fixture(r=>r.path.endsWith('/start')?{state:'rac',authUrl:'https://example.invalid/oauth?login_source=desktop'}:{done:true,session:{provider:'raccoon'}})
  const result = raccoon.bridge.startLogin('cn','embedded','raccoon'); await tick()
  const url = new URL(raccoon.popups[0].location.href)
  assert.equal(url.searchParams.get('login_source'),'web')
  assert.equal(new URL(url.searchParams.get('redirect')).searchParams.get('state'),'rac')
  await raccoon.timer(); assert.equal((await result).provider,'raccoon')
  for (const provider of ['trae','codearts']) {
    const f = fixture(r=>r.path.endsWith('/start')?started(provider):r.path.endsWith('/cancel')?{}:{pending:true})
    const flow = f.bridge.startLogin('cn','external',provider); await tick()
    assert.ok(f.nodes.some(n=>n.id==='a2a-web-overlay'&&!n.removed))
    await f.bridge.cancelLogin(); assert.equal((await flow).canceled,true)
    assert.equal(f.nodes.some(n=>n.id==='a2a-web-overlay'&&!n.removed),false)
  }
})

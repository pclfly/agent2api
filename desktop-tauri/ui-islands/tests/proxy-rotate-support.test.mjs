import test from 'node:test'
import assert from 'node:assert/strict'
import { supportsProxyRotate } from '../src/islands/proxy-rotate-support.ts'

/**
 * 「轮询」档的可见性。
 *
 * 这条判据曾经只放行 OpenCode Zen 一种账号（`isOpencodeZenProvider`），
 * 症状是**其他所有账号的下拉里都看不到「轮询」** —— 用户报的就是
 * 「为什么账号代理还是没有轮询选项」。
 *
 * 后端 `core::proxy_rotate` 只认 `{source:"rotate"}` 这个配置形状，按账号 id
 * 存游标、与提供商无关，所以前端那层限制是多出来的。这里钉住「有账号即可」，
 * 防止哪天又被某个具体提供商的条件改回去。
 */

test('内置提供商的账号也能轮询', () => {
  assert.equal(supportsProxyRotate({ id: 'acc_1', provider: 'workbuddy' }), true)
  assert.equal(supportsProxyRotate({ id: 'acc_2', provider: 'qoder' }), true)
  assert.equal(supportsProxyRotate({ id: 'acc_3', provider: 'gemini' }), true)
})

test('自定义提供商的账号同样能轮询（不再要求 opencode 形态）', () => {
  assert.equal(supportsProxyRotate({ id: 'acc_x', provider: 'custom-abc' }), true)
})

test('没有账号时不给这个档', () => {
  // 首屏列表还没到 / 账号刚被删：那种情况连表单都不该开
  assert.equal(supportsProxyRotate(null), false)
  assert.equal(supportsProxyRotate(undefined), false)
})

test('判据与账号当前代理无关：已经是轮询的当然还在', () => {
  assert.equal(
    supportsProxyRotate({ id: 'acc_r', proxy: { source: 'rotate', config: { source: 'rotate' } } }),
    true,
  )
  // 直连 / 池引用 / 自定义 都不影响档位可见性
  assert.equal(supportsProxyRotate({ id: 'acc_n', proxy: null }), true)
  assert.equal(supportsProxyRotate({ id: 'acc_p', proxy: { source: 'pool' } }), true)
})

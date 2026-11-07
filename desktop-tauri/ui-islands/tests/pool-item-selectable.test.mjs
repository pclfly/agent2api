import test from 'node:test'
import assert from 'node:assert/strict'
import { poolItemSelectable } from '../src/islands/proxy-pool-select.ts'

/**
 * 代理下拉里「这条池条目能不能选」的口径。
 *
 * 注意分工：**禁用的条目根本不会进选项** —— 下拉读的是
 * `/api/proxies/pool/usable`，后端已滤掉它们（见 `api::proxies::pool_usable`）。
 * 这里的判定管的是「已经列出来的那些」还能不能被选，与后端
 * `proxy_pool::should_enable_on_reference` 一一对应：本页禁用的条目可选
 * （选中后后端会顺手启用它 —— 只有旧壳回落整份列表时才见得到这种条目），
 * Clash 侧停用与订阅里已移除的两类不可选。
 * 这两处口径一旦分叉，界面上会出现「灰着但能选中后失败」或「明明能选却被挡住」。
 */

test('被本页禁用的条目仍然可选（旧壳回落整份列表时的兜底）', () => {
  assert.equal(poolItemSelectable({ id: 'px_1', source: 'manual', enabled: false }), true)
  // Clash 条目在本页禁用：`enabled` 仍是 true，`localDisabled` 才是本页那一层
  assert.equal(
    poolItemSelectable({ id: 'px_2', source: 'clash', enabled: false, localDisabled: true, sourceEnabled: true }),
    true,
  )
})

test('已经可用的条目当然可选', () => {
  assert.equal(poolItemSelectable({ id: 'px_3', source: 'manual', enabled: true }), true)
})

test('Clash Verge 里停用的监听器不可选', () => {
  assert.equal(
    poolItemSelectable({ id: 'px_4', source: 'clash', enabled: false, sourceEnabled: false }),
    false,
  )
})

test('订阅里已移除的节点不可选', () => {
  assert.equal(
    poolItemSelectable({ id: 'px_5', source: 'subscription', enabled: false, gone: true }),
    false,
  )
})

/**
 * 「代理池禁用」的显示口径：标签里的「已禁用」只看后端算好的可用性
 * （`enabled && !disabled`），不把「Clash 里禁用了」与「用户禁用了它」
 * 说成同一件事（见 ui-islands/src/islands/accounts-shared.ts 的 poolItemLabel
 * 与后端 core::proxy_pool::is_usable）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import vm from 'node:vm'

/**
 * 在最小上下文里求值模块里的两个纯函数。
 *
 * 源码是 TS，但这两段只有类型标注与模板串；把 `: PoolItem` 这类标注剥掉即可
 * 直接喂给 vm（与 provider-label.test.mjs 的做法同一手法：不为一个纯函数
 * 测试拉整套 TS 工具链）。用**函数源码**而不是整体 eval：模块还引 React 之外的
 * 桥类型，整体加载需要一堆桩件，而测试要钉的判据只在这两个函数里。
 */
async function loadPoolItemLabel() {
  const source = await fs.readFile(
    new URL('../src/islands/accounts-shared.ts', import.meta.url),
    'utf8',
  )
  const start = source.indexOf('export function poolItemAddress')
  const end = source.indexOf('export const POOL_VALUE_PREFIX')
  assert.ok(start > 0 && end > start, '两个函数必须仍在 accounts-shared.ts 里')
  const snippet = source
    .slice(start, end)
    // 只剥本片段用到的类型标注（`: PoolItem` / `): string` / `: string`）
    .replace(/\(\s*item: PoolItem\s*\)/g, '(item)')
    .replace(/\): string \{/g, ') {')
    .replace(/export function /g, 'function ')
  const sandbox = {
    t: (key, params) => (params
      ? Object.entries(params).reduce(
        (text, [name, value]) => text.replaceAll(`{${name}}`, String(value)),
        key,
      )
      : key),
  }
  vm.runInNewContext(`${snippet}\nthis.poolItemLabel = poolItemLabel;`, sandbox)
  return sandbox.poolItemLabel
}

test('disabled label comes from usable, not from the upstream enabled flag', async () => {
  const label = await loadPoolItemLabel()
  const base = { id: 'px_1', name: '香港节点', resolved: { protocol: 'http', host: '127.0.0.1', port: 19107 } }

  // 可用：只写地址，不带「已禁用」
  assert.equal(label({ ...base, enabled: true, disabled: false, usable: true }),
    '香港节点（HTTP 127.0.0.1:19107）')

  // 用户禁用：usable=false → 标「已禁用」
  assert.equal(label({ ...base, enabled: true, disabled: true, usable: false }),
    '香港节点（HTTP 127.0.0.1:19107 · 已禁用）')

  // 上游禁用（Clash 里关了这个监听器）：同样不可用，但标的是「已禁用」
  // 而不是把它当成用户操作 —— 这正是两个字段分开的意义
  assert.equal(label({ ...base, enabled: false, disabled: false, usable: false }),
    '香港节点（HTTP 127.0.0.1:19107 · 已禁用）')

  // 旧壳没有 usable 字段时回落判 disabled（不看 enabled：镜像的 enabled 是上游的事）
  assert.equal(label({ ...base, enabled: false }),
    '香港节点（HTTP 127.0.0.1:19107）')
  assert.equal(label({ ...base, enabled: true, disabled: true }),
    '香港节点（HTTP 127.0.0.1:19107 · 已禁用）')
})

test('a resolve error replaces the address and keeps the disabled note', async () => {
  const label = await loadPoolItemLabel()
  assert.equal(
    label({ id: 'px_2', name: '旧节点', resolveError: '监听器已删除', usable: false, disabled: true }),
    '旧节点（解析失败 · 已禁用）',
  )
  // 没有名字时用 id 兜底（与后端 display_name 同一口径）
  assert.equal(label({ id: 'px_3', usable: true }), 'px_3')
})

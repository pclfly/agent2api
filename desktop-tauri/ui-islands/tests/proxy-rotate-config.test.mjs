import test from 'node:test'
import assert from 'node:assert/strict'
import {
  normalizeFailureMode, rotateDraftOf, rotatePayloadOf,
} from '../src/islands/proxy-rotate-config.ts'

/**
 * 轮询档的配置形状。这些断言逐条对应后端 `core::proxies` 的归一规则：
 * 形状对错了不会报错，只会静默变成「保存后策略弹回 smart」这类难查的偏差。
 */

test('缺键 / 认不出的切换策略一律按 smart（后端同侧）', () => {
  assert.equal(normalizeFailureMode(undefined), 'smart')
  assert.equal(normalizeFailureMode(null), 'smart')
  assert.equal(normalizeFailureMode(''), 'smart')
  assert.equal(normalizeFailureMode('bogus'), 'smart')
  assert.equal(normalizeFailureMode('smart'), 'smart')
  assert.equal(normalizeFailureMode('all_errors'), 'all_errors')
})

test('「全部」档不带 proxyIds，并保留策略', () => {
  const payload = rotatePayloadOf({
    rotateAll: true,
    // 表单里可能还留着上次选的几条 —— 带上会让「以后新增自动加入」失效
    rotateIds: ['px_1', 'px_2'],
    failureMode: 'all_errors',
  })
  assert.deepEqual(payload, { source: 'rotate', all: true, failureMode: 'all_errors' })
  assert.equal('proxyIds' in payload, false)
})

test('指定档带上去重后的 id 与策略', () => {
  const payload = rotatePayloadOf({
    rotateAll: false,
    rotateIds: ['px_1', 'px_2', 'px_1'],
    failureMode: 'smart',
  })
  assert.deepEqual(payload, {
    source: 'rotate', all: false, proxyIds: ['px_1', 'px_2'], failureMode: 'smart',
  })
})

test('指定档一条都没选时返回 null（调用方给提示，不送去 400）', () => {
  assert.equal(rotatePayloadOf({ rotateAll: false, rotateIds: [], failureMode: 'smart' }), null)
  // 空串 / 空白 id 也算「没选」
  assert.equal(rotatePayloadOf({ rotateAll: false, rotateIds: ['', '  '], failureMode: 'smart' }), null)
})

test('策略缺键时 payload 里仍写显式 smart（不依赖后端补默认）', () => {
  const payload = rotatePayloadOf({ rotateAll: true })
  assert.equal(payload?.failureMode, 'smart')
})

test('读回草稿：all 缺键按「全部」，failureMode 缺键按 smart', () => {
  assert.deepEqual(rotateDraftOf(undefined), {
    rotateAll: true, rotateIds: [], failureMode: 'smart',
  })
  assert.deepEqual(rotateDraftOf({}), {
    rotateAll: true, rotateIds: [], failureMode: 'smart',
  })
  assert.deepEqual(
    rotateDraftOf({ all: false, proxyIds: ['px_9'], failureMode: 'all_errors' }),
    { rotateAll: false, rotateIds: ['px_9'], failureMode: 'all_errors' },
  )
})

test('读写往返一致（保存后再打开不应丢策略）', () => {
  for (const config of [
    { all: true, failureMode: 'smart' },
    { all: true, failureMode: 'all_errors' },
    { all: false, proxyIds: ['px_1'], failureMode: 'all_errors' },
  ]) {
    const draft = rotateDraftOf(config)
    const payload = rotatePayloadOf(draft)
    assert.deepEqual(
      { all: payload.all, ...(payload.all ? {} : { proxyIds: payload.proxyIds }), failureMode: payload.failureMode },
      { all: config.all, ...(config.all ? {} : { proxyIds: config.proxyIds }), failureMode: config.failureMode },
    )
  }
})

import test from 'node:test'
import assert from 'node:assert/strict'
import {
  ORPHAN_VENDOR, groupOf, isGoneSubscriptionItem, isOrphanSubscriptionItem,
  isRemovedSubscriptionItem, vendorHits,
} from '../src/islands/proxy-pool-classify.ts'

/**
 * 「提供方」下拉的筛选口径。
 *
 * 筛错的两种症状都不会报错，只会在界面上少几条或多几条：
 *   · 已移除的节点混进某家机场 → 那家报的条数比实际可用多；
 *   · 孤儿 / 已移除没进「已移除的订阅」那一档 → 那批条目哪一档都筛不出来，
 *     用户删不掉它们（删除入口只在池表里）。
 */

const manual = { source: 'manual' }
const clash = { source: 'clash', providerId: '' }
const sub = { source: 'subscription', providerId: 'sub_a', gone: false, orphan: false }
const gone = { source: 'subscription', providerId: 'sub_a', gone: true }
const orphan = { source: 'subscription', providerId: 'sub_deleted', orphan: true }

test('组键按来源分三档，未知来源按手动', () => {
  assert.equal(groupOf(manual), 'manual')
  assert.equal(groupOf({}), 'manual')
  assert.equal(groupOf({ source: 'weird' }), 'manual')
  assert.equal(groupOf(clash), 'clash')
  assert.equal(groupOf(sub), 'subscription')
})

test('孤儿与已移除只在订阅节点上成立', () => {
  assert.equal(isOrphanSubscriptionItem(orphan), true)
  assert.equal(isGoneSubscriptionItem(gone), true)
  // 手动条目上带这些字段是脏数据：不该被当成「可删的订阅镜像」
  assert.equal(isOrphanSubscriptionItem({ source: 'manual', orphan: true }), false)
  assert.equal(isGoneSubscriptionItem({ source: 'clash', gone: true }), false)
  assert.equal(isRemovedSubscriptionItem(orphan), true)
  assert.equal(isRemovedSubscriptionItem(gone), true)
  assert.equal(isRemovedSubscriptionItem(sub), false)
})

test('粗档按来源比，与 providerId 无关', () => {
  assert.ok(vendorHits(manual, 'manual'))
  assert.ok(!vendorHits(clash, 'manual'))
  assert.ok(vendorHits(clash, 'clash'))
  assert.ok(vendorHits(sub, 'subscription'))
  assert.ok(vendorHits(orphan, 'subscription'), '孤儿仍属于订阅这一大类')
  assert.ok(vendorHits(manual, 'all'))
})

test('具体某家只命中该家的存活节点', () => {
  assert.ok(vendorHits(sub, 'sub_a'))
  assert.ok(!vendorHits(sub, 'sub_b'))
  assert.ok(!vendorHits(manual, 'sub_a'), '手动条目没有 providerId，不该被某家筛出来')
  // 已移除的归属于 sub_a，但必须落到「已移除」那一档 —— 同时出现在两档会
  // 让「这家 37 条」这个读数包住两条其实已经失效的条目
  assert.ok(!vendorHits(gone, 'sub_a'))
  assert.ok(!vendorHits(orphan, 'sub_deleted'))
})

test('已移除的订阅那一档收孤儿与已移除两类', () => {
  assert.equal(ORPHAN_VENDOR, '__orphan')
  assert.ok(vendorHits(orphan, ORPHAN_VENDOR))
  assert.ok(vendorHits(gone, ORPHAN_VENDOR))
  assert.ok(!vendorHits(sub, ORPHAN_VENDOR), '活着的节点不该出现在「已移除」里')
  assert.ok(!vendorHits(manual, ORPHAN_VENDOR))
})

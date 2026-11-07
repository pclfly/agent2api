/**
 * 代理池条目的**来源分类**（网络代理页的筛选与「提供方」下拉用）。
 *
 * 单独一个无依赖模块而不是留在 `proxies-state.ts`：那份模块带 `../i18n`
 * 依赖，而这里是纯数据判定（组键、孤儿 / 已移除、提供方命中），放到无依赖
 * 的模块里才能被 `npm test` 直接覆盖 —— 这些判定错了的症状是「筛出来的
 * 那一档少了几条」或「已移除的节点混进某家机场里」，肉眼看代码发现不了。
 */

/** 判定只需要这几个字段（`ProxyPoolItem` 是它的超集） */
export type ClassifiedPoolItem = {
  source?: string
  providerId?: string
  /** 订阅里已移除该节点（同步保留身份、禁用并收回端口） */
  gone?: boolean
  /** 孤儿订阅节点：所属订阅已不存在（后端标注） */
  orphan?: boolean
}

/** 条目 → 组键（未知来源按手动处理：它一定不是那两类只读镜像） */
export function groupOf(item: ClassifiedPoolItem): 'manual' | 'clash' | 'subscription' {
  if (item.source === 'clash') return 'clash'
  if (item.source === 'subscription') return 'subscription'
  return 'manual'
}

/** 是否订阅节点镜像 */
export function isSubscriptionItem(item: ClassifiedPoolItem): boolean {
  return item.source === 'subscription'
}

/** 孤儿订阅节点：所属订阅已不存在，可手动删除 */
export function isOrphanSubscriptionItem(item: ClassifiedPoolItem): boolean {
  return isSubscriptionItem(item) && item.orphan === true
}

/** 订阅里已移除该节点：与孤儿一样可手动删除 */
export function isGoneSubscriptionItem(item: ClassifiedPoolItem): boolean {
  return isSubscriptionItem(item) && item.gone === true
}

/**
 * 「已移除的订阅」那一档的值：孤儿节点（所属订阅已删）与已移除节点单列一档，
 * 别混进各家机场的档里 —— 它们不再属于任何一家，混进去会让「某家 37 条」
 * 这个读数变成 39，而用户点进去看到的 2 条其实已经失效。
 */
export const ORPHAN_VENDOR = '__orphan'

/** 已不归属任何现存订阅（孤儿或已移除） */
export function isRemovedSubscriptionItem(item: ClassifiedPoolItem): boolean {
  return isOrphanSubscriptionItem(item) || isGoneSubscriptionItem(item)
}

/**
 * 提供方档位是否命中。
 *
 * - `all` 不筛；
 * - `manual` / `clash` / `subscription` 三个粗档按来源组比；
 * - `__orphan` 看孤儿与已移除两类；
 * - 其余按条目上的 `providerId` 比（就是那个订阅的 id）——**排除已移除的**，
 *   它们已经在「已移除的订阅」那一档里，同时出现在某家机场里会让两档读数
 *   互相矛盾。
 */
export function vendorHits(item: ClassifiedPoolItem, vendor: string): boolean {
  if (vendor === 'all') return true
  if (vendor === 'manual' || vendor === 'clash' || vendor === 'subscription') {
    return groupOf(item) === vendor
  }
  if (vendor === ORPHAN_VENDOR) return isRemovedSubscriptionItem(item)
  return isSubscriptionItem(item) && !isRemovedSubscriptionItem(item) && item.providerId === vendor
}

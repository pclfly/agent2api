import { t } from '../i18n'
import { proxyServiceLabels, type ProxyUsage } from '../../../ui-shared/proxy-usage'
import { acceptProxyPool } from './proxy-pool-cache'
import type { ClashProxySnapshot } from '../../../ui-shared/clash-proxy'

/* ─── 后端形态 ───────────────────────────────── */

export type ProxyPoolItem = ProxyUsage & {
  id: string
  name: string
  source: string
  enabled: boolean
  sourceEnabled?: boolean
  localDisabled?: boolean
  listenerUid?: string
  protocol: string
  host: string
  port: number | null
  username: string
  password: string
  createdAt: number
  updatedAt: number
  lastTest: {
    success?: boolean
    ip?: string
    durationMs?: number
    at?: number
    error?: string | null
  } | null
  resolved: { protocol?: string; host?: string; port?: number | null; label?: string } | null
  resolveError: string | null
  /** 订阅里已移除该节点（同步保留身份、禁用并收回端口）；可手动删除 */
  gone?: boolean
  /** 订阅节点的归属订阅 id（手动 / Clash 条目为空串）—— 界面按它分批筛选 */
  providerId?: string
  /** 订阅节点在机场清单里的原始节点名 */
  node?: string
  /** 孤儿订阅节点：所属订阅已不存在（后端标注），可手动删除 */
  orphan?: boolean
}

export type ProxyPoolPayload = {
  items?: ProxyPoolItem[]
  removed?: string
  clash?: ClashProxySnapshot
  changes?: number
  syncError?: string | null
  /** 监听器对账：发现多少个账号引用的订阅节点还缺端口（已请求内核补建） */
  listenersMissing?: number | null
  /** 监听器对账：有缺口但补不了（内核没装 / 未启用订阅 / 被手动关闭） */
  listenerBlocked?: boolean | null
}

export type ProxyTestResult = {
  success?: boolean
  status?: unknown
  ip?: string
  durationMs?: unknown
  error?: string
  /** 结果没能落库时的原因（测试结论仍然有效） */
  saveError?: string
  items?: ProxyPoolItem[]
}

export type ProxySettings = {
  /** 订阅节点监听端口区间（生效值；非法存量已由后端回落默认） */
  listenerPortBase?: number
  listenerPortEnd?: number
  testUrl?: string
  /** 轮询出口的并发与排队预算（生效值） */
  rotateMaxConcurrentPerIp?: number
  rotateMaxQueueSize?: number
  rotateQueueTimeoutSeconds?: number
  defaults?: {
    listenerPortBase?: number
    listenerPortEnd?: number
    testUrl?: string
    rotateMaxConcurrentPerIp?: number
    rotateMaxQueueSize?: number
    rotateQueueTimeoutSeconds?: number
  }
}

/** 设置弹窗一次提交完整表单（接口本身仍允许部分字段更新）。 */
export type ProxySettingsPayload = Required<Omit<ProxySettings, 'defaults'>>

/** 清除轮询候选冷却的结果；计数是记录数，不是节点数。 */
export type ProxyRotateCooldownClearResult = {
  /** 出口 × 目标维度的冷却记录数。 */
  cleared: number
  /** false 表示仅内存生效，未能持久化。 */
  persisted: boolean
}

/* ─── 桥（只声明本页用到的成员；不 declare global，理由见 accounts-shared）─── */

export type ProxyPoolBridge = {
  getProxyPool?: () => Promise<ProxyPoolPayload | null | undefined>
  createProxyPoolItem?: (payload: Record<string, unknown>) => Promise<ProxyPoolPayload | null | undefined>
  updateProxyPoolItem?: (payload: Record<string, unknown>) => Promise<ProxyPoolPayload | null | undefined>
  setProxyPoolItemEnabled?: (payload: { id: string; enabled: boolean }) => Promise<ProxyPoolPayload | null | undefined>
  removeProxyPoolItem?: (id: string) => Promise<ProxyPoolPayload | null | undefined>
  testProxyPoolItem?: (id: string) => Promise<ProxyTestResult | null | undefined>
  syncClashToProxyPool?: () => Promise<ProxyPoolPayload | null | undefined>
  /** 清除全部轮询候选冷却（管理动作）；只清冷却，不动账号限额与内核 */
  clearProxyRotateCooldowns?: () => Promise<ProxyRotateCooldownClearResult | null | undefined>
  /** 测**未保存**的表单内容（弹窗里的「测试出口」，payload 是 `{proxy}`） */
  testProxy?: (payload: { proxy: unknown }) => Promise<ProxyTestResult | null | undefined>
  /** 连通性测试目标网址 */
  getProxySettings?: () => Promise<ProxySettings | null | undefined>
  saveProxySettings?: (payload: Record<string, unknown>) => Promise<ProxySettings | null | undefined>
}

export type ProxySharedWindow = {
  workbuddyDesktop?: ProxyPoolBridge
  wbApp?: {
    toast?: (message: string, kind?: 'err' | 'ok') => void
    formatTime?: (value: unknown) => string
    /** 当前页标识：首屏只在用户正看着本页时才自拉一次 */
    readonly currentPage?: string
  }
  wbConfirm?: {
    ask?: (options: {
      title?: string
      html?: string
      text?: string
      okText?: string
      okClass?: string
      cancelText?: string
    }) => Promise<boolean>
  }
}

export function wb(): ProxySharedWindow {
  return window as unknown as ProxySharedWindow
}

export function toast(message: string, kind?: 'err' | 'ok'): void {
  wb().wbApp?.toast?.(message, kind)
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

export function formatTime(value: unknown): string {
  return wb().wbApp?.formatTime?.(value) || ''
}

/** 耗时文案：`2300` → `2.3s`，`230` → `230ms`（与账号弹窗的出口测试同一口径） */
export function durationText(value: unknown): string {
  const ms = Number(value)
  if (!Number.isFinite(ms) || ms < 0) return ''
  return ms < 1000 ? `${Math.round(ms)}ms` : `${(ms / 1000).toFixed(1)}s`
}

/** 确认框 / 提示里的 HTML 转义（值来自用户输入的名称） */
export function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, char => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char] || char
  ))
}

export function itemName(item: ProxyPoolItem | null | undefined): string {
  return String(item?.name || item?.id || '')
}

/**
 * 只读条目的统一判定（Clash 同步 / 订阅镜像都不给**编辑**入口）。
 *
 * 孤儿与已移除是删除入口的唯一例外（见 `proxy-pool-classify`）——
 * 归属的订阅不认识它们了，删除是清理它们的唯一出路。
 */
export function isReadonlyPoolItem(item: ProxyPoolItem): boolean {
  return item.source === 'clash' || item.source === 'subscription'
}

/* ─── store ─────────────────────────────────── */

type PoolState = {
  data: ProxyPoolPayload | null
  loading: boolean
  syncing: boolean
  error: string
  /** 正在测试的条目 id（行内按钮的文案与去重闸；批量测试时是「当前那一条」） */
  testing: string | null
  /** 写操作 / 单条同步在途的条目 id（该行的开关与按钮禁用，防连点） */
  pending: ReadonlySet<string>
  /** 批量操作在途（批量按钮的禁用与进度文案） */
  batchBusy: boolean
  /** 批量测试的进度 `{done, total}`；null = 没在跑 */
  batchProgress: { done: number; total: number } | null
  /** 勾选的条目 id（只作用于当前勾选，不随列表刷新自动增减 —— 消失的会被剔除） */
  selected: ReadonlySet<string>
  /** 「清除轮询候选冷却」在途（那颗按钮的文案与去重闸） */
  clearingCooldowns: boolean
}

let state: PoolState = {
  data: null,
  loading: false,
  syncing: false,
  error: '',
  testing: null,
  pending: new Set<string>(),
  batchBusy: false,
  batchProgress: null,
  selected: new Set<string>(),
  clearingCooldowns: false,
}

const listeners = new Set<() => void>()

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

export function getSnapshot(): PoolState {
  return state
}

function patch(partial: Partial<PoolState>): void {
  state = { ...state, ...partial }
  listeners.forEach(listener => listener())
}

/** 当前列表（`[]` = 还没读到或一条都没有，用 `state.data` 区分） */
export function items(): ProxyPoolItem[] {
  return state.data?.items ?? []
}

/** 「全选」的作用域：当前列表里的全部条目 id（顺序与表格一致） */
export function pickableIds(): string[] {
  return items().map(item => item.id)
}

/* ─── 勾选 ───────────────────────────────────── */

export function isPicked(id: string): boolean {
  return state.selected.has(id)
}

export function togglePick(id: string, picked: boolean): void {
  const next = new Set(state.selected)
  if (picked) next.add(id)
  else next.delete(id)
  patch({ selected: next })
}

export function setAllPicked(ids: string[], picked: boolean): void {
  const next = new Set(state.selected)
  for (const id of ids) {
    if (picked) next.add(id)
    else next.delete(id)
  }
  patch({ selected: next })
}

export function clearSelection(): void {
  patch({ selected: new Set<string>() })
}

export function accept(payload: ProxyPoolPayload | null | undefined): void {
  if (!payload || typeof payload !== 'object' || !Array.isArray(payload.items)) return
  const alive = new Set(payload.items.map(item => item.id))
  const selected = new Set([...state.selected].filter(id => alive.has(id)))
  patch({ data: { ...state.data, ...payload }, error: '', selected })
  acceptProxyPool(payload.items)
}

/* ─── 取数与单条动作 ─────────────────────────── */

export async function loadPanel(silent = false): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getProxyPool !== 'function') {
    patch({ error: t('当前环境不支持代理池（桥接方法缺失）') })
    return
  }
  if (!silent) patch({ loading: true })
  try {
    const data = await bridge.getProxyPool()
    if (!data || !Array.isArray(data.items)) throw new Error(t('代理列表响应异常'))
    accept(data)
    // 进入本页时后端会顺手对一次监听器账（见 `api::proxies::reconcile_listeners`）：
    // 有缺口就已在请求内核补建。静默轮询不提示，免得同一条 toast 反复弹。
    if (!silent) {
      if (data.listenerBlocked) {
        toast(t('有账号引用的订阅节点还没有监听端口，但代理内核当前未运行 —— 点「启动内核」后端口才会分配'), 'err')
      } else if (data.listenersMissing) {
        toast(t('发现 {n} 个账号引用的订阅节点还没有监听端口，正在请求内核补建；稍等片刻后重新读取', { n: data.listenersMissing }), 'ok')
      }
    }
  } catch (error) {
    patch({ error: errorMessage(error) })
    if (!silent) toast(t('代理列表加载失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ loading: false })
  }
}

export async function syncClash(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.syncClashToProxyPool !== 'function') {
    toast(t('当前环境不支持同步（桥接方法缺失）'), 'err')
    return
  }
  if (state.syncing || state.batchBusy || state.testing || state.pending.size) return
  patch({ syncing: true })
  try {
    const data = await bridge.syncClashToProxyPool()
    if (!data || !Array.isArray(data.items)) throw new Error(t('代理列表响应异常'))
    accept(data)
    if (data.syncError) toast(t('没能同步：{reason}', { reason: data.syncError }), 'err')
    else toast(data.changes
      ? t('✅ 已同步 Clash Verge 出口（{n} 项变更）', { n: data.changes })
      : t('已是最新，没有需要同步的变更'), 'ok')
  } catch (error) {
    toast(t('同步失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ syncing: false })
  }
}

/**
 * 清除**全部**轮询候选冷却（管理动作）。
 *
 * ── 它清的是什么、不清什么 ──────────────────────────────────
 * 只清出口冷却（`proxy_rotate::health` 那张表：一条出口刚失败过、这段时间
 * 先别用它）。**不**碰账号限额（`rateLimits`）、**不**重启内核 —— 三件事
 * 互不相干，混在一起会让排障时分不清到底清掉了什么。
 *
 * 与当前筛选 / 勾选**无关**：后端只接受空 body（带筛选字段会被 400 拒绝），
 * 否则「只清了某个节点」的误读比多一次确认更贵。所以确认文案里也明说这一点。
 */
export async function clearRotateCooldowns(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.clearProxyRotateCooldowns !== 'function') {
    toast(t('当前环境不支持清除轮询候选冷却（桥接方法缺失）'), 'err')
    return
  }
  if (state.clearingCooldowns || state.batchBusy || state.testing) return
  const ok = await wb().wbConfirm?.ask?.({
    title: t('清除轮询候选冷却'),
    text: t('清除全部本地轮询候选冷却，不受当前筛选或勾选影响。不会清除账号限额或上游限流，不会启用已禁用节点，也不会停止正在进行的请求。后续失败仍可能重新进入冷却。确定继续？'),
    okText: t('清除轮询候选冷却'),
    cancelText: t('取消清除'),
  })
  if (!ok) return
  patch({ clearingCooldowns: true })
  try {
    const data = await bridge.clearProxyRotateCooldowns()
    if (!data || typeof data.cleared !== 'number') throw new Error(t('清除轮询候选冷却响应异常'))
    if (!data.cleared) {
      toast(t('没有需要清除的轮询候选冷却记录'), 'ok')
    } else if (!data.persisted) {
      // 仅内存生效：重启后冷却可能从库里恢复回来。这条**必须**说出来，
      // 否则用户以为清干净了、重启后「明明清过还是失败」。
      toast(t('仅在内存中清除 {cleared} 条冷却记录，未能持久化；重启后冷却可能恢复。', { cleared: data.cleared }), 'err')
    } else {
      toast(t('已清除 {cleared} 条轮询候选冷却记录', { cleared: data.cleared }), 'ok')
    }
  } catch (error) {
    toast(t('清除轮询候选冷却失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ clearingCooldowns: false })
  }
}

export async function testItem(item: ProxyPoolItem): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.testProxyPoolItem !== 'function') {
    toast(t('当前环境不支持代理测试（桥接方法缺失）'), 'err')
    return
  }
  if (state.testing) return
  patch({ testing: item.id })
  try {
    const data = await bridge.testProxyPoolItem(item.id)
    if (data?.items) accept({ items: data.items })
    if (data?.success) {
      const suffix = durationText(data.durationMs)
      // 出口 IP / 耗时是附带读数：IP 是后端数据、耗时只有数字，各自做形参不做键
      toast(t('✅ 「{name}」出口可用{ip}{took}', {
        name: itemName(item),
        ip: data.ip ? t('　出口 IP {ip}', { ip: data.ip }) : '',
        took: suffix ? `　${suffix}` : '',
      }))
    } else {
      toast(t('❌ 「{name}」{error}', { name: itemName(item), error: data?.error || t('连接失败') }), 'err')
    }
    if (data?.saveError) toast(t('测试结果没能记下：{reason}', { reason: data.saveError }), 'err')
  } catch (error) {
    toast(t('测试失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ testing: null })
  }
}

export async function toggleItem(item: ProxyPoolItem, enabled: boolean): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.setProxyPoolItemEnabled !== 'function') {
    toast(t('当前环境不支持启停（桥接方法缺失）'), 'err')
    return
  }
  const pending = new Set(state.pending)
  pending.add(item.id)
  patch({ pending })
  try {
    const data = await bridge.setProxyPoolItemEnabled({ id: item.id, enabled })
    accept(data)
    const name = itemName(item)
    toast(enabled ? t('已启用「{name}」', { name }) : t('已禁用「{name}」', { name }))
  } catch (error) {
    toast(t('保存失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    const next = new Set(state.pending)
    next.delete(item.id)
    patch({ pending: next })
  }
}

export async function removeItem(item: ProxyPoolItem): Promise<void> {
  if (item.source === 'clash') {
    toast(t('只读：在 Clash Verge 里改'), 'err')
    return
  }
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.removeProxyPoolItem !== 'function') return
  const used = item.usedBy || []
  const usedHtml = used.length
    ? `<p style="margin-top:6px">${t('有 <strong>{n}</strong> 个账号正在使用它（{names}）—— 删除后这些账号会回退直连。', {
        n: used.length,
        names: used.map(entry => escapeHtml(String(entry.name || entry.id || ''))).join(t('、')),
      })}</p>`
    : ''
  const services = proxyServiceLabels(item, t)
  const serviceHtml = services.length
    ? `<p style="margin-top:6px">${t('此代理用于{services}；删除后这些线路将不可用，不会自动回退直连。', { services: services.map(escapeHtml).join(t('、')) })}</p>`
    : ''
  const ok = await wb().wbConfirm?.ask?.({
    title: t('删除代理'),
    html: t('确定删除「<strong>{name}</strong>」？{used}', {
      name: escapeHtml(itemName(item)),
      used: usedHtml + serviceHtml,
    }),
    okText: t('删除'),
    okClass: 'danger',
  })
  if (!ok) return
  const pending = new Set(state.pending)
  pending.add(item.id)
  patch({ pending })
  try {
    const data = await bridge.removeProxyPoolItem(item.id)
    accept(data)
    toast(t('已删除「{name}」', { name: itemName(item) }))
  } catch (error) {
    toast(t('删除失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    const next = new Set(state.pending)
    next.delete(item.id)
    patch({ pending: next })
  }
}

/* ─── 批量 ───────────────────────────────────── */

export async function batchTest(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.testProxyPoolItem !== 'function') {
    toast(t('当前环境不支持代理测试（桥接方法缺失）'), 'err')
    return
  }
  if (state.batchBusy || state.testing) return
  const targets = items().filter(item => state.selected.has(item.id))
  if (!targets.length) return
  patch({ batchBusy: true, batchProgress: { done: 0, total: targets.length } })
  let ok = 0
  let failed = 0
  let saveError = ''
  for (const [index, item] of targets.entries()) {
    patch({ testing: item.id, batchProgress: { done: index, total: targets.length } })
    try {
      const data = await bridge.testProxyPoolItem(item.id)
      if (data?.items) accept({ items: data.items })
      if (data?.success) ok += 1
      else failed += 1
      if (data?.saveError && !saveError) saveError = data.saveError
    } catch {
      failed += 1
    }
  }
  patch({ testing: null, batchBusy: false, batchProgress: null })
  const summary = failed
    ? t('批量测试完成：{ok} 个可用、{failed} 个失败（共 {total} 个）', { ok, failed, total: targets.length })
    : t('批量测试完成：{ok} 个可用（共 {total} 个）', { ok, total: targets.length })
  toast(failed ? summary : `✅ ${summary}`, failed ? 'err' : 'ok')
  if (saveError) toast(t('部分测试结果没能记下：{reason}', { reason: saveError }), 'err')
}

export async function batchRemove(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.removeProxyPoolItem !== 'function') return
  // 与批量测试互斥（一条在测时删它，测试结果回来会报「条目不存在」）；
  // 行内启停 / 单条测试同理 —— 视图侧按钮也按同一组条件禁用
  if (state.batchBusy || state.testing) return
  const picked = items().filter(item => state.selected.has(item.id))
  const deletable = picked.filter(item => item.source !== 'clash')
  const skipped = picked.length - deletable.length
  if (!deletable.length) {
    if (skipped) toast(t('选中的 {n} 条都来自 Clash Verge 同步，不能删除（请到 Clash Verge 里删）', { n: skipped }), 'err')
    return
  }
  const names = deletable.slice(0, 8).map(item => escapeHtml(itemName(item))).join(t('、'))
  const namesText = deletable.length > 8
    ? t('{names} 等 {n} 条', { names, n: deletable.length })
    : names
  const usedCount = deletable.filter(item => (item.usedBy || []).length).length
  const usedHtml = usedCount
    ? `<p style="margin-top:6px">${t('其中 <strong>{n}</strong> 条正被账号使用 —— 删除后那些账号会回退直连。', { n: usedCount })}</p>`
    : ''
  const services = [...new Set(deletable.flatMap(item => proxyServiceLabels(item, t)))]
  const serviceHtml = services.length
    ? `<p style="margin-top:6px">${t('选中的代理用于{services}；删除后这些线路将不可用，不会自动回退直连。', { services: services.map(escapeHtml).join(t('、')) })}</p>`
    : ''
  const skippedHtml = skipped
    ? `<p style="margin-top:6px">${t('另有 {n} 条来自 Clash Verge 同步，将跳过（请到 Clash Verge 里删）。', { n: skipped })}</p>`
    : ''
  const ok = await wb().wbConfirm?.ask?.({
    title: t('批量删除代理'),
    html: `${t('确定删除选中的 <strong>{n}</strong> 个代理？', { n: deletable.length })}<p style="margin-top:6px">${namesText}</p>${usedHtml}${serviceHtml}${skippedHtml}`,
    okText: t('删除'),
    okClass: 'danger',
  })
  if (!ok) return
  patch({ batchBusy: true, batchProgress: { done: 0, total: deletable.length } })
  let done = 0
  let failed = 0
  let firstError = ''
  for (const [index, item] of deletable.entries()) {
    patch({ batchProgress: { done: index, total: deletable.length } })
    try {
      const data = await bridge.removeProxyPoolItem(item.id)
      accept(data)
      done += 1
    } catch (error) {
      failed += 1
      if (!firstError) firstError = errorMessage(error)
    }
  }
  patch({ batchBusy: false, batchProgress: null })
  const parts = [t('已删除 {n} 个代理', { n: done })]
  if (skipped) parts.push(t('跳过 {n} 个（Clash 同步）', { n: skipped }))
  if (failed) parts.push(t('失败 {n} 个（{reason}）', { n: failed, reason: firstError }))
  toast(failed ? parts.join(t('，')) : `✅ ${parts.join(t('，'))}`, failed ? 'err' : 'ok')
}

/** 新建 / 编辑提交（表单已在弹窗里校验过形状） */
export async function saveItem(
  editing: ProxyPoolItem | null,
  payload: Record<string, unknown>,
): Promise<ProxyPoolPayload | null | undefined> {
  const bridge = wb().workbuddyDesktop
  const body = editing ? { ...payload, id: editing.id } : payload
  const data = editing
    ? await bridge?.updateProxyPoolItem?.(body)
    : await bridge?.createProxyPoolItem?.(body)
  if (!data || !Array.isArray(data.items)) throw new Error(t('代理列表响应异常'))
  accept(data)
  return data
}

/* ─── 代理设置（设置弹窗）────────────────────── */

/** 读代理设置（设置弹窗打开时调一次；拿不到返回 null，弹窗显示读取失败） */
export async function loadSettings(): Promise<ProxySettings | null> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getProxySettings !== 'function') {
    throw new Error(t('当前环境不支持代理设置（桥接方法缺失）'))
  }
  const data = await bridge.getProxySettings()
  if (!data || typeof data !== 'object') {
    throw new Error(t('代理设置响应异常'))
  }
  return data
}

export async function saveSettings(payload: ProxySettingsPayload): Promise<ProxySettings> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.saveProxySettings !== 'function') {
    throw new Error(t('当前环境不支持代理设置（桥接方法缺失）'))
  }
  const data = await bridge.saveProxySettings(payload)
  if (!data || typeof data !== 'object') {
    throw new Error(t('代理设置响应异常'))
  }
  return data
}

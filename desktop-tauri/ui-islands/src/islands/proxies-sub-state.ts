/**
 * 「订阅」区块的数据层（非岛：`.ts` 不被 import.meta.glob 当岛加载）。
 *
 * 视图层在 `proxies-sub-view.tsx`（岛内组件），本文件只放状态、取数与动作；
 * 依赖方向是视图 → 本文件 → 桥（与 proxies-state.ts 的拆分同一套约定）。
 *
 * ── 与代理池的关系 ─────────────────────────────────────────
 * 订阅节点由内核镜像进代理池（`source: "subscription"` 的只读条目），本区块
 * 只管「订阅与内核」这一层：添加 / 编辑 / 删除订阅、下载 / 重启内核。节点
 * 本身出现在下面的代理池表格里（proxies-page 的表格，不用这里管）。
 *
 * ── 内核命令是异步的（轮询而不是推送）────────────────────────
 * 写操作（增删改订阅）后端投一条内核命令就立即返回：内核重启 + provider
 * 拉取要几秒到几十秒，同步等会把本地请求挂死。这里在每次写操作后轮询内核
 * 状态几轮（`pollKernelAfterChange`），节点数 / 运行状态随之刷新；下载内核
 * 的进度也靠轮询（`downloadKernel` 自带轮询循环，直到状态离开 downloading）。
 * 不引入第二条推送通道 —— 面板页面的其它区块（余额快照 / 连接数）同样轮询。
 */

import { t } from '../i18n'

/* ─── 后端形态 ───────────────────────────────── */

/** 一条订阅（`api::subscriptions` 的 items 项；写操作返回同形全量列表） */
export type SubscriptionItem = {
  id: string
  name: string
  /** `url` 链接订阅（内核定时拉取）| `file` 本地配置文件（不参与定时刷新） */
  kind: 'url' | 'file'
  /** 链接订阅的地址；本地配置文件恒为空串 */
  url: string
  /** 本地配置文件的**原始路径**（导入时记住的，用于「立即更新」重新导入） */
  sourcePath: string
  enabled: boolean
  createdAt: number
  updatedAt: number
  /** 内核上次同步到的节点数（内核快照注入） */
  nodeCount: number
}

/** 内核状态（`sub_kernel::status_json`） */
export type KernelStatus = {
  installed: boolean
  version: string
  asset?: string
  running: boolean
  controllerPort?: number
  listenerCount: number
  kernelDir: string
  lastError: string | null
  /** 各订阅最后一次成功同步：`subId → {name, count, at}` */
  providerSync?: Record<string, { name?: string; count?: number; at?: number }>
  download: {
    state: 'idle' | 'downloading' | 'done' | 'error'
    received: number
    total: number
    error: string | null
  }
}

export type SubscriptionsPayload = {
  items?: SubscriptionItem[]
  kernel?: KernelStatus
  /** 删除响应里带被删条目的名称（toast 文案用） */
  removed?: string
}

/** `importSubscriptionFile` 的回执（校验通过后的文件元信息 + 文件原文） */
export type ImportedFile = {
  canceled?: boolean
  path?: string
  name?: string
  /** 文件里声明的节点条数（不是内核拉到的数量） */
  declaredNodes?: number
  bytes?: number
  /**
   * 文件原文。导入分两步（先选文件校验、再随表单落库），而落库时后端要的
   * 就是这份内容（服务端可能读不到本机路径，见 `addFileSubscription`）——
   * 所以壳把它一并回给前端暂存。配置文件是几百 KB 级，多一份内存副本换
   * 「校验与落库走同一条写路径」是值的。
   */
  content?: string
  /** 壳读文件失败 / 后端校验失败时的原因 */
  error?: string
}

/* ─── 桥（只声明本区块用到的成员）──────────────── */

export type SubscriptionBridge = {
  getSubscriptions?: () => Promise<SubscriptionsPayload | null | undefined>
  addSubscription?: (payload: Record<string, unknown>) => Promise<SubscriptionsPayload | null | undefined>
  saveSubscription?: (payload: Record<string, unknown>) => Promise<SubscriptionsPayload | null | undefined>
  refreshSubscription?: (id: string) => Promise<SubscriptionsPayload | null | undefined>
  removeSubscription?: (id: string) => Promise<SubscriptionsPayload | null | undefined>
  decodeQrImage?: (payload: { imageBase64: string }) => Promise<{ url?: string } | null | undefined>
  /**
   * 导入本地配置文件：壳弹选文件框、读原文交给后端校验，回执里带路径 /
   * 声明节点数 / **文件原文**（导入分两步，落库时后端要的就是这份内容）。
   * 用户取消选择时回执 `{canceled:true}`。网页端没有「本机路径」，`path` 是空串。
   */
  importSubscriptionFile?: () => Promise<ImportedFile | null | undefined>
  /** 原地替换本地文件订阅的内容（条目 id 不变；`path` 给了就一并换位置） */
  replaceSubscriptionFile?: (payload: { id: string; content: string; path?: string })
    => Promise<SubscriptionsPayload | null | undefined>
  getKernelStatus?: () => Promise<KernelStatus | null | undefined>
  downloadKernel?: () => Promise<KernelStatus | null | undefined>
  restartKernel?: () => Promise<KernelStatus | null | undefined>
}

type SubSharedWindow = {
  workbuddyDesktop?: SubscriptionBridge
  wbApp?: {
    toast?: (message: string, kind?: 'err' | 'ok') => void
  }
  wbConfirm?: {
    ask?: (options: { title?: string; html?: string; okText?: string; okClass?: string }) => Promise<boolean>
  }
}

export function wb(): SubSharedWindow {
  return window as unknown as SubSharedWindow
}

export function toast(message: string, kind?: 'err' | 'ok'): void {
  wb().wbApp?.toast?.(message, kind)
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

/* ─── store（模块级快照 + useSyncExternalStore，照 proxies-state 的形态）─── */

type SubState = {
  data: SubscriptionsPayload | null
  loading: boolean
  error: string
  /** 写操作在途（按钮禁用，防连点） */
  pending: boolean
  /** 正在刷新的订阅 id（行内「立即更新」的文案） */
  refreshing: string | null
}

let state: SubState = {
  data: null,
  loading: false,
  error: '',
  pending: false,
  refreshing: null,
}

const listeners = new Set<() => void>()

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

export function getSnapshot(): SubState {
  return state
}

function patch(partial: Partial<SubState>): void {
  state = { ...state, ...partial }
  listeners.forEach(listener => listener())
}

/** 当前列表（`[]` = 还没读到或一条都没有，用 `state.data` 区分） */
export function subs(): SubscriptionItem[] {
  return state.data?.items ?? []
}

/** 当前内核状态（null = 还没读到） */
export function kernel(): KernelStatus | null {
  return state.data?.kernel ?? null
}

/** 写操作返回的整份列表就地替换（与代理池同一约定） */
function accept(payload: SubscriptionsPayload | null | undefined): void {
  if (!payload || typeof payload !== 'object' || !Array.isArray(payload.items)) return
  patch({ data: payload, error: '' })
}

/* ─── 取数 ───────────────────────────────────── */

export async function loadSection(silent = false): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getSubscriptions !== 'function') {
    patch({ error: t('当前环境不支持订阅（桥接方法缺失）') })
    return
  }
  if (!silent) patch({ loading: true })
  try {
    accept(await bridge.getSubscriptions())
  } catch (error) {
    patch({ error: errorMessage(error) })
  } finally {
    if (!silent) patch({ loading: false })
  }
}

/**
 * 写操作后轮询整个区块几轮：内核命令是异步的（重启 + 拉订阅要几秒），
 * **列表与内核必须一起刷** —— 节点数由内核的同步结果注入列表，只刷 kernel
 * 会让「N 个节点」徽标冻在操作瞬间的 0 上不更新（实测踩过这坑）。
 * 最多 6 轮 × 1.5s；内核状态稳定（不在下载中，且已运行或已报错）就提前收手。
 */
async function pollSectionAfterChange(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getSubscriptions !== 'function') return
  for (let round = 0; round < 6; round += 1) {
    await new Promise(resolve => setTimeout(resolve, 1500))
    try {
      const payload = await bridge.getSubscriptions()
      if (payload && Array.isArray(payload.items)) accept(payload)
      const kernel = payload?.kernel
      if (kernel && kernel.download.state !== 'downloading' && round >= 1) {
        if (kernel.running || kernel.lastError) return
      }
    } catch {
      return
    }
  }
}

/* ─── 订阅动作 ───────────────────────────────── */

export async function addSubscription(name: string, url: string): Promise<SubscriptionsPayload | null> {
  const bridge = wb().workbuddyDesktop
  const data = await bridge?.addSubscription?.({ name: name.trim(), url: url.trim(), enabled: true })
  accept(data)
  toast(t('已新增订阅「{name}」，内核正在同步节点…', { name: name.trim() }))
  void pollSectionAfterChange()
  return data ?? null
}

/**
 * 新增**本地配置文件**订阅。
 *
 * `content` 必须一起提交（不是只给路径）：服务端进程可能就是另一个容器，
 * 不能假定它能读到本机这个路径；而后端的 YAML 校验与写盘都要那份内容。
 * `sourcePath` 给后端用于「立即更新 = 从原路径重新导入」（网页端是空串，
 * 那种情况「立即更新」退回「让内核重读现有文件」）。
 */
export async function addFileSubscription(
  name: string,
  sourcePath: string,
  content: string,
): Promise<SubscriptionsPayload | null> {
  const bridge = wb().workbuddyDesktop
  const data = await bridge?.addSubscription?.({
    name: name.trim(),
    kind: 'file',
    sourcePath,
    content,
    enabled: true,
  })
  accept(data)
  toast(t('已导入「{name}」，内核正在读取节点…', { name: name.trim() }))
  void pollSectionAfterChange()
  return data ?? null
}

/**
 * 导入本地配置文件的第一步：让壳弹选文件框、由后端校验并回执元信息。
 * 用户取消选择时返回 null（不是错误，不 toast）。
 */
export async function pickSubscriptionFile(): Promise<ImportedFile | null> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.importSubscriptionFile !== 'function') {
    throw new Error(t('当前环境不支持导入本地配置文件'))
  }
  const result = await bridge.importSubscriptionFile()
  if (!result || result.canceled) return null
  if (result.error) throw new Error(result.error)
  return result
}

/**
 * 原地替换本地文件订阅的内容（条目 id 不变）。
 *
 * 为什么不「删掉重建」：id 是 mihomo provider 名与账号引用链的一环，
 * 换 id 会让所有引用悬空、订阅节点镜像全部重建。`path` 只在换了文件位置时传，
 * 不传就保留原路径（内容变了、位置没变）。
 */
export async function replaceSubscriptionFile(
  item: SubscriptionItem,
  content: string,
  path?: string,
): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.replaceSubscriptionFile !== 'function') {
    throw new Error(t('当前环境不支持替换本地配置文件'))
  }
  const data = await bridge.replaceSubscriptionFile({ id: item.id, content, path })
  accept(data)
  toast(t('已替换「{name}」的内容，正在让内核重读…', { name: item.name }))
  void pollSectionAfterChange()
}

export async function saveSubscription(
  item: SubscriptionItem,
  payload: { name: string; url: string; enabled: boolean },
): Promise<void> {
  const bridge = wb().workbuddyDesktop
  // kind / sourcePath 必须一起回传：后端 `update` 是整份覆盖语义，而
  // `normalize_item` 对 `kind:'file'` 要求 `sourcePath` 非空 —— 少传一个键，
  // 编辑一条本地文件订阅就会报「请选择要导入的本地配置文件」。
  const data = await bridge?.saveSubscription?.({
    id: item.id,
    kind: item.kind,
    sourcePath: item.sourcePath,
    ...payload,
  })
  accept(data)
  toast(t('已保存「{name}」', { name: payload.name.trim() }))
  void pollSectionAfterChange()
}

/** 行内启停（整份覆盖语义：名称与链接原样回传） */
export async function toggleSubscription(item: SubscriptionItem, enabled: boolean): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.pending) return
  patch({ pending: true })
  try {
    const data = await bridge?.saveSubscription?.({
      id: item.id,
      name: item.name,
      kind: item.kind,
      url: item.url,
      sourcePath: item.sourcePath,
      enabled,
    })
    accept(data)
    toast(enabled ? t('已启用「{name}」', { name: item.name }) : t('已禁用「{name}」', { name: item.name }))
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('保存失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/**
 * 行内「立即更新」：强制拉取订阅内容（后端节点变了会自动重启内核）。
 *
 * 本地配置文件（`kind === 'file'`）的含义不同：后端会先从**原路径**重新读
 * 那份文件写进内核目录，再让内核重读 —— 所以文案分开写。
 */
export async function refreshSubscription(item: SubscriptionItem): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.refreshing || state.pending) return
  patch({ refreshing: item.id })
  try {
    const data = await bridge?.refreshSubscription?.(item.id)
    accept(data)
    toast(item.kind === 'file'
      ? t('已重新导入「{name}」，正在让内核重读…', { name: item.name })
      : t('已刷新「{name}」，正在同步节点…', { name: item.name }))
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('刷新失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ refreshing: null })
  }
}

export async function removeSubscription(item: SubscriptionItem): Promise<void> {
  const bridge = wb().workbuddyDesktop
  const ok = await window.wbConfirm?.ask?.({
    title: t('删除订阅'),
    html: t('确定删除「<strong>{name}</strong>」？它在代理池里的节点会一并移除，引用这些节点的账号会回退直连。', {
      name: item.name || item.id,
    }),
    okText: t('删除'),
    okClass: 'danger',
  })
  if (!ok) return
  if (state.pending) return
  patch({ pending: true })
  try {
    const data = await bridge?.removeSubscription?.(item.id)
    accept(data)
    toast(t('已删除「{name}」', { name: item.name || item.id }))
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('删除失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/* ─── 内核动作 ───────────────────────────────── */

/**
 * 下载内核（带轮询循环：每 1s 读一次状态直到离开 downloading，上限 300s
 * —— 20-40MB 在慢线路上可能要几分钟；期间进度条随 received/total 变）。
 */
export async function downloadKernel(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.downloadKernel !== 'function') {
    toast(t('当前环境不支持订阅（桥接方法缺失）'), 'err')
    return
  }
  if (state.pending) return
  patch({ pending: true })
  try {
    const kernel = await bridge.downloadKernel()
    if (kernel && state.data) patch({ data: { ...state.data, kernel } })
    // 轮询直到下载结束（状态离开 downloading）或超时（300 轮）
    for (let round = 0; round < 300; round += 1) {
      await new Promise(resolve => setTimeout(resolve, 1000))
      const next = await bridge.getKernelStatus?.()
      if (next && state.data) patch({ data: { ...state.data, kernel: next } })
      if (!next || next.download.state !== 'downloading') break
    }
    const finalKernel = state.data?.kernel
    if (finalKernel?.download.state === 'error') {
      toast(t('内核下载失败：{reason}', { reason: finalKernel.download.error || t('未知错误') }), 'err')
    } else if (finalKernel?.download.state === 'done') {
      // 下载完成：后端已自动拉起内核（有启用的订阅时），再轮几轮看运行状态
      toast(t('✅ 内核下载完成，正在启动…'))
      void pollSectionAfterChange()
    }
  } catch (error) {
    toast(t('内核下载失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/** 手动启动 / 重启内核（重建配置；节点数随轮询刷新） */
export async function restartKernel(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.pending) return
  patch({ pending: true })
  try {
    const kernel = await bridge?.restartKernel?.()
    if (kernel && state.data) patch({ data: { ...state.data, kernel } })
    toast(t('内核正在启动…'))
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('内核启动失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/**
 * 从图片解二维码（服务端解码）：图片转 base64 上传，返回订阅链接。
 * @param file 剪贴板 / 文件选择器拿到的图片文件
 * @returns 订阅链接；识别失败时抛错（调用方 toast）
 */
export async function decodeQrImage(file: File): Promise<string> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.decodeQrImage !== 'function') {
    throw new Error(t('当前环境不支持订阅（桥接方法缺失）'))
  }
  const imageBase64 = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result || ''))
    reader.onerror = () => reject(new Error(t('图片读取失败')))
    reader.readAsDataURL(file)
  })
  const data = await bridge.decodeQrImage({ imageBase64 })
  const url = data?.url
  if (!url) throw new Error(t('二维码里没有订阅链接'))
  return url
}

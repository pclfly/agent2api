/**
 * 订阅与内核的状态层（「网络代理 → 订阅」区块与内核弹窗共用）。
 *
 * ── 为什么单独一个文件而不是塞进 proxies-state ─────────────────
 * 两块数据的**刷新时机不同**：代理池在页面上任何时候都可能变（测试结果、
 * Clash 同步），而订阅列表只在用户动作后变（增删改 / 刷新 / 内核状态）。
 * 放进同一个快照会让「每次测个代理就把订阅列表整份重拉」—— 订阅刷新是
 * 网络请求（部分订阅要拉几十 KB），不该被无关动作带着跑。
 *
 * ── 形状按**实际响应**定，不是按旧界面猜 ──────────────────────
 * `GET /api/subscriptions` → `{items, kernel}`：
 *
 *   items[]: {id, name, kind, url, fileName, enabled, nodeCount,
 *             createdAt, updatedAt}
 *   kernel:  {installed, running, version, asset, kernelDir, kernelDirDefault,
 *             listenerCount, controllerPort, testPort, halted, lastError,
 *             download: {state, received, total, error}, downloadProxy,
 *             providerSync: {"<subId>": {at, count, name}}}
 *
 * 这几个字段与旧版订阅界面的假设**已经分叉**：旧界面没有 `nodeCount`（它按
 * 内核同步结果现算），而本版的 `nodeCount` 由后端按 `providerSync` 直接给出。
 * 照抄旧界面会显示 0 —— 所以这里按真形状写。
 */

import { t } from '../i18n'

const BASE = '/api/subscriptions'

/** 一条订阅（形状见模块头）。 */
export type SubscriptionItem = {
  id: string
  name: string
  /** `url` = 链接订阅；`file` = 本地配置文件 */
  kind: string
  url: string
  fileName: string
  /** 节点名过滤词（空格 / 逗号分隔；空串 = 不过滤）。命中的节点不进内核、不进代理池 */
  exclude: string
  enabled: boolean
  /** 本轮内核同步到的节点数（0 = 没拉到 / 还没同步） */
  nodeCount: number
  createdAt: number
  updatedAt: number
}

/** 内核下载进度。 */
export type KernelDownload = {
  /** `idle` / `downloading` / `done` / `failed` */
  state: string
  received: number
  total: number
  error: string | null
}

/** 内核状态（形状见模块头）。 */
export type KernelStatus = {
  installed: boolean
  running: boolean
  version: string
  asset: string
  kernelDir: string
  kernelDirDefault: string
  listenerCount: number
  controllerPort: number | null
  testPort: number | null
  /** 用户显式停过（`halted` = 不自动拉起） */
  halted: boolean
  lastError: string | null
  download: KernelDownload
  /** 每个订阅最近一次同步的结果 */
  providerSync: Record<string, { at: number; count: number; name: string }>
}

export type SubscriptionSnapshot = {
  status: 'idle' | 'loading' | 'ready' | 'error'
  error: string
  items: SubscriptionItem[]
  kernel: KernelStatus | null
  /** 正在提交的动作（禁用按钮用；空串 = 空闲） */
  busy: string
}

const empty: SubscriptionSnapshot = {
  status: 'idle',
  error: '',
  items: [],
  kernel: null,
  busy: '',
}

let snapshot: SubscriptionSnapshot = empty
const listeners = new Set<() => void>()

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function getSnapshot(): SubscriptionSnapshot {
  return snapshot
}

function patch(next: Partial<SubscriptionSnapshot>): void {
  snapshot = { ...snapshot, ...next }
  listeners.forEach(listener => listener())
}

/** 面板请求（与 proxies-state 同一套 cookie + 401 处理口径）。 */
async function panel(path: string, init?: RequestInit): Promise<Response> {
  const response = await fetch(`${BASE}${path}`, {
    credentials: 'same-origin',
    ...init,
    headers: {
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...(init?.headers || {}),
    },
  })
  return response
}

/** 读一次订阅 + 内核状态。 */
export async function loadSubscriptions(): Promise<void> {
  patch({ status: snapshot.status === 'ready' ? 'ready' : 'loading', error: '' })
  try {
    const response = await panel('')
    if (!response.ok) {
      patch({ status: 'error', error: t('读取订阅失败（HTTP {code}）', { code: response.status }) })
      return
    }
    const payload = await response.json()
    applyPayload(payload?.data ?? payload)
  } catch (error) {
    patch({ status: 'error', error: t('读取订阅失败：{error}', { error: String(error) }) })
  }
}

/** 把 `{items, kernel}` 或写接口的 `{items}` 应用进快照。 */
function applyPayload(data: unknown): void {
  if (!data || typeof data !== 'object') {
    patch({ status: 'error', error: t('订阅响应形状不对') })
    return
  }
  const object = data as Record<string, unknown>
  const items = Array.isArray(object.items)
    ? (object.items as SubscriptionItem[]).map(normalizeItem)
    : snapshot.items
  // kernel 只有整份读（GET /）才带；写接口的回包里没有它 —— 那时保留旧值
  const kernel = object.kernel && typeof object.kernel === 'object'
    ? normalizeKernel(object.kernel as Record<string, unknown>)
    : snapshot.kernel
  patch({ status: 'ready', error: '', items, kernel })
}

function normalizeItem(raw: SubscriptionItem): SubscriptionItem {
  return {
    id: String(raw?.id ?? ''),
    name: String(raw?.name ?? ''),
    kind: String(raw?.kind ?? 'url'),
    url: String(raw?.url ?? ''),
    fileName: String(raw?.fileName ?? ''),
    exclude: String(raw?.exclude ?? ''),
    enabled: raw?.enabled !== false,
    nodeCount: Number(raw?.nodeCount) || 0,
    createdAt: Number(raw?.createdAt) || 0,
    updatedAt: Number(raw?.updatedAt) || 0,
  }
}

function normalizeKernel(raw: Record<string, unknown>): KernelStatus {
  const download = (raw.download ?? {}) as Record<string, unknown>
  return {
    installed: raw.installed === true,
    running: raw.running === true,
    version: String(raw.version ?? ''),
    asset: String(raw.asset ?? ''),
    kernelDir: String(raw.kernelDir ?? ''),
    kernelDirDefault: String(raw.kernelDirDefault ?? ''),
    listenerCount: Number(raw.listenerCount) || 0,
    controllerPort: typeof raw.controllerPort === 'number' ? raw.controllerPort : null,
    testPort: typeof raw.testPort === 'number' ? raw.testPort : null,
    halted: raw.halted === true,
    lastError: raw.lastError ? String(raw.lastError) : null,
    download: {
      state: String(download.state ?? 'idle'),
      received: Number(download.received) || 0,
      total: Number(download.total) || 0,
      error: download.error ? String(download.error) : null,
    },
    providerSync: (raw.providerSync ?? {}) as KernelStatus['providerSync'],
  }
}

/** 统一的一次写操作：设 busy → 发请求 → 用回包刷新 → 清 busy。 */
async function mutate(
  action: string,
  path: string,
  body?: unknown,
): Promise<{ ok: boolean; error?: string }> {
  patch({ busy: action })
  try {
    const response = await panel(path, {
      method: 'POST',
      body: body === undefined ? '{}' : JSON.stringify(body),
    })
    const payload = await response.json().catch(() => null)
    if (!response.ok) {
      const message = payload?.error || payload?.data?.error || t('操作失败（HTTP {code}）', { code: response.status })
      return { ok: false, error: String(message) }
    }
    applyPayload(payload?.data ?? payload)
    return { ok: true }
  } catch (error) {
    return { ok: false, error: String(error) }
  } finally {
    patch({ busy: '' })
  }
}

export function addSubscription(input: { name: string; url: string; exclude: string }): Promise<{ ok: boolean; error?: string }> {
  return mutate('add', '/add', input)
}

/**
 * 新增**本地 YAML 文件**订阅（`kind: local`）。
 *
 * ── 为什么传 base64 而不是文本 ──────────────────────────────────
 * 后端 `attach_local_content` 认的是 `contentBase64`（它自己解 base64 并校验
 * UTF-8），传 `content` 会被 `normalize_item` 当成「没带内容」而报
 * 「请先选择要导入的 YAML 文件」。用 FileReader 的 data URL 直接喂最省事。
 */
export function addLocalSubscription(input: {
  name: string
  contentBase64: string
  fileName: string
  exclude: string
}): Promise<{ ok: boolean; error?: string }> {
  return mutate('add', '/add', input)
}

export function saveSubscription(input: {
  id: string
  name: string
  url: string
  /** 节点名过滤词（空格 / 逗号分隔；空串 = 清除过滤） */
  exclude: string
  /** 本地订阅换了文件时带这两项（后端 `attach_local_content` 认 base64） */
  contentBase64?: string
  fileName?: string
}): Promise<{ ok: boolean; error?: string }> {
  return mutate('save', '/save', input)
}

export function removeSubscription(id: string): Promise<{ ok: boolean; error?: string }> {
  return mutate('remove', '/remove', { id })
}

/** 刷新一条订阅（重新拉取内容并把节点写进代理池）。 */
export function refreshSubscription(id: string): Promise<{ ok: boolean; error?: string }> {
  return mutate('refresh', '/refresh', { id })
}

/**
 * 行内启停。
 *
 * ── 为什么必须回传 name / url / kind ──────────────────────────
 * `/save` 走的是**整条重建**（`normalize_item`），`name` 是必填项 —— 只发
 * `{id, enabled}` 会被 400「请填写订阅名称」。后端注释里写明这类调用
 * （「行内启停 / 老客户端只回传 name/url/enabled」）就是要回传这几项。
 *
 * `kind` 也要带上：本地文件订阅的 `url` 是空串，不带 `kind` 时后端按
 * 「无 kind → 沿用旧条目的 kind」处理，但那样在条目刚被改名后存在竞态；
 * 显式带上最稳。
 */
export function setSubscriptionEnabled(item: SubscriptionItem, enabled: boolean): Promise<{ ok: boolean; error?: string }> {
  return mutate('toggle', '/save', {
    id: item.id,
    name: item.name,
    url: item.url,
    kind: item.kind,
    fileName: item.fileName,
    enabled,
  })
}

/** 从二维码截图里解出订阅链接（body 是 data URL 或 base64）。 */
export async function decodeSubscriptionQr(image: string): Promise<{ ok: boolean; url?: string; error?: string }> {
  patch({ busy: 'decode' })
  try {
    // 后端读的键是 `imageBase64`（可带 data URL 前缀，它自己会剥）
    const response = await panel('/decode-qr', { method: 'POST', body: JSON.stringify({ imageBase64: image }) })
    const payload = await response.json().catch(() => null)
    if (!response.ok) {
      return { ok: false, error: String(payload?.error || t('识别失败')) }
    }
    const url = payload?.data?.url ?? payload?.url ?? ''
    return url ? { ok: true, url: String(url) } : { ok: false, error: t('二维码里没有订阅链接') }
  } catch (error) {
    return { ok: false, error: String(error) }
  } finally {
    patch({ busy: '' })
  }
}

// ─── 内核动作 ──────────────────────────────────────────────

export function downloadKernel(proxy?: unknown): Promise<{ ok: boolean; error?: string }> {
  return mutate('download', '/kernel/download', proxy === undefined ? {} : { proxy })
}

export function cancelKernelDownload(): Promise<{ ok: boolean; error?: string }> {
  return mutate('cancel', '/kernel/download/cancel')
}

export function restartKernel(): Promise<{ ok: boolean; error?: string }> {
  return mutate('restart', '/kernel/restart')
}

export function stopKernel(): Promise<{ ok: boolean; error?: string }> {
  return mutate('stop', '/kernel/stop')
}

export function removeKernel(): Promise<{ ok: boolean; error?: string }> {
  return mutate('remove', '/kernel/remove')
}

/** 内核下载进度百分比（`0` 时调用方显示「准备中」而不是 0%）。 */
export function downloadPercent(kernel: KernelStatus | null): number {
  const total = kernel?.download.total ?? 0
  const received = kernel?.download.received ?? 0
  if (!total) return 0
  return Math.min(100, Math.max(0, Math.round((received / total) * 100)))
}

/** 内核三态：未安装 / 已停止 / 运行中。 */
export function kernelPhase(kernel: KernelStatus | null): 'missing' | 'stopped' | 'running' {
  if (!kernel?.installed) return 'missing'
  return kernel.running ? 'running' : 'stopped'
}

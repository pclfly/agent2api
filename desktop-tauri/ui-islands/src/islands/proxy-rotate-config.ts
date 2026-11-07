/**
 * 轮询档（`{source:'rotate'}`）的配置形状：读回表单草稿、构造提交 payload。
 *
 * 单独一个无依赖模块而不是留在 `accounts-proxy-form.tsx`：那个文件是 `.tsx`、
 * 带 React 与 i18n 依赖，`npm test` 进不去。而这里的形状**必须**与后端
 * `core::proxies::normalize_rotate` 逐字段对齐 —— 对错了不会报错，只会静默
 * 变成「保存后策略又弹回 smart」这类难查的偏差。
 *
 * ── 后端归一的口径（`core/proxies.rs` 的 failureMode 一段）─────
 *   · 缺键 = `smart`（安全的那一侧：业务错误换出口照样失败，给好出口记
 *     冷却只会把它白冻住）；
 *   · 只认 `smart` / `all_errors`，其它值直接 400；
 *   · `all: true` 时**不带** `proxyIds`（否则「以后新增的条目要不要进轮询」
 *     就成了两处冲突的状态）。
 */

export type RotateFailureMode = 'smart' | 'all_errors'

/** 构造 payload 需要的最小输入（`ProxyDraft` 是它的超集） */
export type RotateDraftLike = {
  rotateAll: boolean
  rotateIds: string[]
  failureMode?: RotateFailureMode | string | null
}

export type RotatePayload =
  | { source: 'rotate'; all: true; failureMode: RotateFailureMode }
  | { source: 'rotate'; all: false; proxyIds: string[]; failureMode: RotateFailureMode }

/** 认不出的策略一律按 `smart`（后端「认不出按 smart」同一侧） */
export function normalizeFailureMode(value: unknown): RotateFailureMode {
  return value === 'all_errors' ? 'all_errors' : 'smart'
}

/**
 * 草稿 → payload。
 *
 * `rotateAll` 时忽略 `rotateIds`（哪怕表单里还留着上次选的几条）：那几条进
 * payload 会让「全部」失去「以后新增自动加入」的语义。
 *
 * 指定档一条都没选时返回 `null`（调用方给出「至少要选一条」的提示）——
 * 空列表送上去会被后端 400，不如在这里就说清楚。
 */
export function rotatePayloadOf(draft: RotateDraftLike): RotatePayload | null {
  const failureMode = normalizeFailureMode(draft.failureMode)
  if (draft.rotateAll) return { source: 'rotate', all: true, failureMode }
  // 先 trim 再滤空：全空白的 id 送上去会被后端当成一条真 id（它也是 trim 后
  // 判空的），最后变成「选了一条但不存在的代理」
  const ids = [...new Set(
    (draft.rotateIds || []).map(id => String(id).trim()).filter(Boolean),
  )]
  if (!ids.length) return null
  return { source: 'rotate', all: false, proxyIds: ids, failureMode }
}

/**
 * 账号配置 → 表单草稿的轮询部分（`draftOfProxy` 用它）。
 *
 * `all` 缺键按「全部」读：那是这一档的默认语义（老配置没有这个键时，
 * 它当年的意思就是「全部启用的出口」）。
 */
export function rotateDraftOf(config: { all?: boolean; proxyIds?: unknown[]; failureMode?: unknown } | null | undefined): {
  rotateAll: boolean
  rotateIds: string[]
  failureMode: RotateFailureMode
} {
  return {
    rotateAll: config?.all !== false,
    rotateIds: Array.isArray(config?.proxyIds) ? config.proxyIds.map(String) : [],
    failureMode: normalizeFailureMode(config?.failureMode),
  }
}

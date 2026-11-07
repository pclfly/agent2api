export type ForceUpdatePolicy = { required: boolean; minimumVersion: string; targetVersion: string; message: string; source?: string }

export type UpdateChannels = {
  stable: boolean
  beta: boolean
  saved?: boolean
}

export type UpdateCheckPlan = {
  enabled: boolean
  interval: number
  saved?: boolean
}

export type UpdateSettingsMode = 'follow' | 'custom'

type Translator = (key: string, params?: Record<string, string | number>) => string

/**
 * 更新入口的本地策略开关（桌面端与手机端共用一处口径）。
 *
 * ── 为什么要有它 ────────────────────────────────────────────
 * 这一版按本地定制关闭了两类**主动打扰**的更新入口：
 *   · `autoPrompt`：定时检查 / 首屏读到缓存时自动弹「检测到更新」对话框；
 *   · `download`：「下载并安装 / 立即更新 / 安装并重启」这类会从上游取包并
 *     交给系统安装器的动作。
 *
 * 保留的是**只读**那半边：侧栏「设置」上的「新」角标、面板里的「检查更新」按钮、
 * 版本号与更新日志的展示、以及「到 GitHub 发布页」这条外链出口。也就是说用户
 * 仍然看得到「有新版本」，只是决定权完全在他手里 —— 不由程序替他起下载。
 *
 * ── 为什么不做成设置项 ──────────────────────────────────────
 * 它是构建期策略，不是用户偏好：做成设置项就还要在配置层、同步、多端之间各维护
 * 一份，而它服务的只是这一版的分发口径。真要在发布时改回去，把这里两个值翻成
 * `true`、重建前端产物即可（`npm --prefix desktop-tauri/ui-islands run build`、
 * `npm --prefix desktop-tauri/mobile run build`）。
 *
 * 两个读取函数都显式标 `: boolean`：字面量类型 `false` 会让调用点的 `if` 被
 * 收窄成恒假分支，读起来像死代码，宁可退化成普通布尔。
 */
export const UPDATE_SURFACE = {
  /** 是否自动弹「检测到更新」对话框 */
  autoPrompt: false,
  /** 是否提供「下载并安装」一类的下载 / 安装入口 */
  download: false,
} as const

/** 自动弹窗是否开启（`update-modal` / 强制更新门读它） */
export function updateAutoPromptEnabled(): boolean {
  return UPDATE_SURFACE.autoPrompt
}

/** 下载 / 安装入口是否开启（桌面面板与手机端面板读它） */
export function updateDownloadEnabled(): boolean {
  return UPDATE_SURFACE.download
}

/**
 * 「有新版本，但没有可下载的包」时后端给的诊断（check 响应的 `assetProbe`）。
 *
 * ── 为什么需要它 ────────────────────────────────────────────
 * 光凭 `asset` 为空说不清原因，而三种原因的处置办法完全不同：上游压根没传包
 * （等构建 / 自己去发布页下）、只传了别的平台（同上）、被更新源的访问方式挡掉了
 * （去「更新设置」改访问方式）。这三个字段就是用来分开它们的。
 *
 * 字段全可选：升级前落库的那份检查结果里没有它，界面必须能退化到「不说原因」。
 */
export type AssetProbe = {
  /** 这一版 release 上传的文件数（按更新源白名单过滤**前**的清单） */
  assetsTotal?: number
  /** 这些文件出现过的后缀（小写、带点、去重） */
  suffixes?: string[]
  /** 有文件因更新源的访问方式（加速前缀 / 自定义 API）没通过而被丢掉 */
  filteredBySource?: boolean
  /** 本机要找的后缀（桌面安装包；容器内更新是空表） */
  expectedSuffixes?: string[]
  /** 本机要找的完整文件名（容器内更新包；桌面是 null） */
  expectedName?: string | null
}

/**
 * 「为什么没有可下载的包」——toast / 弹窗 / 面板状态行共用这一处口径。
 *
 * 返回的是一句能独立看懂的**原因**，由调用方拼进自己的句子里（桌面与容器的
 * 前半句不同，见 update-panel 与 update-modal）。取不到诊断时退化成「本机要
 * 什么」这一层 —— 至少让排查有方向，而不是只丢一句「没有包」。
 */
export function assetMissingReason(
  probe: AssetProbe | null | undefined,
  options: { selfUpdate?: boolean; fallbackSuffix?: string },
  t: Translator,
): string {
  // ① 被更新源的访问方式过滤掉：文件其实传了，是我们这边把它们挡掉了。
  // 先说这条 —— 它的处置办法（去「更新设置」改访问方式）与另外两种完全不同
  if (probe?.filteredBySource) {
    return t('发布页上的文件被更新源的访问方式过滤掉了，请检查「更新设置」里的 GitHub 访问方式')
  }
  // ② 一个文件都没传：构建失败，或者还在打包上传
  if (probe?.assetsTotal === 0) {
    return t('这一版还没有上传任何文件（构建可能失败，或仍在打包上传）')
  }
  // ③ 传了文件，但没有本机要的那一份 —— 把「有什么」与「要什么」都摆出来。
  // 后端给的 expected 优先（Linux 桌面壳是 [deb, AppImage] 两个候选），
  // 老后端没给时退到调用方按 installerKind 推的那个后缀
  const seen = (probe?.suffixes || []).filter(Boolean).slice(0, 6)
  const wanted = options.selfUpdate
    ? String(probe?.expectedName || '')
    : ((probe?.expectedSuffixes || []).filter(Boolean).join(' / ') || String(options.fallbackSuffix || ''))
  if (seen.length && wanted) {
    return t('这一版只有 {seen}，没有本机需要的 {wanted}', { seen: seen.join(' / '), wanted })
  }
  // ④ 兜底：连「有什么」都不知道，至少说清「本机要什么」
  if (options.selfUpdate) {
    return wanted
      ? t('发布页上没有 {wanted} 这个文件', { wanted })
      : t('发布页上没有对应本机架构的服务端更新包')
  }
  return wanted
    ? t('这一版没有发布本机需要的安装包（{wanted}）', { wanted })
    : t('这一版没有为当前平台提供安装包')
}

export function githubErrorMessage(error: unknown, t: Translator, settings: 'update' | 'plugin'): string {
  const message = error instanceof Error ? error.message : String(error ?? '')
  if (message === 'GitHub 接口限流中，请在「更新设置」的「GitHub 令牌」中填写凭证，以提高请求限额。'
    || message === 'GitHub 接口限流中，请在「插件设置」的「GitHub 令牌」中填写凭证，以提高请求限额。') {
    return settings === 'plugin'
      ? t('GitHub 接口限流中，请在「插件设置」的「GitHub 令牌」中填写凭证，以提高请求限额。')
      : t('GitHub 接口限流中，请在「更新设置」的「GitHub 令牌」中填写凭证，以提高请求限额。')
  }
  const cooldown = /^GitHub 接口限流中，请在 (\d+) 秒后重试$/.exec(message)
  if (cooldown) return t('GitHub 接口限流中，请在 {seconds} 秒后重试', { seconds: cooldown[1] })
  if (message === 'GitHub 接口限流中，请稍后重试') return t('GitHub 接口限流中，请稍后重试')
  return message
}

/**
 * 代理池条目「能不能被账号选中」的判定。
 *
 * 单独一个文件而不是留在 `accounts-shared.ts`：那份模块带 `../i18n` 依赖，
 * 而这个判定是纯数据逻辑（后端 `proxy_pool::should_enable_on_reference` 的
 * 前端镜像），放到无依赖的模块里才能被 `npm test` 直接覆盖 —— 两边口径分叉
 * 的症状是「灰着但能选中后失败」或「明明能选却被挡住」，只靠肉眼看代码发现不了。
 */

/** 判定只需要这几个字段（`PoolItem` 是它的超集） */
export type SelectablePoolItem = {
  source?: string
  enabled?: boolean
  /** 来源侧是否启用（Clash 镜像值；订阅与手动条目恒为 true） */
  sourceEnabled?: boolean
  /** 本页额外禁用（只出现在 Clash 条目上；`enabled` 已经把这一层算进去了） */
  localDisabled?: boolean
  /** 订阅里已移除该节点：条目留着只为不悬空账号引用，启用也拿不到端口 */
  gone?: boolean
}

/**
 * 这条池条目**能不能被账号选中**。
 *
 * 禁用本身不再是拒绝理由：账号指定一条被本页禁用的条目时，后端会顺手把它
 * 启用（见 `proxy_pool::enable_for_reference`）。真正选不了的只有两类 ——
 *   · **订阅里已移除的节点**（`gone`）：节点定义都没了，启用也永远拿不到端口；
 *   · **Clash Verge 里停用的监听器**：本页的启用开关对它不生效，改请到
 *     Clash Verge 里打开。
 * 两类都必须**列出来并说明原因**，而不是凭空消失：用户会以为池里没这条。
 */
export function poolItemSelectable(item: SelectablePoolItem): boolean {
  if (item.gone === true) return false
  return !(item.source === 'clash' && item.sourceEnabled === false)
}

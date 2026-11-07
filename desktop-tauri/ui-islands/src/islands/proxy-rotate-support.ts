/**
 * 「这条账号能不能用出网轮询」的判据。
 *
 * 单独一个无依赖模块而不是留在 `accounts-domain.ts`：那份模块带 `../i18n` 与
 * `shared()`（读 window）依赖，而这个判定是纯数据逻辑 —— 放到无依赖的模块
 * 里才能被 `npm test` 直接覆盖。
 *
 * ── 这里曾经只放行 OpenCode Zen ────────────────────────────
 * 早期轮询是为「免费档按 IP 限流」那一家做的，判据写死成
 * `isOpencodeZenProvider(...)`，于是其余所有账号的下拉里都看不到「轮询」档 ——
 * 用户看到的症状是「为什么账号代理还是没有轮询选项」。
 *
 * 但后端的 `core::proxy_rotate` 从来就是通用的：它只认 `{source:"rotate"}`
 * 这个配置形状（`core::proxies::is_rotate_config`），按账号 id 存游标、与
 * 提供商无关，`store.rs` 解析出口时对所有账号一视同仁。所以那个限制是前端
 * 多出来的一层，不是后端的能力边界。
 *
 * 现在与官方实现对齐：**有账号即可**。
 */

/** 判定只需要「这是一条真实账号」——传 null / undefined 表示列表还没到或已删除 */
export type RotateAccountLike = { id?: string; proxy?: unknown } | null | undefined

/**
 * 这条账号能不能用「轮询」档。
 *
 * 只排除「没有账号」这一种情况：那种场景连表单都不该打开（首屏列表未到、
 * 账号刚被删）。任何**存在的**账号都可以轮询 —— 不按提供商、套餐或 IP 额度
 * 设限，与后端 `proxy_rotate` 的通用语义一致。
 */
export function supportsProxyRotate(account: RotateAccountLike): boolean {
  return Boolean(account)
}

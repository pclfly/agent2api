//! WorkBuddy 出站身份（WB / VSC / CLI）—— 三种客户端指纹与配额通道。
//!
//! 参考 `workbuddy2api-hub` 的 `wb_identity.py`（同口径移植，值照抄实测）：
//!
//! | 身份 | X-IDE-Type | X-Product | UA 形态 | 端点（国内版） |
//! |------|-----------|-----------|---------|----------------|
//! | WB（默认） | WorkBuddy | WorkBuddy / WorkBuddy AI | `WorkBuddy/… WorkBuddy AI/… CLI/…` | copilot.tencent.com |
//! | VSC | VSCode | SaaS | `VSCode/1.119.0 WorkBuddy/4.9.…` | **www.workbuddy.cn** |
//! | CLI | CLI | SaaS | `CLI/2.117.2 CodeBuddy/2.117.2` | copilot.tencent.com |
//!
//! 国际版三种身份共用 `www.workbuddy.ai`（与 `endpoints.rs` 的版本默认一致）。
//!
//! ── 为什么要分身份 ──────────────────────────────────────────
//! 上游按客户端身份**分别计额**：桌面端、VSCode 插件、CLI 各是一条配额线。
//! 同一个账号在一条线上被限流时，换身份重试可能直接成功（hub 的
//! `_try_switch_product` 就是干这个的）。身份是**账号属性**（账号记录里的
//! `product` 字段），缺省 WB —— 与 hub 的 `account.product` 同一形态。
//!
//! ── 与 `EditionInfo`（版本）的关系 ──────────────────────────
//! 版本（cn / intl）回答「打哪个网关、用哪套语言」，身份回答「以哪个客户端
//! 的样子出站」。两者正交：同一个账号 = 一个版本 × 一个身份。本模块只负责
//! 身份那一维；版本相关的值（`client_version` 等）由调用方传入。
//!
//! ── 头集合的组装位置 ────────────────────────────────────────
//! 本模块只产出**身份头**（不含鉴权）。`X-User-Id` / `Authorization` /
//! `X-Enterprise-Id` 等由 `AuthService::build_auth_headers` 在调用方补上 ——
//! 两种头在 `chat_headers` 里合并（同名去重，见那里的说明）。

use serde_json::Value;

use crate::server::core::endpoints::EditionInfo;

/// 出站身份。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Product {
    /// WorkBuddy 独立桌面客户端（默认）
    Wb,
    /// 官方 VSCode 插件
    Vsc,
    /// 官方 CodeBuddy CLI
    Cli,
}

/// 账号记录里的 `product` 取值（落盘契约；缺省 = wb）。
pub const PRODUCT_FIELD: &str = "product";

/// 版本常量（照抄 hub 的实测值；换版本号只改这一处）。
const CLI_IDE_VERSION: &str = "2.117.2";
const VSC_IDE_VERSION: &str = "1.119.0";
const VSC_PRODUCT_VERSION: &str = "4.9.29177644";

/// 宽松解析身份：接受多种写法（大小写不敏感），未知值回落 WB
/// （与 hub 的 `normalize_product` 同一口径 —— 老账号没有这个字段 = WB）。
pub fn resolve_product(value: Option<&str>) -> Product {
    let text = value.unwrap_or("").trim().to_lowercase();
    match text.as_str() {
        "" | "wb" | "workbuddy" | "desktop" => Product::Wb,
        "vsc" | "vscode" | "ide" => Product::Vsc,
        "cli" | "codebuddy" => Product::Cli,
        _ => Product::Wb,
    }
}

/// 身份的落盘 id（`"wb"` / `"vsc"` / `"cli"`）
pub const fn product_id(product: Product) -> &'static str {
    match product {
        Product::Wb => "wb",
        Product::Vsc => "vsc",
        Product::Cli => "cli",
    }
}

/// 身份的展示名（界面用）
pub const fn product_label(product: Product) -> &'static str {
    match product {
        Product::Wb => "WorkBuddy 桌面端",
        Product::Vsc => "VSCode 插件",
        Product::Cli => "CodeBuddy CLI",
    }
}

/// 身份对应的端点覆盖（`None` = 用账号/版本的默认端点）。
///
/// 照抄 hub 的 `_ENDPOINTS` 表：唯一与版本默认不同的一项是
/// **国内版 + VSCode 插件 → `www.workbuddy.cn`**（其余都与版本默认一致，
/// 返回 None 让调用方走既有回落链，避免把「没有覆盖」伪装成「有覆盖」）。
pub fn identity_endpoint(edition: &EditionInfo, product: Product) -> Option<&'static str> {
    match (edition.id, product) {
        ("cn", Product::Vsc) => Some("https://www.workbuddy.cn"),
        _ => None,
    }
}

/// 身份对应的 `X-Domain` 值（域名，不带协议）。
///
/// 照抄 hub 的 `endpoint_for(realm, product)[1]`：
///   - 国内版：WB/CLI → `copilot.tencent.com`；VSC → `www.workbuddy.cn`；
///   - 国际版：三种身份 → `www.workbuddy.ai`。
pub fn identity_domain(edition: &EditionInfo, product: Product) -> &'static str {
    match (edition.id, product) {
        ("cn", Product::Vsc) => "www.workbuddy.cn",
        ("cn", _) => "copilot.tencent.com",
        _ => "www.workbuddy.ai",
    }
}

/// 构造**身份头**（不含鉴权与会话追踪之外的业务头）。
///
/// `request_id` 是本次转发的追踪 id（调用方生成）——三个身份都把它同时用作
/// 会话与请求追踪（与 `chat_headers` 的既有口径一致）。
///
/// ── 与 hub 的一处刻意差异 ───────────────────────────────────
/// WB 身份**沿用 agent2api 既有头集**（`X-Agent-Intent: craft` + 四个追踪头），
/// 不照抄 hub 的 `X-Agent-Purpose: conversation`：既有头集是 Node 版逐字移植
/// 并经线上验证的，换头有回归风险；VSC / CLI 才按 hub 的新口径构造。
pub fn identity_headers(
    product: Product,
    edition: &EditionInfo,
    request_id: &str,
) -> Vec<(String, String)> {
    match product {
        Product::Wb => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                crate::server::core::endpoints::user_agent_for_edition(Some(edition.id)),
            ),
            ("X-IDE-Type".to_string(), edition.ua_platform.to_string()),
            ("X-IDE-Name".to_string(), edition.product_name.to_string()),
            ("X-IDE-Version".to_string(), edition.client_version.to_string()),
            ("X-Product".to_string(), edition.product_name.to_string()),
            ("X-Agent-Intent".to_string(), "craft".to_string()),
            ("X-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-ID".to_string(), request_id.to_string()),
            ("X-Session-ID".to_string(), request_id.to_string()),
        ],
        Product::Vsc => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                format!("VSCode/{VSC_IDE_VERSION} WorkBuddy/{VSC_PRODUCT_VERSION}"),
            ),
            ("X-Agent-Intent".to_string(), "craft".to_string()),
            ("X-IDE-Type".to_string(), "VSCode".to_string()),
            ("X-IDE-Name".to_string(), "VSCode".to_string()),
            ("X-IDE-Version".to_string(), VSC_IDE_VERSION.to_string()),
            ("X-Product-Version".to_string(), VSC_PRODUCT_VERSION.to_string()),
            ("X-Env-ID".to_string(), "production".to_string()),
            ("X-Product".to_string(), "SaaS".to_string()),
            (
                "X-Domain".to_string(),
                identity_domain(edition, product).to_string(),
            ),
            ("X-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-ID".to_string(), request_id.to_string()),
            ("X-Conversation-Message-ID".to_string(), request_id.to_string()),
        ],
        Product::Cli => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                format!("CLI/{CLI_IDE_VERSION} CodeBuddy/{CLI_IDE_VERSION}"),
            ),
            ("X-Agent-Intent".to_string(), "craft".to_string()),
            ("X-IDE-Type".to_string(), "CLI".to_string()),
            ("X-IDE-Name".to_string(), "CLI".to_string()),
            ("X-IDE-Version".to_string(), CLI_IDE_VERSION.to_string()),
            ("X-Product".to_string(), "SaaS".to_string()),
            (
                "X-Domain".to_string(),
                identity_domain(edition, product).to_string(),
            ),
            ("X-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-Request-ID".to_string(), request_id.to_string()),
            ("X-Conversation-ID".to_string(), request_id.to_string()),
            ("X-Conversation-Message-ID".to_string(), request_id.to_string()),
        ],
    }
}

/// 从账号记录（会话对象）解析身份。
pub fn product_of(session: &Value) -> Product {
    resolve_product(session.get(PRODUCT_FIELD).and_then(Value::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::endpoints::resolve_edition;

    #[test]
    fn resolves_aliases_and_falls_back_to_wb() {
        assert_eq!(resolve_product(None), Product::Wb);
        assert_eq!(resolve_product(Some("")), Product::Wb);
        assert_eq!(resolve_product(Some("WB")), Product::Wb);
        assert_eq!(resolve_product(Some("workbuddy")), Product::Wb);
        assert_eq!(resolve_product(Some("desktop")), Product::Wb);
        assert_eq!(resolve_product(Some("vsc")), Product::Vsc);
        assert_eq!(resolve_product(Some("VSCode")), Product::Vsc);
        assert_eq!(resolve_product(Some("cli")), Product::Cli);
        assert_eq!(resolve_product(Some("CodeBuddy")), Product::Cli);
        assert_eq!(resolve_product(Some("unknown")), Product::Wb);
    }

    #[test]
    fn vsc_cn_uses_workbuddy_cn_endpoint_and_domain() {
        let cn = resolve_edition(Some("cn"));
        let vsc = resolve_product(Some("vsc"));
        assert_eq!(
            identity_endpoint(cn, vsc),
            Some("https://www.workbuddy.cn")
        );
        assert_eq!(identity_domain(cn, vsc), "www.workbuddy.cn");
        // 其余组合无覆盖 / 走版本默认域
        assert_eq!(identity_endpoint(cn, Product::Wb), None);
        assert_eq!(identity_endpoint(cn, Product::Cli), None);
        assert_eq!(identity_domain(cn, Product::Wb), "copilot.tencent.com");
        assert_eq!(identity_domain(cn, Product::Cli), "copilot.tencent.com");
        let intl = resolve_edition(Some("intl"));
        assert_eq!(identity_domain(intl, Product::Vsc), "www.workbuddy.ai");
        assert_eq!(identity_endpoint(intl, Product::Cli), None);
    }

    #[test]
    fn identity_headers_carry_distinct_fingerprints() {
        let cn = resolve_edition(Some("cn"));
        let header = |product, name: &str| -> String {
            identity_headers(product, cn, "req-1")
                .into_iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value)
                .unwrap_or_default()
        };
        // WB：保持既有头集
        assert_eq!(header(Product::Wb, "X-IDE-Type"), "WorkBuddy");
        assert_eq!(header(Product::Wb, "X-Agent-Intent"), "craft");
        // VSC
        assert_eq!(header(Product::Vsc, "X-IDE-Type"), "VSCode");
        assert_eq!(header(Product::Vsc, "X-Product"), "SaaS");
        assert_eq!(header(Product::Vsc, "X-IDE-Version"), "1.119.0");
        assert_eq!(
            header(Product::Vsc, "User-Agent"),
            "VSCode/1.119.0 WorkBuddy/4.9.29177644"
        );
        assert_eq!(header(Product::Vsc, "X-Domain"), "www.workbuddy.cn");
        // CLI
        assert_eq!(header(Product::Cli, "X-IDE-Type"), "CLI");
        assert_eq!(header(Product::Cli, "X-Product"), "SaaS");
        assert_eq!(
            header(Product::Cli, "User-Agent"),
            "CLI/2.117.2 CodeBuddy/2.117.2"
        );
        assert_eq!(header(Product::Cli, "X-Domain"), "copilot.tencent.com");
    }
}

//! 账号出口解析：维护链路（签到 / 余额 / 目录 / 续期 / 健康探测）共用一份。
//!
//! ── 为什么单独一个模块 ──────────────────────────────────────
//! 每条维护链路都要回答同一个问题：这条账号记录这次该从哪个出口出去。
//! 各写一遍必然分叉 —— 有的悄悄回退直连、有的把 `frontProxy` 忘掉。
//! 这里只有一条规则：**配置了代理就必须用，解析不出来就报错**，
//! 且 `frontProxy` 与 `proxy` 会先在本机建两跳中转，再交给 reqwest 使用
//! （`core::proxy_chain` 的既有语义）。
//!
//! ── 与兼容路径的边界 ────────────────────────────────────────
//! `proxies::session_proxy` 与 `egress::client_for` 保留「解析失败回退直连」
//! 的旧契约，转发链路依赖它。维护链路不能沿用：一次静默直连就会把账号
//! 的真实出口 IP 暴露给上游。所以这里用 `ProxyResolution` 显式区分失败。
//!
//!
//! ── 本模块在 v3.4.0 架构里**没有调用点**（这是有意的）──────────
//! 新版把签到 / 余额 / 目录 / 续期整体交给提供商插件执行，宿主侧不再自己
//! 发这些维护请求（唯一残留的保活链只做记账，不出网）。所以「维护链路必须
//! 走账号配置的出口」这条保证**换了载体**：
//!
//! ```text
//! 宿主 plugin_account()  →  {state:"failed", message}
//!   → 插件 StoreProxy::Failed
//!   → 插件 proxy_util::resolve_account_proxy 返回 Failed
//!   → 各家的维护请求直接报错（不回退直连）
//! ```
//!
//! 也就是说：**严格的语义留在了宿主的代理解析里，执行侧搬到了插件**。
//! 本模块保留为契约参考（宿主将来若重新承担维护链路，直接用这里的
//! `resolve` / `client` / `send_json`，不要另写一份）。
//!
//! 判据：给本模块加调用点之前先确认「宿主真的在发维护请求」；否则加了也
//! 只是把死代码变成更复杂的死代码。
//! ── 未配置代理 = 直连 ───────────────────────────────────────
//! 返回 `Ok(None)` 是**明确的直连**（`client` 会给 `no_proxy` 客户端），
//! 不跟随 `HTTPS_PROXY` 等环境变量 —— 维护请求的出口必须由账号配置决定。

use std::sync::Arc;

use serde_json::Value;

use crate::server::core::{egress, proxy_chain, proxies};
use crate::server::core::proxies::{ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;

/// 每个供应商的文案前缀（错误里要能看出是哪一家、哪个字段）。
fn label_of(provider: &str) -> &'static str {
    match provider {
        "workbuddy" => "WorkBuddy",
        "workbuddy-intl" => "WorkBuddy 国际版",
        "raccoon" => "小浣熊",
        "catpaw" => "CatPaw",
        "autoclaw" | "autoclaw-intl" => "AutoClaw",
        "qoder" | "qoder-intl" => "Qoder",
        "cline-free" | "cline-pass" => "Cline",
        "accio" | "accio-cn" => "Accio",
        "zcode" | "zcode-intl" => "ZCode",
        "codearts" => "CodeArts",
        "trae" => "Trae",
        "lobsterai" => "LobsterAI",
        "loomy" => "Loomy",
        "kuku" => "KukuAI",
        "minimax" => "MiniMax",
        "monkeycode" | "monkeycode-intl" => "MonkeyCode",
        "commandcode" => "Command Code",
        "antigravity" => "Antigravity",
        "officeace" => "OfficeAce",
        "qwenwork" => "QwenWork",
        "orcaterm" => "OrcaTerm",
        "chatglm" => "智谱清言",
        _ => "该账号",
    }
}

fn resolve_field(
    record: &Value,
    field: &str,
    sticky: Option<&str>,
) -> Result<Option<ResolvedProxy>, GatewayError> {
    let provider = record.get("provider").and_then(Value::as_str).unwrap_or("");
    let label = label_of(provider);
    let Some(config) = record.get(field).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    // 归一化只做校验；轮询配置必须用原值查粘性选择（归一会给老记录补默认键，
    // 与 warm 记录的相等判定对不上，见 `proxy_rotate::warm`）。
    let Some(normalized) = proxies::normalize_account_proxy(config)
        .map_err(|reason| GatewayError::with_status(400, format!("{label} {field} 配置无效：{reason}")))?
    else {
        return Ok(None);
    };
    let is_rotate = proxies::is_rotate_config(config);
    let config = if is_rotate { config } else { &normalized };
    let resolution = match sticky {
        Some(key) => proxies::resolve_account_proxy_rotating(Some(config), key),
        None => proxies::resolve_account_proxy(Some(config)),
    };
    match resolution {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(
            502,
            format!("{label} {field} 不可用，已停止请求（不回退直连）：{reason}"),
        )),
        // 配置存在却解析不出出口：同样是失败，绝不能当成直连。
        None => Err(GatewayError::with_status(
            502,
            format!("{label} {field} 已配置但没有可用出口"),
        )),
    }
}

/// 这条账号记录的维护出口。
///
/// - `Ok(None)`：账号没有配代理，明确直连；
/// - `Ok(Some(p))`：走 `p`（如果配了 `frontProxy`，`p` 是本机两跳中转端点）；
/// - `Err`：配了但不可用 —— 调用方必须停止请求。
pub fn resolve(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    let label = label_of(record.get("provider").and_then(Value::as_str).unwrap_or(""));
    let sticky = record.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
    // 后置跳用账号 id 做粘性键（同一账号跨请求走同一个节点）；前置跳固定不推进游标。
    let back = resolve_field(record, "proxy", sticky)?;
    let front = resolve_field(record, "frontProxy", None)?;
    egress::validate_account_route(back.as_ref(), front.as_ref())
        .map_err(|reason| GatewayError::with_status(400, format!("{label} 代理配置无效：{reason}")))?;
    chain(back, front).map_err(|error| {
        GatewayError::with_status(error.status_code, format!("{label}：{}", error.message))
    })
}

/// 严格取客户端：构造失败返回 Err，绝不回退直连或系统代理。
///
/// 与 `egress::try_client_for_with_tls` 同一份缓存与超时；`None` 出口时
/// 仍是显式 `no_proxy` 的客户端。
pub fn client(record: &Value, _accept_any_cert: bool) -> Result<Arc<reqwest::Client>, GatewayError> {
    let provider = record.get("provider").and_then(Value::as_str).unwrap_or("");
    let label = label_of(provider);
    // 这里只给「已解析好的单跳/两跳中转」，两跳的中转在 `resolve` 里已经建好，
    // 所以直接走 `client_for`（构造失败由它内部兜底成直连客户端）。
    //
    // 注意：本函数**不**回退直连 —— 走到这里的 record 已经过 `resolve`，
    // 代理配错会先在那里报错。
    let route = resolve(record)?;
    Ok(egress::client_for(route.as_ref()))
}

/// 发一次 JSON 控制面请求：出口由账号记录决定，客户端构造失败直接报错。
///
/// 与 `auth_http::send_raw`（**解析失败回退直连**）刻意不同 —— 维护链路不能
/// 因为代理坏了就把账号的真实出口 IP 暴露出去。响应形状沿用 `ApiResponse`，
/// 调用方的判定链不必改。
pub async fn send_json(
    record: &Value,
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    timeout_ms: Option<u64>,
) -> Result<crate::server::core::auth_http::ApiResponse, GatewayError> {
    let provider = record.get("provider").and_then(Value::as_str).unwrap_or("");
    let label = label_of(provider);
    let http = client(record, false)?;
    let verb = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| GatewayError::with_status(400, format!("{label} 请求方法无效")))?;
    let mut builder = http.request(verb, url).header("Accept", "application/json");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    if let Some(payload) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(payload.to_string());
    }
    if let Some(timeout) = timeout_ms {
        builder = builder.timeout(std::time::Duration::from_millis(timeout));
    }
    let response = builder.send().await.map_err(|error| {
        GatewayError::with_status(502, format!("{label} 请求发不出去：{}", egress::describe_error_detail(&error)))
    })?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let text = response.text().await.map_err(|error| {
        GatewayError::with_status(502, format!("{label} 响应读取失败：{}", egress::describe_error_detail(&error)))
    })?;
    let payload = if text.trim().is_empty() { None } else { serde_json::from_str(&text).ok() };
    Ok(crate::server::core::auth_http::ApiResponse { status, ok, payload })
}

/// 把已解析的一对跳组装成最终出口（会话形态调用方用）。
///
/// 转发链与保活链路手里只有**已解析**的 `ResolvedProxy`（session.proxy），
/// 需要与原始 `frontProxy` 组合时用这个，规则与 [`resolve`] 完全一致。
pub fn chain(
    back: Option<ResolvedProxy>,
    front: Option<ResolvedProxy>,
) -> Result<Option<ResolvedProxy>, GatewayError> {
    egress::validate_account_route(back.as_ref(), front.as_ref())
        .map_err(|reason| GatewayError::with_status(400, format!("代理配置无效：{reason}")))?;
    let Some(front) = front else { return Ok(back) };
    let Some(back) = back else {
        return Err(GatewayError::with_status(
            400,
            "frontProxy 需要同时配置 proxy（第二跳代理）",
        ));
    };
    let endpoint = proxy_chain::chain_endpoint(&front, &back)
        .ok_or_else(|| GatewayError::with_status(502, "两跳代理链建立失败，已停止请求"))?;
    let address: std::net::SocketAddr = endpoint
        .local_addr
        .parse()
        .map_err(|_| GatewayError::with_status(502, "代理链本地端点无效"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(GatewayError::with_status(502, "代理链必须使用有效的本机端点"));
    }
    Ok(Some(ResolvedProxy {
        source: "custom".to_string(),
        protocol: "http".to_string(),
        host: address.ip().to_string(),
        port: Some(address.port()),
        username: String::new(),
        password: String::new(),
        label: endpoint.label,
    }))
}

/// 会话形态（`session.proxy` 已解析 + `session.frontProxy` 原始配置）的出口。
///
/// 供只有会话、没有账号记录的链路使用（WorkBuddy 保活等）。`frontProxy`
/// 配置损坏或解析不出来时返回 Err —— 与 [`resolve`] 同一纪律。
pub fn from_session(session: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    let back = match ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null)) {
        Ok(proxy) => proxy,
        Err(reason) => {
            return Err(GatewayError::with_status(
                502,
                format!("账号代理不可用，已停止请求（不回退直连）：{reason}"),
            ))
        }
    };
    let front = match session.get("frontProxy").filter(|value| !value.is_null()) {
        None => None,
        Some(config) => match proxies::resolve_account_proxy(Some(config)) {
            Some(ProxyResolution::Resolved(proxy)) => Some(proxy),
            Some(ProxyResolution::Failed(reason)) => {
                return Err(GatewayError::with_status(
                    502,
                    format!("frontProxy 不可用，已停止请求（不回退直连）：{reason}"),
                ))
            }
            None => None,
        },
    };
    chain(back, front)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_proxy_is_explicit_direct_and_never_errors() {
        assert!(resolve(&json!({"provider":"cline-free"})).unwrap().is_none());
        assert!(resolve(&json!({"provider":"officeace","proxy":null})).unwrap().is_none());
    }

    #[test]
    fn broken_proxy_never_becomes_direct() {
        for record in [
            json!({"provider":"autoclaw","proxy":{"source":"custom","protocol":"http","host":"127.0.0.1","port":0}}),
            json!({"provider":"officeace","proxy":{"source":"pool","proxyId":""}}),
            json!({"provider":"loomy","proxy":{"source":"unknown"}}),
            json!({"provider":"commandcode","proxy":{"source":"custom","protocol":"ftp","host":"127.0.0.1","port":1}}),
        ] {
            assert!(resolve(&record).is_err(), "{record}");
        }
    }

    #[test]
    fn front_proxy_requires_a_second_hop_and_http() {
        let only_front = json!({"provider":"cline-free","frontProxy":{"source":"custom","protocol":"http","host":"127.0.0.1","port":1234}});
        assert!(resolve(&only_front).unwrap_err().message.contains("frontProxy"));
        let socks_front = json!({
            "provider":"cline-free",
            "proxy":{"source":"custom","protocol":"http","host":"127.0.0.1","port":1235},
            "frontProxy":{"source":"custom","protocol":"socks5","host":"127.0.0.1","port":1234},
        });
        assert!(resolve(&socks_front).unwrap_err().message.contains("代理配置无效"));
    }

    #[test]
    fn explicit_proxy_resolves_without_network() {
        let record = json!({"provider":"minimax","proxy":{"source":"custom","protocol":"http","host":"127.0.0.1","port":3128,"username":"u","password":"p"}});
        let route = resolve(&record).unwrap().unwrap();
        assert_eq!(route.host, "127.0.0.1");
        assert_eq!(route.port, Some(3128));
        assert_eq!(route.username, "u");
        // 客户端构造不建立连接
        assert!(client(&record, false).is_ok());
        assert!(client(&json!({"provider":"minimax"}), false).is_ok());
    }
}

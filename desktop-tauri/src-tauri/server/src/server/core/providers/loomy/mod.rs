//! Loomy（讯飞办公助手）适配器。
//!
//! ── 上游长什么样（移植来源：参考实现 `src/loomy*.ts`）──────────
//!   - 对话：`POST {apiBase}/chat/completions`，**标准 OpenAI 兼容 +
//!     标准 SSE**（`data:` 帧 + 空 `data:` 终止帧，思考在
//!     `delta.reasoning_content`，无加密、无信封、无格式转换）。
//!     因此走**无状态**转发路径（与 raccoon / autoclaw 同构）。
//!   - 认证：chat 端点只认 `Authorization: Bearer`，业务端点只认小写
//!     `token` 头 —— chat 请求**两个都发**（官方客户端同款，见 `client.rs`）。
//!   - 登录：短信验证码（`core::login` 的 sms 分派 + `api/session` 两条路由），
//!     签名见 `sign.rs`。
//!   - **没有 refresh 端点**：`supports_refresh` / `credentials_expiring`
//!     恒 false（诚实标记；14 天后重新短信登录）。
//!
//! ── 思考档位 ────────────────────────────────────────────────
//! 远端 `GET /models` 直接下发 `reasoning_efforts`（实测 8 个模型完全一致：
//! `none/low/medium/high/xhigh`）与 `default_reasoning_effort`。
//! 注入字段是 `reasoning_effort`，取值必须在该模型的 efforts 内 ——
//! `none` 是官方档位之一（「关闭思考」），有实测支撑，因此本家对 `off`/`none`
//! 的处理是**查档位表**而不是一律 Skip。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   product.rs      常量（双基址 / 签名密钥 / 兜底模型表）
//!   sign.rs         HMAC-SHA1 签名（9 段拼串，纯函数 + 单测锁死）
//!   credentials.rs  凭据结构 + 信封解析 + 展示名倍率规范化
//!   client.rs       两套认证头 + 账号请求体 + JSON 请求工具
//!   oauth.rs        短信发码 / 校验登录
//!   models.rs       模型目录（type=chat 过滤 + 兜底 8 条 + 档位元数据）
//!   checkin.rs      first-login 签到（幂等判据 alreadyProcessed）
//!   balance.rs      两池积分（records 只读）
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic。

use std::pin::Pin;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, ReasoningPatch, UpstreamErrorClass,
};
use super::{kind_id, ProviderKind};

pub mod balance;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod product;
pub mod sign;

/// 提供商 id（注册表与 `ProviderKind` 用同一条）。
pub const PROVIDER_ID: &str = super::kind_id(ProviderKind::Loomy);

/// Loomy 适配器（无状态单例）。
pub struct LoomyAdapter;

/// 进程级实例（`adapter_for` 返回它的引用）。
pub static LOOMY_ADAPTER: LoomyAdapter = LoomyAdapter;

impl LoomyAdapter {
    /// 取某账号的凭据快照（`account_id` 为空 = 这一家的队首可用账号）。
    fn snapshot_for(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
        let record = store.loomy_account_record(account_id);
        if !account_id.is_empty() && record.is_none() {
            return Err(GatewayError::with_status(404, "账号不存在"));
        }
        record.ok_or_else(|| {
            GatewayError::with_status(
                401,
                format!("没有可用的 {} 账号", kind_id(ProviderKind::Loomy)),
            )
        })
    }
}

impl ProviderAdapter for LoomyAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Loomy
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {apiBase}/chat/completions`（body 原样透传）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::Credential::from_value(account);
        if !credential.valid() {
            return Err(GatewayError::with_status(401, "Loomy 账号缺少 session，无法转发"));
        }
        let headers = client::chat_headers(&credential.access_token);
        let url = format!("{}{}", product::API_BASE, product::CHAT_PATH);
        Ok(ChatRequestPlan::chat(url, headers, body.clone()))
    }

    /// 思考档位翻译：注入 `reasoning_effort`（取值必须在该模型的 efforts 内）。
    ///
    /// `off` / `none`：Loomy 的官方档位表里就有 `none`（「关闭思考」），
    /// 因此查表命中就注入，不套用「一律不注入」的全局判据（本家有实测支撑）。
    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> ReasoningPatch {
        // 客户端已显式指定时不覆盖
        if crate::server::core::model_rules::read_client_level(body).is_some() {
            return ReasoningPatch::Skip { reason: "客户端请求体里已指定思考等级" };
        }
        let Some(efforts) = models::efforts_of(model) else {
            return ReasoningPatch::Skip { reason: "该模型没有思考档位声明（远端未下发 reasoning_efforts）" };
        };
        let wanted = if crate::server::core::model_rules::reasoning_is_off(level) {
            "none"
        } else {
            level
        };
        if efforts.iter().any(|item| item == wanted) {
            ReasoningPatch::Set {
                field: "reasoning_effort",
                value: Value::String(wanted.to_string()),
            }
        } else {
            ReasoningPatch::Skip { reason: "该等级不在本家接受的档位内（模型档位表里没有它）" }
        }
    }

    /// 上游错误分类。
    ///
    /// Loomy 业务失败恒 HTTP 200（信封判据在实现内部处理），chat 走标准 SSE ——
    /// 这里按 HTTP 状态码分类：401/403 → 刷新重试（本家无刷新，会让用户重登）、
    /// 429 → 限额换号、其余透传。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let message = error_body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let code = error_body
            .get("code")
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|number| number.to_string())
                    .or_else(|| value.as_str().map(str::to_string))
            })
            .unwrap_or_default();
        // 登录失效：HTTP 401/403 或信封码 100002
        if status == 401 || status == 403 || code == credentials::AUTH_ERROR_CODE {
            return UpstreamErrorClass::TokenExpired {
                message: format!("上游返回 {status}: {message}"),
            };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {message}"),
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {message}"),
            upstream_code: error_body.get("code").and_then(Value::as_i64),
        }
    }

    /// 取可用 access token。
    ///
    /// 本家**没有刷新端点**：过期就直接报「请重新短信登录」，
    /// 不做任何假装的续期尝试（那只会把一条清楚的错误变成两条）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = Self::snapshot_for(store, account_id)?;
            let credential = credentials::Credential::from_value(&record);
            if !credential.valid() {
                return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法转发"));
            }
            // 本地推算的 14 天有效期过了：如实报「重新登录」（服务端是否仍认由
            // 401 那条链兜底 —— 本地读数只用于提前提示，不阻止请求）
            if credentials::is_expired(&credential, crate::server::logging::now_ms()) {
                crate::server::logging::verbose(
                    "[Accounts]",
                    "Loomy 凭据已过本地推算的有效期，仍按可用处理（服务端 401 时再提示重登）",
                );
            }
            Ok(credential.access_token)
        })
    }

    /// 本家**不支持**主动刷新（没有 refresh 端点）。
    fn supports_refresh(&self) -> bool {
        false
    }

    /// 本家判不出「该刷新了」—— 恒 false（没有刷新手段，进维护队列只会白跑）。
    fn credentials_expiring(&self, _store: &AccountStore, _account_id: &str) -> bool {
        false
    }

    fn supports_usage(&self) -> bool {
        true
    }

    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(async move { balance::query_usage(store, account_id).await })
    }

    /// 本家**有**远程模型目录（`GET /models`）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        _force: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = match Self::snapshot_for(store, account_id) {
                Ok(record) => record,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            let credential = credentials::Credential::from_value(&record);
            if !credential.valid() {
                return ModelRefreshOutcome::failed("该账号没有可用凭证，无法刷新模型目录");
            }
            let (count, error) = models::refresh(&credential.access_token).await;
            match error {
                Some(reason) => ModelRefreshOutcome::failed(reason),
                None => ModelRefreshOutcome::refreshed(count),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 转发入口拿的是**会话形态**（凭证在 `auth` 下）—— 它必须能构造出请求，
    /// 且 chat 端点那两个头（`Authorization` + 小写 `token`）都带上。
    ///
    /// 这条用例钉的是「每一次转发都 401」那个缺陷：只认平铺的记录形态时，
    /// 这里会以「Loomy 账号缺少 session，无法转发」返回 Err。
    #[test]
    fn build_request_accepts_session_shape() {
        let session = json!({
            "endpoint": "https://loomy.xunfei.cn",
            "auth": { "accessToken": "sess-nested", "tokenType": "Bearer" },
            "account": { "uid": "123456789012345678" },
        });
        let body = json!({
            "model": "MiniMax-M3",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
        });
        let plan = LOOMY_ADAPTER
            .build_chat_request(&session, &body, &HeaderMap::new())
            .expect("会话形态必须能构造请求（否则转发恒 401）");
        // chat 端点两个头都发（见 client.rs 的实测口径）
        assert!(plan
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer sess-nested"));
        assert!(plan
            .headers
            .iter()
            .any(|(name, value)| name == "token" && value == "sess-nested"));
        assert!(plan.url.ends_with("/chat/completions"), "{}", plan.url);
        // body 原样透传
        assert_eq!(plan.body, body);
    }

    /// 会话形态缺 session 时仍要如实报错。
    #[test]
    fn build_request_rejects_session_without_token() {
        let session = json!({ "auth": { "refreshToken": "rt-only" } });
        let body = json!({ "model": "MiniMax-M3", "messages": [] });
        // `ChatRequestPlan` 没有 Debug，用 match 而不是 expect_err
        // （不为此给共享结构加派生）。
        let error = match LOOMY_ADAPTER.build_chat_request(&session, &body, &HeaderMap::new()) {
            Ok(_) => panic!("没有 session 必须报错"),
            Err(error) => error,
        };
        assert_eq!(error.status_code, 401);
        assert!(error.message.contains("session"), "文案要点明缺什么：{}", error.message);
    }

    #[test]
    fn error_classification() {
        let class = LOOMY_ADAPTER.classify_error(401, &json!({"message": "unauthorized"}));
        assert!(matches!(class, UpstreamErrorClass::TokenExpired { .. }));
        let class = LOOMY_ADAPTER.classify_error(200, &json!({"code": "100002", "desc": "缺少 token"}));
        assert!(matches!(class, UpstreamErrorClass::TokenExpired { .. }));
        let class = LOOMY_ADAPTER.classify_error(429, &json!({"message": "too many"}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
        let class = LOOMY_ADAPTER.classify_error(500, &json!({"message": "oops"}));
        assert!(matches!(class, UpstreamErrorClass::Fatal { status: 500, .. }));
    }

    #[test]
    fn reasoning_patch_checks_model_efforts() {
        // 兜底路径：8 个模型统一 5 档
        let patch = LOOMY_ADAPTER.reasoning_patch("high", "MiniMax-M3", &json!({}));
        match patch {
            ReasoningPatch::Set { field, value } => {
                assert_eq!(field, "reasoning_effort");
                assert_eq!(value, json!("high"));
            }
            other => panic!("应注入 high：{other:?}"),
        }
        // off → none（官方档位之一）
        let patch = LOOMY_ADAPTER.reasoning_patch("off", "MiniMax-M3", &json!({}));
        match patch {
            ReasoningPatch::Set { value, .. } => assert_eq!(value, json!("none")),
            other => panic!("应注入 none：{other:?}"),
        }
        // 不在档位内 → Skip
        let patch = LOOMY_ADAPTER.reasoning_patch("max", "MiniMax-M3", &json!({}));
        assert!(matches!(patch, ReasoningPatch::Skip { .. }));
        // 客户端已指定 → Skip
        let patch = LOOMY_ADAPTER.reasoning_patch("high", "MiniMax-M3", &json!({"reasoning_effort": "low"}));
        assert!(matches!(patch, ReasoningPatch::Skip { .. }));
    }
}

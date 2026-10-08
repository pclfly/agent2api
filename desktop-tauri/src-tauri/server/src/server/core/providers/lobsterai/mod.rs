//! LobsterAI（有道龙虾）适配器。
//!
//! ── 上游长什么样（移植来源：参考实现 `src/lobsterai*.ts`）────────
//!   - 对话：`POST {apiBase}/api/proxy/v1/chat/completions`，OpenAI 兼容、
//!     **仅支持 SSE**（无 `stream:false` 形态），Bearer 鉴权 + 两个
//!     `X-LobsterAI-Client-*` 头（出站指纹，见 `client.rs`）。
//!     因为上游说 Chat 协议、一次请求就是一次对话，本家走**无状态**转发
//!     路径（与 raccoon / autoclaw 同构），`is_stateful()` 保持默认 false。
//!   - 鉴权：`POST /api/auth/exchange`（换 token）/ `POST /api/auth/refresh`
//!     （续期，**不带 Authorization**，请求体必须带 uuid / firstKeyfrom /
//!     latestKeyfrom / version —— 丢了只能重新登录，见 `credentials.rs`）。
//!   - 登录：本地回调服务器 + authCode 换 token（`core::login` 的
//!     `start_lobsterai_login`，与 Trae 的回调形态同源）。
//!
//! ── 错误分类（`lobsterai-errors.ts` 的逐条对齐）──────────────
//! 判定顺序即优先级：402 → hard-credit 关键词 → session-dead 关键词 →
//! 429 → 404 → 5xx → 4xx。**body 关键词排在状态码之前**（除 402）：实测上游
//! 用 400 + 中文「积分不足」表达余额耗尽，只按状态码会把这类错误误判成
//! 可重试的 client 错误，于是反复重试一个永远不会成功的账号。
//! 本项目映射：hard-credit → `QuotaLimited`（换号 + 冷却）、session-dead /
//! 401 → `TokenExpired`（刷新后同账号重试一次）、其余 → `Fatal`。
//!
//! ── 与参考实现的一处刻意分歧 ────────────────────────────────
//! 参考实现「任何非 2xx 都换号」（Go 的 `handler.go:218-243` 每个分支都以
//! `continue` 结尾）。本项目的换号信号只有 `QuotaLimited` 一类（编排层的
//! 既有契约），因此 404 / 5xx / 其余 4xx 归 `Fatal` 透传 —— 与 Qoder / Trae
//! 的处置一致（404 多为请求级：模型名错，换号无用）。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   product.rs      常量（基址 / 端点 / 出站指纹 / 兜底模型表）
//!   credentials.rs  凭据结构 + 信封解析 + keyfrom 载荷 + uid 四级回退
//!   client.rs       出站头构造 + JSON 请求小工具
//!   version.rs      客户端版本号解析器（12h 缓存 + 兜底）
//!   models.rs       模型目录（远程拉取 + 兜底 19 条 + 思考档位元数据）
//!   checkin.rs      三步签到（slot → context → check_in）
//!   balance.rs      积分余额（profile-summary）
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic。

use std::pin::Pin;
use std::time::Duration;

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
pub mod version;

/// 提供商 id（注册表与 `ProviderKind` 用同一条）。
pub const PROVIDER_ID: &str = super::kind_id(ProviderKind::LobsterAi);

/// 凭证临期窗口（毫秒）：剩余不足 5 分钟视为需要刷新。
///
/// 与 workbuddy / 小浣熊 / AutoClaw 的窗口一致；各家自己判（见
/// `ProviderAdapter::credentials_expiring` 的说明：临期窗口不同，不能统一）。
const EXPIRY_WINDOW_MS: i64 = 5 * 60 * 1000;

/// LobsterAI 适配器（无状态单例）。
pub struct LobsterAiAdapter;

/// 进程级实例（`adapter_for` 返回它的引用）。
pub static LOBSTERAI_ADAPTER: LobsterAiAdapter = LobsterAiAdapter;

impl LobsterAiAdapter {
    /// 取某账号的凭据快照（`account_id` 为空 = 这一家的队首可用账号）。
    ///
    /// 与转发选路的口径一致：只挑 enabled + 有 token 的记录；
    /// 点名 id 时**按 id 直取**，取不到明确报错（不回落到队首 ——
    /// 那会变成「选了 A、用的是 B」的静默错误）。
    fn snapshot_for(
        store: &AccountStore,
        account_id: &str,
    ) -> Result<Value, GatewayError> {
        let record = store.lobsterai_account_record(account_id);
        if !account_id.is_empty() && record.is_none() {
            return Err(GatewayError::with_status(404, "账号不存在"));
        }
        record.ok_or_else(|| {
            GatewayError::with_status(
                401,
                format!("没有可用的 {} 账号", kind_id(ProviderKind::LobsterAi)),
            )
        })
    }

    /// 续期（`force = false` 时仍会真刷 —— 调用方先问 `credentials_expiring`
    /// 或收到 401 才进来；本函数不做二次临期判断）。
    async fn refresh_credential(
        store: &AccountStore,
        credential: &credentials::Credential,
    ) -> Result<credentials::Credential, GatewayError> {
        if !credential.can_refresh() {
            return Err(GatewayError::with_status(
                401,
                "该账号没有 refreshToken，无法续期：请重新登录添加",
            ));
        }
        let (client_version, _source) = version::resolve().await;
        let body = credentials::refresh_body(credential, &client_version);
        let reply = client::post_api(
            product::REFRESH_PATH,
            &body,
            &client::anonymous_headers(),
            None,
            Duration::from_millis(product::REQUEST_TIMEOUT_MS),
        )
        .await?;
        if reply.status == 401 || reply.status == 403 {
            return Err(GatewayError::with_status(
                401,
                "refreshToken 被拒绝（会话已终止）：请重新登录添加账号",
            ));
        }
        if !(200..300).contains(&reply.status) {
            return Err(GatewayError::with_status(
                502,
                format!("续期失败：上游返回 HTTP {}", reply.status),
            ));
        }
        let Some(body) = reply.json() else {
            return Err(GatewayError::with_status(502, "续期响应不是 JSON"));
        };
        let envelope = credentials::parse_envelope(&body)
            .map_err(|failure| GatewayError::with_status(502, format!("续期失败：{}", failure.message)))?;
        // session-dead 关键词（40100 / 40101）：终态，重试无意义
        let message = credentials::read_string(&body, "msg");
        if message.contains("40100") || message.contains("40101") {
            return Err(GatewayError::with_status(
                401,
                "refreshToken 被拒绝（会话已终止）：请重新登录添加账号",
            ));
        }
        let payload = credentials::parse_token_payload(&envelope.data);
        if payload.access_token.is_empty() {
            return Err(GatewayError::with_status(502, "续期响应缺少 accessToken"));
        }
        let next = credentials::apply_refresh(credential, &payload, crate::server::logging::now_ms());
        // 写回（比较-再写：记录里的凭证仍是续期前那份才写）
        let expected = credential_to_value(credential);
        let _ = store.update_lobsterai_credentials_if_current(&expected, &next);
        Ok(next)
    }
}

/// 凭据 → 比较用的 `Value`（写回时的 expected）。
fn credential_to_value(credential: &credentials::Credential) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in credential.patch_fields() {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

impl ProviderAdapter for LobsterAiAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::LobsterAi
    }

    /// 模型清单（远程优先，否则兜底 19 条）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {apiBase}/api/proxy/v1/chat/completions`。
    ///
    /// body **原样透传**：上游是 OpenAI 兼容实现，客户端发来的就是它要的
    /// 形态（与 raccoon 同一处置）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::Credential::from_value(account);
        if !credential.valid() {
            return Err(GatewayError::with_status(401, "LobsterAI 账号缺少 accessToken，无法转发"));
        }
        // 版本号走同步读（`ensure_access_token` 已预热缓存；未命中时给兜底值，
        // 并顺手触发一次后台刷新 —— 见 `version::current` 的说明）。
        let client_version = version::current();
        let headers = client::chat_headers(&credential, &client_version);
        let url = format!("{}{}", product::API_BASE, product::CHAT_PATH);
        Ok(ChatRequestPlan::chat(url, headers, body.clone()))
    }

    /// 思考档位翻译：映射上的产品侧 `level` → 上游 wire 值 `openclawLevel`。
    ///
    /// ── 两个字段不能混用（这是本家最容易搞错的地方）──────────
    /// 远端 `thinkingConfig.options[]` 里 `level` 是产品侧名（含 `max`），
    /// `openclawLevel` 是发给服务端的取值（**没有 `max`**：`max` 映射到
    /// `xhigh`）。直接发 `reasoning_effort: 'max'` 与不带参数无差异 ——
    /// 必须用 `openclawLevel`。
    ///
    /// `off` / `none` 档：全局判据是「一律不注入」，但本家有实测支撑的
    /// 关闭表达（`off` 是官方 wire 值之一，`thinking-level-control-v1`
    /// 能力头就是为它加的）。因此这里**先查模型档位表**：有 `off` 档就注入，
    /// 没有就 Skip（不把未知取值推给上游）。
    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> ReasoningPatch {
        // 客户端已显式指定时不覆盖（用户的明确意图比映射默认值更具体）
        if crate::server::core::model_rules::read_client_level(body).is_some() {
            return ReasoningPatch::Skip { reason: "客户端请求体里已指定思考等级" };
        }
        let Some(config) = models::thinking_config_of(model) else {
            return ReasoningPatch::Skip { reason: "该模型没有思考档位声明（远端未下发 thinkingConfig）" };
        };
        let is_off = crate::server::core::model_rules::reasoning_is_off(level);
        if is_off {
            // 关闭思考：只在该模型确实有 off 档时注入
            return match config.wire_for("off") {
                Some(wire) => ReasoningPatch::Set {
                    field: "reasoning_effort",
                    value: Value::String(wire.to_string()),
                },
                None => ReasoningPatch::Skip { reason: "该模型不支持关闭思考（档位表里没有 off）" },
            };
        }
        match config.wire_for(level) {
            Some(wire) => ReasoningPatch::Set {
                field: "reasoning_effort",
                value: Value::String(wire.to_string()),
            },
            None => ReasoningPatch::Skip { reason: "该等级不在本家接受的档位内（模型档位表里没有它）" },
        }
    }

    /// 上游错误分类（判定顺序即优先级，见模块头）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        // 关键词匹配的输入：归一化后的 code + message（都做小写化处理）
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
        let haystack = format!("{message} {code}");
        let lower = haystack.to_lowercase();

        // 402 Payment Required：最直接的「余额不足」信号
        if status == 402 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {message}"),
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        // hard-credit 关键词（中英双通道；body 关键词排在状态码之前）
        if HARD_CREDIT_MARKERS.iter().any(|marker| {
            lower.contains(&marker.to_lowercase()) || haystack.contains(marker)
        }) {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {message}"),
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        // session-dead 标记：终态，让刷新链路试一次；失败则用户重登
        if SESSION_DEAD_MARKERS.iter().any(|marker| haystack.contains(marker)) {
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
        if status == 401 {
            return UpstreamErrorClass::TokenExpired {
                message: format!("上游返回 {status}: {message}"),
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {message}"),
            upstream_code: error_body.get("code").and_then(Value::as_i64),
        }
    }

    /// 取可用 access token（临期主动刷新；刷新结果回写 store）。
    ///
    /// 同时**预热版本号缓存**（`build_chat_request` 是同步的，读缓存；
    /// 第一次请求的预热在这里发生）。
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
            // 预热版本号缓存（失败不阻塞：`current` 会给兜底值）
            let _ = version::resolve().await;
            let now = crate::server::logging::now_ms();
            if !credentials::is_expired(&credential, now) {
                // 临期窗口内也主动续期（与既有家同一口径）
                if credentials::credential_expires_at_ms(&credential)
                    .map(|expires_at| expires_at - now > EXPIRY_WINDOW_MS)
                    .unwrap_or(true)
                {
                    return Ok(credential.access_token);
                }
            }
            let next = Self::refresh_credential(store, &credential).await?;
            Ok(next.access_token)
        })
    }

    /// 401 后的**强制**刷新（不看临期窗口）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = Self::snapshot_for(store, account_id)?;
            let credential = credentials::Credential::from_value(&record);
            let next = Self::refresh_credential(store, &credential).await?;
            Ok(next.access_token)
        })
    }

    /// 本家支持主动刷新（refresh 端点存在且实测可用）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 该账号的凭证此刻是否已过期或临期（5 分钟窗口）。
    ///
    /// 判不出过期时间（无 expires_at 且 JWT 解不出 exp）时返回 false ——
    /// 无从判断就说「需要刷新」会让维护任务每轮都白跑一趟。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.lobsterai_account_record(account_id) else {
            return false;
        };
        let credential = credentials::Credential::from_value(&record);
        if !credential.can_refresh() {
            // 没有 refreshToken 的账号进维护队列只会每轮刷一次失败日志
            return false;
        }
        let Some(expires_at) = credentials::credential_expires_at_ms(&credential) else {
            return false;
        };
        expires_at - crate::server::logging::now_ms() <= EXPIRY_WINDOW_MS
    }

    /// 凭证的过期时间（毫秒；解不出 → None）。
    ///
    /// 给凭证自动维护算「维护窗口」用（见 trait 的说明）：转发窗口是 5 分钟，
    /// 维护间隔默认 10 分钟 —— 没有这一项时，两次维护之间到期的凭证会漏刷。
    fn credentials_expires_at_ms(&self, store: &AccountStore, account_id: &str) -> Option<i64> {
        let record = store.lobsterai_account_record(account_id)?;
        let credential = credentials::Credential::from_value(&record);
        credentials::credential_expires_at_ms(&credential)
    }

    /// 本家支持余额查询。
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

    /// 本家**有**远程模型目录（`GET /api/models/available`）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷新模型目录。
    ///
    /// `force` 在本家没有 TTL 早退的差异（每次都真拉 —— 26 条以内的小响应，
    /// 自动路径的频率由 `catalog_refresh` 的排期控制），参数保留是为了契约一致。
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
            let (client_version, _source) = version::resolve().await;
            let (count, error) = models::refresh(&credential, &client_version, None).await;
            match error {
                Some(reason) => ModelRefreshOutcome::failed(reason),
                None => ModelRefreshOutcome::refreshed(count),
            }
        })
    }
}

/// 余额不足关键词（对齐参考实现 `classify.go:55-60` 的 `hardMarkers`）。
///
/// 中英双通道是必需的：LobsterAI 是网易有道系产品，同一后端在不同场景下
/// 会返回中文或英文文案（实测两种都出现过），只匹配一种会漏判。
/// 2026-09 补充的「已用完 / 升级套餐」是真实缺陷的修复 —— 早期表里只有
/// 「额度用尽」「积分用完」，「已用完」与「用尽」字面不同，于是这个最主要的
/// 失败模式被判成 `none`（不换号），用户看到「一个账号用完出错但没有切换」。
const HARD_CREDIT_MARKERS: &[&str] = &[
    "insufficient credit",
    "no credit",
    "credit exhausted",
    "out of credit",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "not enough credit",
    "freecreditsused",
    "free credits used",
    "free quota",
    "quota used up",
    "upgrade your plan",
    "upgrade to continue",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
    "积分耗尽",
    "额度已用完",
    "升级套餐",
];

/// 会话终止标记（`40100` / `40101` 是 LobsterAI 刷新被拒的业务码）。
const SESSION_DEAD_MARKERS: &[&str] = &[
    "40100",
    "40101",
    "token rejected",
    "refresh token was rejected",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 转发入口拿的是**会话形态**（凭证在 `auth` 下）—— 它必须能构造出请求。
    ///
    /// 这条用例钉的是「每一次转发都 401」那个缺陷：只认平铺的记录形态时，
    /// 这里会以「LobsterAI 账号缺少 accessToken，无法转发」返回 Err。
    ///
    /// `#[tokio::test]`：本家的 `build_chat_request` 会读版本号缓存，未命中时
    /// 触发一次后台刷新（`version::current` → `tokio::spawn`）—— 生产里它**总是**
    /// 跑在 tokio 运行时内（转发链路是 async），所以用例也要给出同样的上下文。
    #[tokio::test]
    async fn build_request_accepts_session_shape() {
        let session = json!({
            "endpoint": "https://api.lobsterai.com",
            "auth": {
                "accessToken": "at-sess",
                "tokenType": "Bearer",
                "expiresAt": 1800000000000_i64,
            },
            "account": { "uid": "u-9" },
        });
        let body = json!({
            "model": "kimi-k3",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
        });
        let plan = LOBSTERAI_ADAPTER
            .build_chat_request(&session, &body, &HeaderMap::new())
            .expect("会话形态必须能构造请求（否则转发恒 401）");
        assert!(plan
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer at-sess"));
        assert!(plan.url.ends_with("/api/proxy/v1/chat/completions"), "{}", plan.url);
        // body 原样透传（本家上游是 OpenAI 兼容实现）
        assert_eq!(plan.body, body);
    }

    /// 会话形态缺 token 时仍要如实报错（不是静默发出无鉴权请求）。
    #[test]
    fn build_request_rejects_session_without_token() {
        let session = json!({ "auth": { "refreshToken": "rt-only" } });
        let body = json!({ "model": "kimi-k3", "messages": [] });
        // `ChatRequestPlan` 没有 Debug，用 match 而不是 expect_err
        // （不为此给共享结构加派生）。
        let error = match LOBSTERAI_ADAPTER.build_chat_request(&session, &body, &HeaderMap::new()) {
            Ok(_) => panic!("没有 accessToken 必须报错"),
            Err(error) => error,
        };
        assert_eq!(error.status_code, 401);
        assert!(error.message.contains("accessToken"), "文案要点明缺什么：{}", error.message);
    }

    #[test]
    fn hard_credit_keywords_are_classified_before_status() {
        // 400 + 中文「积分不足」：这是最主要的失败模式，必须归限额而不是 Fatal
        let class = LOBSTERAI_ADAPTER.classify_error(400, &json!({"message": "积分不足，请充值"}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
        // 英文关键词
        let class = LOBSTERAI_ADAPTER
            .classify_error(400, &json!({"message": "insufficient credit balance"}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
        // 402 直接限额
        let class = LOBSTERAI_ADAPTER.classify_error(402, &json!({"message": ""}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
    }

    #[test]
    fn session_dead_markers_map_to_token_expired() {
        let class = LOBSTERAI_ADAPTER.classify_error(401, &json!({"code": 40100, "message": "token rejected"}));
        assert!(matches!(class, UpstreamErrorClass::TokenExpired { .. }));
    }

    #[test]
    fn other_statuses_map_to_fatal() {
        let class = LOBSTERAI_ADAPTER.classify_error(404, &json!({"message": "not found"}));
        assert!(matches!(class, UpstreamErrorClass::Fatal { status: 404, .. }));
        let class = LOBSTERAI_ADAPTER.classify_error(500, &json!({"message": "server error"}));
        assert!(matches!(class, UpstreamErrorClass::Fatal { status: 500, .. }));
    }

    #[test]
    fn reasoning_patch_uses_wire_values_from_thinking_config() {
        // 注入一个模型：level=max 映射到 openclawLevel=xhigh
        // 通过 models 模块的进程缓存无法在单测里直接写，这里改测
        // thinking_config_of 的解析（parse_thinking_config 是纯函数）。
        let record = json!({
            "modelId": "deepseek-v4-pro",
            "thinkingConfig": {
                "options": [
                    { "level": "low", "openclawLevel": "low" },
                    { "level": "max", "openclawLevel": "xhigh" }
                ],
                "defaultLevel": "low"
            }
        });
        let config = models::parse_thinking_config(&record).expect("应解析出档位配置");
        assert_eq!(config.wire_for("max"), Some("xhigh"));
        assert_eq!(config.wire_for("low"), Some("low"));
        assert_eq!(config.wire_for("off"), None);
        assert_eq!(config.default_level.as_deref(), Some("low"));
    }
}

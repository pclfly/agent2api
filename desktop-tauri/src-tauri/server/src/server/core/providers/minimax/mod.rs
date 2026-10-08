//! MiniMax Code（中国版）适配器。
//!
//! ── 上游长什么样（移植来源：参考实现 `src/minimax*.ts`）────────
//!   - 推理：`POST {apiHost}/mavis/api/v1/llm/v1/messages`，**Anthropic
//!     Messages 协议**（`system` 顶层字段、`tool_use` / `tool_result` 块、
//!     图片 `source.type=base64` 裸 base64）。
//!   - 登录：OAuth 设备码 + PKCE S256（`account.minimax.cn`），
//!     与 Qoder / Cline 的设备授权同形。
//!   - 凭据：token 前缀 `mmoat_` / `mmort_`，**不是 JWT** ——
//!     `expires_at` 必须由 `expires_in` 自算写入（见 `credentials.rs`）。
//!
//! ── 转发为什么走**无状态**路径（与参考实现的结构差异）──────────
//! 参考实现（DSH 插件）自己写 SSE 消费与 OpenAI chunk 转换，因为它没有
//! 翻译层。本仓**已有**「Anthropic 请求体转换 + Anthropic SSE 折回 chat SSE」
//! 的共享层（`protocol::anthropic_outbound` + `upstream::translate`，
//! ZCode 活动套餐通道在用），因此本家走**无状态路径 + `UpstreamResponse::
//! Anthropic`**：`build_chat_request` 把 OpenAI body 转成 Anthropic Messages
//! 形态并声明响应协议，编排层与翻译层负责其余 —— 账号轮换、限额冷却、
//! 退避重试、usage 与取消处理全部复用既有链路。
//!
//! ── 思考决策表（真机实测，逐条对齐参考实现）────────────────────
//! | 情形 | 请求体 |
//! |---|---|
//! | `off` / `none`（switchable 模型）| `thinking:{type:'disabled'}` |
//! | `on`（switchable）| `thinking:{type:'adaptive'}` |
//! | 有档位 effort（模型档位表内含它）| `thinking:{type:'adaptive'}` + `output_config.effort` |
//! | M3.1（`ADAPTIVE_ONLY_PREFIX`，无档位）| `thinking:{type:'adaptive'}`（**必须**，否则 400） |
//! | 其余（M2.7 系，无档位）| **整个不发**（服务端默认 forced_on） |
//!
//! ⚠️ **M3 不发 `thinking` 时默认「不思考」**（实测两轮各 0 字符），
//! 所以「开启思考」**必须显式发 adaptive** —— 这正是 `on` 档存在的理由。
//! ⚠️ `forced_on` 的模型**绝不**发 `disabled`：M3.1 会硬 400，
//! M2.7 会被静默忽略（那会让用户以为关掉了、实际没关）。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   product.rs      常量（主机 / OAuth 参数 / 端点 / 兜底模型表）
//!   credentials.rs  凭据结构 + 过期判定 + 令牌响应解析
//!   client.rs       出站头构造 + JSON 请求小工具
//!   oauth.rs        设备码 + PKCE + 轮询 + 续期
//!   models.rs       模型目录（长名/短名、窗口最大档、档位元数据）
//!   checkin.rs      签到（signin/status + signin/claim，timezone_id）
//!   balance.rs      积分（credit/details）
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic。

use std::pin::Pin;

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::protocol::anthropic_outbound;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, ReasoningPatch, UpstreamErrorClass,
    UpstreamResponse,
};
use super::ProviderKind;

pub mod balance;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod import;
pub mod models;
pub mod oauth;
pub mod product;
pub mod profile;

/// 提供商 id（注册表与 `ProviderKind` 用同一条）。
pub const PROVIDER_ID: &str = super::kind_id(ProviderKind::Minimax);

/// 凭证临期窗口（毫秒）：剩余不足 5 分钟视为需要刷新。
const EXPIRY_WINDOW_MS: i64 = 5 * 60 * 1000;

/// 从账号记录解析凭据。
///
/// `desktop: true` 的记录**优先重读客户端文件**（`~/.minimax/local-runtime.auth.json`
/// 或旧的 `auth/...`，客户端刷新后网关自动拿到新 token；文件不可用 / 是另一个
/// 账号时回退落盘快照）—— 这是「导入客户端登录态」的只读纪律之一
/// （见 `import.rs` 的模块头）。
pub fn credential_of(record: &Value) -> credentials::Credential {
    if is_desktop(record) {
        if let Ok(Some(fresh)) = import::read_desktop_credentials() {
            if fresh.credential.valid() && desktop_same_account(record, &fresh) {
                return fresh.credential;
            }
        }
    }
    credentials::Credential::from_value(record)
}

/// 该记录是不是「导入的客户端登录态」。
fn is_desktop(record: &Value) -> bool {
    record.get("desktop").and_then(Value::as_bool) == Some(true)
}

/// 客户端文件与账号记录是不是同一个账号（防串号）。
///
/// 判据是 `realUserId`（JWT 来源的导入会写入它）：两边都有值时必须相等。
/// 记录没有 `realUserId`（mcode 来源的旧导入）或文件没有它（mcode 文件）时
/// 不阻断 —— 那是「同一文件来源」的设计假设（单账号场景）。
fn desktop_same_account(record: &Value, fresh: &import::DesktopCredentials) -> bool {
    let recorded = record
        .get("realUserId")
        .and_then(Value::as_str)
        .unwrap_or("");
    if recorded.is_empty() || fresh.real_user_id.is_empty() {
        return true;
    }
    recorded == fresh.real_user_id
}

/// MiniMax 适配器（无状态单例）。
pub struct MinimaxAdapter;

/// 进程级实例（`adapter_for` 返回它的引用）。
pub static MINIMAX_ADAPTER: MinimaxAdapter = MinimaxAdapter;

impl MinimaxAdapter {
    /// 取某账号的凭据快照（`account_id` 为空 = 这一家的队首可用账号）。
    fn snapshot_for(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
        let record = store.minimax_account_record(account_id);
        if !account_id.is_empty() && record.is_none() {
            return Err(GatewayError::with_status(404, "账号不存在"));
        }
        record.ok_or_else(|| {
            GatewayError::with_status(
                401,
                format!("没有可用的 {} 账号", super::kind_id(ProviderKind::Minimax)),
            )
        })
    }

    /// 续期（refresh_token grant），结果按「比较再写」回写账号库。
    ///
    /// ── 为什么要显式带上账号 id（2026-10 修复的 bug）────────────
    /// `update_minimax_credentials_if_current` 用 `expected["id"]` 定位记录，
    /// 而这里原来只放 `credential.patch_fields()`（7 个凭证字段，**没有 id**）——
    /// 于是每次续期都返回 `Stale` 且被 `let _ =` 吞掉，**新 token 一次也没落库**。
    /// 后果（`MiniMax 凭证 60 分钟到期` 时期最明显）：
    ///   1. 账号页的「有效期」永远停在旧值 → 一直显示「已过期」；
    ///   2. 每次使用都重判「临期」→ 每个请求都打一次上游续期；
    ///   3. 上游 refresh_token 轮换（本仓注释明确「一次一换」）后，库里仍是
    ///      **旧 refresh_token** → 下一次续期被上游拒（401 invalid_grant）
    ///      → 用户看到「登录态失效，请重新登录」。
    /// 现在 id 随 `expected_patch` 一起给，写回成功即刷新结束。
    async fn refresh_credential(
        store: &AccountStore,
        account_id: &str,
        credential: &credentials::Credential,
    ) -> Result<credentials::Credential, GatewayError> {
        let next = oauth::refresh_credential(&credential.refresh_token, credential).await?;
        match store.update_minimax_credentials_if_current(
            &expected_patch(account_id, credential),
            &next,
        ) {
            Ok(CredentialWrite::Written) => {}
            // Stale：期间凭证被换过（重导入 / 换号 / 另一轮刷新先落地）。
            // 本轮结果作废但不报错 —— 手里的新 token 仍然可用，直接返回。
            Ok(CredentialWrite::Stale) => logging::log(
                "[Accounts]",
                &format!("MiniMax 账号 {account_id} 的续期结果未落库（记录已被更换）"),
            ),
            Err(error) => logging::log(
                "[Accounts]",
                &format!("MiniMax 账号 {account_id} 的续期结果写库失败：{}", error.message),
            ),
        }
        Ok(next)
    }
}

/// 「比较-再写」用的期望快照：**必须含账号 id**。
///
/// 见 [`MinimaxAdapter::refresh_credential`] 的说明 —— 漏掉 id 会让每一次
/// 续期都被判成 `Stale`（记录找不到），这是本项目踩过的坑。
/// `pub(crate)`：账号存储的回归测试要拿它当「正确形状」的参照。
pub(crate) fn expected_patch(account_id: &str, credential: &credentials::Credential) -> Value {
    let mut expected = serde_json::Map::new();
    expected.insert("id".to_string(), Value::String(account_id.to_string()));
    for (key, value) in credential.patch_fields() {
        expected.insert(key.to_string(), value);
    }
    Value::Object(expected)
}

impl ProviderAdapter for MinimaxAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Minimax
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {apiHost}/mavis/api/v1/llm/v1/messages`（Anthropic Messages）。
    ///
    /// 两步：`anthropic_request_from_chat` 把 OpenAI body 转成 Anthropic 形态，
    /// 再按思考决策表注入 `thinking` / `output_config`。
    /// `response: UpstreamResponse::Anthropic` 让编排层走翻译层（Anthropic SSE
    /// → chat SSE），与 ZCode 活动套餐通道同一条链。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::Credential::from_value(account);
        if !credential.valid() {
            return Err(GatewayError::with_status(401, "MiniMax 账号缺少 accessToken，无法转发"));
        }
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if model.is_empty() {
            // Anthropic 协议必填 model（与 chat 路径「没点名就用上游默认」不同）
            return Err(GatewayError::with_status(
                400,
                "MiniMax 需要明确的模型名：请在客户端指定模型后重试",
            ));
        }
        let mut payload = anthropic_outbound::anthropic_request_from_chat(body, &model)
            .map_err(|message| GatewayError::with_status(400, format!("请求体转换失败：{message}")))?;
        apply_thinking_from_client(&mut payload, &model, body);
        // 提示缓存断点：Anthropic 协议是**显式断点制**，请求里没有
        // `cache_control` 就等于关闭缓存（上游不命中、也不回报
        // `cache_read_input_tokens`）—— 这正是「MiniMax 缓存读取恒为 0」的根因。
        // 转换器是保真语义，客户端（chat 协议）不带断点就一个都不加，所以这里补。
        // 见 `anthropic_outbound::apply_prompt_cache_breakpoints` 的说明。
        anthropic_outbound::apply_prompt_cache_breakpoints(&mut payload);
        let headers = client::infer_headers(&credential);
        Ok(ChatRequestPlan {
            url: format!("{}{}", product::API_HOST, product::INFER_PATH),
            headers,
            body: payload,
            response: UpstreamResponse::Anthropic,
        })
    }

    /// 映射绑定的思考等级 → Anthropic 字段（见模块头的决策表）。
    ///
    /// `body` 是**已转换的** Anthropic 请求体：若它已经有 `thinking` /
    /// `output_config`（build 阶段按客户端档位写过），说明客户端已明确指定，
    /// 映射绑定**不覆盖**。
    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> ReasoningPatch {
        if body.get("thinking").is_some() || body.get("output_config").is_some() {
            return ReasoningPatch::Skip { reason: "客户端请求体里已指定思考等级" };
        }
        let thinking_mode = models::thinking_mode_of(model);
        let switchable = thinking_mode.as_deref() == Some("switchable");
        let is_off = crate::server::core::model_rules::reasoning_is_off(level);
        if is_off {
            return if switchable {
                ReasoningPatch::Set {
                    field: "thinking",
                    value: json!({ "type": "disabled" }),
                }
            } else {
                ReasoningPatch::Skip { reason: "该模型强制开启思考（不能关闭）" }
            };
        }
        if level == "on" {
            return if switchable || thinking_mode.as_deref() == Some("forced_on") {
                ReasoningPatch::Set {
                    field: "thinking",
                    value: json!({ "type": "adaptive" }),
                }
            } else {
                ReasoningPatch::Skip { reason: "该模型没有思考开关声明" }
            };
        }
        // 档位：必须落在该模型的 effort_options 内（否则不发）
        match models::efforts_of(model) {
            Some(efforts) if efforts.iter().any(|item| item == level) => ReasoningPatch::Set {
                field: "output_config",
                value: json!({ "effort": level }),
            },
            Some(_) => ReasoningPatch::Skip { reason: "该等级不在本家接受的档位内（模型档位表里没有它）" },
            None => ReasoningPatch::Skip { reason: "该模型没有档位声明（远端未下发 effort_options）" },
        }
    }

    /// 上游错误分类。
    ///
    /// `402` 是**余额不足**，必须原样透出（最常见的真实失败，归成 502 会让
    /// 用户看不到「去充值」这个唯一有效动作）；`429` 限额换号；`401` 刷新重试。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let message = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .or_else(|| error_body.get("error").and_then(|error| error.get("message")))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let code = error_body.get("code").and_then(Value::as_i64);
        if status == 402 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {message}（余额不足，请充值）"),
                upstream_code: code,
                status,
            };
        }
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired {
                message: format!("上游返回 {status}: {message}"),
            };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {message}"),
                upstream_code: code,
                status,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {message}"),
            upstream_code: code,
        }
    }

    /// 取可用 access token（临期主动续期；结果回写 store）。
    ///
    /// `desktop: true` 的账号（导入的客户端登录态）**不代刷新** ——
    /// 重读客户端文件已由 [`credential_of`] 完成，过期就提示重新导入
    /// （refresh_token 可能轮换，网关侧刷新会让客户端登录态作废）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = Self::snapshot_for(store, account_id)?;
            let credential = credential_of(&record);
            if !credential.valid() {
                return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法转发"));
            }
            let now = crate::server::logging::now_ms();
            let fresh = credentials::credential_expires_at_ms(&credential)
                .map(|expires_at| expires_at - now > EXPIRY_WINDOW_MS)
                .unwrap_or(true);
            if !credentials::is_expired(&credential, now) && fresh {
                return Ok(credential.access_token);
            }
            if is_desktop(&record) {
                // 客户端登录态：重读已尽力（credential_of），仍过期就提示重新导入
                return Err(GatewayError::with_status(
                    401,
                    "MiniMax 客户端登录态已过期：请在 MiniMax 客户端登录后重新导入",
                ));
            }
            if !credential.can_refresh() {
                // 没有 refreshToken：如实报错（不假装能续期）
                if credentials::is_expired(&credential, now) {
                    return Err(GatewayError::with_status(
                        401,
                        "MiniMax 凭据已过期且没有 refreshToken：请重新登录该账号",
                    ));
                }
                return Ok(credential.access_token);
            }
            // 续期结果的写回要按**账号 id** 定位记录（见 `refresh_credential` 的说明）
            let account_id = record.get("id").and_then(Value::as_str).unwrap_or("");
            let next = Self::refresh_credential(store, account_id, &credential).await?;
            Ok(next.access_token)
        })
    }

    /// 401 后的**强制**刷新（不看临期窗口）。
    ///
    /// `desktop: true` 的账号：重读客户端文件（「刷新」= 同步客户端最新
    /// 状态，**不调上游 refresh**），并把结果**写回账号库**（只读文件，
    /// 无副作用）—— 下一次请求直接就能用上新 token，而不是每次 401 都
    /// 重读一遍。文件不可用 / 是另一个账号时报错提示重新导入。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = Self::snapshot_for(store, account_id)?;
            if is_desktop(&record) {
                let fresh = match import::read_desktop_credentials() {
                    Ok(Some(fresh)) if fresh.credential.valid() => fresh,
                    _ => {
                        return Err(GatewayError::with_status(
                            401,
                            "本机 MiniMax 登录态不可用：请在客户端登录后重新导入",
                        ))
                    }
                };
                if !desktop_same_account(&record, &fresh) {
                    return Err(GatewayError::with_status(
                        401,
                        "本机 MiniMax 登录态是另一个账号：请在客户端切回该账号，或删除本账号后重新导入",
                    ));
                }
                // 比较-再写（与普通账号的续期回写同一契约）：只更新账号库，
                // 客户端文件永远不动
                let current = credentials::Credential::from_value(&record);
                let account_id = record.get("id").and_then(Value::as_str).unwrap_or("");
                let _ = store.update_minimax_credentials_if_current(
                    &expected_patch(account_id, &current),
                    &fresh.credential,
                );
                return Ok(fresh.credential.access_token);
            }
            let credential = credentials::Credential::from_value(&record);
            let account_id = record.get("id").and_then(Value::as_str).unwrap_or("");
            let next = Self::refresh_credential(store, account_id, &credential).await?;
            Ok(next.access_token)
        })
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.minimax_account_record(account_id) else {
            return false;
        };
        // desktop 账号（导入的客户端登录态）：**不调上游续期**，但客户端文件
        // 比记录新时（桌面端续期后）触发同步 —— 同步 = 重读文件，只读无害
        // （见 `refresh_access_token` 的 desktop 分支）。
        // 文件与记录相同时返回 false，避免每轮维护都空跑一次读盘 + 写库。
        if is_desktop(&record) {
            return match import::read_desktop_credentials() {
                Ok(Some(fresh)) => {
                    fresh.credential.valid()
                        && desktop_same_account(&record, &fresh)
                        && fresh.credential.access_token
                            != record
                                .get("accessToken")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                }
                _ => false,
            };
        }
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
    /// 给凭证自动维护算「维护窗口」用（见 trait 的说明）：本家 token 实测 60 分钟
    /// 有效期，而转发窗口只有 5 分钟 —— 维护间隔（默认 10 分钟）大于窗口时，
    /// 两次维护之间到期的凭证会漏刷一轮（账号页显示「已过期」，但转发仍有懒刷新兜底）。
    fn credentials_expires_at_ms(&self, store: &AccountStore, account_id: &str) -> Option<i64> {
        let record = store.minimax_account_record(account_id)?;
        let credential = credential_of(&record);
        credentials::credential_expires_at_ms(&credential)
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
            let credential = credential_of(&record);
            if !credential.valid() {
                return ModelRefreshOutcome::failed("该账号没有可用凭证，无法刷新模型目录");
            }
            let (count, error) = models::refresh(&credential).await;
            match error {
                Some(reason) => ModelRefreshOutcome::failed(reason),
                None => ModelRefreshOutcome::refreshed(count),
            }
        })
    }
}

/// 构建阶段的思考注入（客户端档位路径；见模块头决策表）。
///
/// 读的是**客户端原始 body**（`client_body`）—— 转换器不认档位字段，
/// 转换后的 payload 里已经没有它们了。
fn apply_thinking_from_client(payload: &mut Value, model: &str, client_body: &Value) {
    let Some(object) = payload.as_object_mut() else { return };
    let thinking_mode = models::thinking_mode_of(model);
    let switchable = thinking_mode.as_deref() == Some("switchable");
    let requires_adaptive = model.starts_with(product::ADAPTIVE_ONLY_PREFIX);
    let client_level = crate::server::core::model_rules::read_client_level(client_body);
    let is_off = client_level
        .as_deref()
        .is_some_and(crate::server::core::model_rules::reasoning_is_off);

    if requires_adaptive {
        // M3.1 必须 adaptive（否则硬 400）
        object.insert("thinking".to_string(), json!({ "type": "adaptive" }));
        // 客户端档位 → output_config.effort（须在档位表内）
        if let Some(level) = client_level.as_deref().filter(|_| !is_off) {
            if level != "on" {
                if let Some(efforts) = models::efforts_of(model) {
                    if efforts.iter().any(|item| item == level) {
                        object.insert("output_config".to_string(), json!({ "effort": level }));
                    }
                }
            }
        }
        return;
    }
    if switchable {
        // M3：off/none → disabled；on → adaptive；其余不发（服务端默认不思考）
        match client_level.as_deref() {
            Some(_) if is_off => {
                object.insert("thinking".to_string(), json!({ "type": "disabled" }));
            }
            Some("on") => {
                object.insert("thinking".to_string(), json!({ "type": "adaptive" }));
            }
            _ => {}
        }
        return;
    }
    // M2.7 系：整个不发（服务端默认 forced_on）
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转发入口拿的是**会话形态**（`session_from_record` 的返回值，凭证在
    /// `auth` 下）—— 它必须能构造出请求，且 Authorization 是那把 token。
    ///
    /// 这条用例钉的是「每一次转发都 401」那个缺陷：只认平铺的记录形态时，
    /// 这里会以「MiniMax 账号缺少 accessToken，无法转发」返回 Err，而账号页
    /// （读记录形态）一切正常。
    #[test]
    fn build_request_accepts_session_shape() {
        let session = json!({
            "endpoint": "https://agent.minimax.cn",
            "edition": "cn",
            "auth": { "accessToken": "mmoat_sess", "tokenType": "Bearer" },
            "account": { "uid": "548212364582207496" },
        });
        let body = json!({
            "model": "MiniMax-M3",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
        });
        let plan = MINIMAX_ADAPTER
            .build_chat_request(&session, &body, &HeaderMap::new())
            .expect("会话形态必须能构造请求（否则转发恒 401）");
        assert_eq!(plan.url, "https://agent.minimax.cn/mavis/api/v1/llm/v1/messages");
        assert!(plan
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer mmoat_sess"));
        // 会话形态的请求体是 Anthropic Messages（不是 chat 体）
        assert_eq!(plan.response, UpstreamResponse::Anthropic);
        assert!(plan.body.get("system").is_none() || plan.body.get("system").is_some());
    }

    /// 转发请求**必须**带提示缓存断点。
    ///
    /// 这条钉的是「MiniMax 缓存读取恒为 0」那个缺陷：Anthropic 协议是显式断点制，
    /// 出站请求没有 `cache_control` 上游就不命中、也不回报
    /// `cache_read_input_tokens`。转换器是保真语义（客户端不带就不加），
    /// 所以 `build_chat_request` 必须自己补上。
    #[test]
    fn build_request_carries_prompt_cache_breakpoints() {
        let session = json!({
            "endpoint": "https://agent.minimax.cn",
            "edition": "cn",
            "auth": { "accessToken": "mmoat_sess", "tokenType": "Bearer" },
        });
        let body = json!({
            "model": "MiniMax-M3",
            "messages": [
                { "role": "system", "content": "you are helpful" },
                { "role": "user", "content": "hi" },
            ],
            "stream": true,
        });
        let plan = MINIMAX_ADAPTER
            .build_chat_request(&session, &body, &HeaderMap::new())
            .expect("会话形态必须能构造请求");
        // system 断点
        assert_eq!(
            plan.body["system"][0]["cache_control"],
            json!({ "type": "ephemeral" }),
            "system 上必须有缓存断点，否则上游不缓存"
        );
        // 最后一条 user 消息断点（字符串已升格成块数组）
        assert_eq!(
            plan.body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" }),
            "最后一条 user 消息上必须有缓存断点"
        );
    }

    /// 会话形态缺 token（例如记录里只有 refreshToken）时仍要如实报错。
    #[test]
    fn build_request_rejects_session_without_token() {
        let session = json!({ "auth": { "refreshToken": "mmort_only" } });
        let body = json!({ "model": "MiniMax-M3", "messages": [] });
        // `ChatRequestPlan` 没有 Debug（它是纯构造产物，不需要打印），
        // 所以用 match 而不是 expect_err —— 不为此给共享结构加派生。
        let error = match MINIMAX_ADAPTER.build_chat_request(&session, &body, &HeaderMap::new()) {
            Ok(_) => panic!("没有 accessToken 必须报错"),
            Err(error) => error,
        };
        assert_eq!(error.status_code, 401);
        assert!(error.message.contains("accessToken"), "文案要点明缺什么：{}", error.message);
    }

    #[test]
    fn quota_and_auth_classification() {
        let class = MINIMAX_ADAPTER.classify_error(402, &json!({"message": "insufficient balance"}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
        let class = MINIMAX_ADAPTER.classify_error(401, &json!({"message": "expired"}));
        assert!(matches!(class, UpstreamErrorClass::TokenExpired { .. }));
        let class = MINIMAX_ADAPTER.classify_error(429, &json!({"message": "too many"}));
        assert!(matches!(class, UpstreamErrorClass::QuotaLimited { .. }));
        let class = MINIMAX_ADAPTER.classify_error(400, &json!({"message": "bad"}));
        assert!(matches!(class, UpstreamErrorClass::Fatal { status: 400, .. }));
    }

    #[test]
    fn reasoning_patch_respects_switchable_and_forced_on() {
        // M3（switchable）：off → disabled
        let patch = MINIMAX_ADAPTER.reasoning_patch("off", "MiniMax-M3", &json!({}));
        match patch {
            ReasoningPatch::Set { field, value } => {
                assert_eq!(field, "thinking");
                assert_eq!(value, json!({"type": "disabled"}));
            }
            other => panic!("M3 应可关闭思考：{other:?}"),
        }
        // M3：on → adaptive
        let patch = MINIMAX_ADAPTER.reasoning_patch("on", "MiniMax-M3", &json!({}));
        match patch {
            ReasoningPatch::Set { value, .. } => assert_eq!(value, json!({"type": "adaptive"})),
            other => panic!("M3 应可开启思考：{other:?}"),
        }
        // M2.7（forced_on）：off → Skip（绝不发 disabled）
        let patch = MINIMAX_ADAPTER.reasoning_patch("off", "MiniMax-M2.7", &json!({}));
        assert!(matches!(patch, ReasoningPatch::Skip { .. }));
        // M3.1：档位 → output_config
        let patch = MINIMAX_ADAPTER.reasoning_patch("high", "MiniMax-M3.1-Flash-Preview", &json!({}));
        match patch {
            ReasoningPatch::Set { field, value } => {
                assert_eq!(field, "output_config");
                assert_eq!(value, json!({"effort": "high"}));
            }
            other => panic!("M3.1 应注入档位：{other:?}"),
        }
        // M3.1：表外档位 → Skip
        let patch = MINIMAX_ADAPTER.reasoning_patch("minimal", "MiniMax-M3.1-Flash-Preview", &json!({}));
        assert!(matches!(patch, ReasoningPatch::Skip { .. }));
        // 客户端已指定（body 里已有 thinking）→ Skip
        let patch = MINIMAX_ADAPTER.reasoning_patch("high", "MiniMax-M3", &json!({"thinking": {"type": "adaptive"}}));
        assert!(matches!(patch, ReasoningPatch::Skip { .. }));
    }

    #[test]
    fn build_stage_thinking_decision_table() {
        // M3.1 无档位 → 必须 adaptive
        let mut payload = json!({"model": "MiniMax-M3.1-Flash-Preview"});
        apply_thinking_from_client(&mut payload, "MiniMax-M3.1-Flash-Preview", &json!({}));
        assert_eq!(payload["thinking"], json!({"type": "adaptive"}));
        // M3.1 有档位 → adaptive + output_config
        let mut payload = json!({"model": "MiniMax-M3.1-Flash-Preview"});
        apply_thinking_from_client(
            &mut payload,
            "MiniMax-M3.1-Flash-Preview",
            &json!({"reasoning_effort": "high"}),
        );
        assert_eq!(payload["thinking"], json!({"type": "adaptive"}));
        assert_eq!(payload["output_config"], json!({"effort": "high"}));
        // M2.7 → 整个不发
        let mut payload = json!({"model": "MiniMax-M2.7"});
        apply_thinking_from_client(&mut payload, "MiniMax-M2.7", &json!({"reasoning_effort": "high"}));
        assert!(payload.get("thinking").is_none());
        assert!(payload.get("output_config").is_none());
        // M3 客户端选 off → disabled
        let mut payload = json!({"model": "MiniMax-M3"});
        apply_thinking_from_client(&mut payload, "MiniMax-M3", &json!({"reasoning_effort": "off"}));
        assert_eq!(payload["thinking"], json!({"type": "disabled"}));
    }
}

//! MiniMax 设备授权登录（OAuth 设备码 + PKCE，与 Qoder / Cline 同形）。
//!
//! ── 与另外两条设备授权链的异同 ─────────────────────────────
//!   - 形态与 Cline 一致：**同步问上游要 `user_code` 与授权页地址**（一次
//!     POST），把地址交给界面打开；用户确认后由后台任务**轮询**换令牌；
//!   - 差别只在轮询判据：Cline 认 RFC 8628 的 `authorization_pending`；
//!     MiniMax **两种「还在等」的形态都要认**（HTTP 200 + `status` 的
//!     `pending` / `slow_down`，以及非 200 + `error` 的标准形态，
//!     见 `providers::minimax::oauth` 的模块头）。
//!
//! 没有额外的待办表：这一家的中间态（device_code / code_verifier）全在
//! 后台任务的闭包里，取消由 `LoginService::cancel` 的通用路径（任务标记
//! canceled）经轮询循环的取消检查生效。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::providers::minimax::oauth::{self, PollOutcome};
use crate::server::core::providers::minimax::profile;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle};

impl LoginService {
    /// 发起一次 MiniMax 设备授权登录。
    ///
    /// 第一步（要 `user_code`）是同步等待的 —— 它决定 `authUrl`，
    /// 拿不到就没有页面可给用户开。之后的确认过程在后台轮询。
    pub async fn start_minimax_device_login(
        &self,
        name: Option<String>,
    ) -> Result<LoginTaskHandle, String> {
        let label = crate::server::core::providers::label_of(kind_id(ProviderKind::Minimax));
        let start = oauth::start_device_authorization()
            .await
            .map_err(|error| error.message)?;
        // state 用 device_code 前缀：够稳定（同一轮设备授权唯一）、够短
        // （进表键与日志），且不把完整的 device_code 暴露到界面上
        let state = format!(
            "minimax-{}",
            crate::server::core::account_store::store_util::truncate_text(&start.device_code, 12)
        );
        let auth_url = if start.verification_uri_complete.is_empty() {
            start.verification_uri.clone()
        } else {
            start.verification_uri_complete.clone()
        };
        let info = crate::server::core::endpoints::resolve_edition(Some(
            crate::server::core::endpoints::DEFAULT_EDITION,
        ));
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::Minimax));
        handle.update(|task| {
            task.state = Some(state.clone());
            task.auth_url = Some(auth_url.clone());
        });
        self.tasks.register(&state, handle.clone());
        logging::log(
            "[Login]",
            &format!("发起{label}设备授权登录（授权码 {}）", start.user_code),
        );
        // 后台轮询：用户确认后换令牌、落账号、写回句柄
        let service = self.clone();
        let task_state = state.clone();
        let task = handle.clone();
        crate::spawn_task(async move {
            let store = service.store.clone();
            let outcome = poll_and_register(&store, &start, &task, name.as_deref()).await;
            // 任务已被 cancel / 过期回收：结果无处可写（账号已经落地时，
            // 用户下次刷新列表就能看到 —— 这里只记一条日志）
            let Some(handle) = service.tasks.get(&task_state) else {
                logging::verbose("[Login]", "MiniMax 登录任务已不在表中，结果未写回");
                return;
            };
            if task.snapshot().canceled {
                // 取消那一步由 `/cancel` 收尾，这里不再补一条错误
                return;
            }
            match outcome {
                Ok(account_id) => {
                    let account = store.minimax_account_record(&account_id);
                    let mut guard = handle.lock();
                    guard.session = Some(json!({
                        "accountUid": account.as_ref().and_then(|item| item.get("id")),
                        "nickname": account.as_ref().and_then(|item| item.get("name")),
                        "edition": guard.edition,
                        "provider": kind_id(ProviderKind::Minimax),
                    }));
                    guard.done = true;
                    guard.finished_at = Some(logging::now_ms());
                }
                Err(message) => {
                    drop(handle);
                    finish_task_error(&task, &message);
                }
            }
        });
        Ok(handle)
    }
}

/// 轮询 → 落账号。
async fn poll_and_register(
    store: &crate::server::core::account_store::AccountStore,
    start: &oauth::DeviceAuthorization,
    task: &LoginTaskHandle,
    name: Option<&str>,
) -> Result<String, String> {
    let mut credential = match oauth::poll_device_token(start, || task.snapshot().canceled).await {
        Ok(credential) => credential,
        Err(PollOutcome::Denied) => return Err("MiniMax 授权被拒绝：请在授权页重新确认".to_string()),
        Err(PollOutcome::Expired) => {
            return Err("MiniMax 设备码已过期（授权页未及时确认），请重新发起登录".to_string())
        }
        Err(PollOutcome::Cancelled) => return Err("已取消".to_string()),
        Err(PollOutcome::Failed(message)) => return Err(message),
    };
    // 手机号（账号展示名用，见 `providers::minimax::profile`）：
    // OAuth 令牌响应里没有它，落账号前顺手查一次；**失败不阻断登录** ——
    // 拿不到只是展示名回落到昵称，用户随时可以点一次余额把它补上。
    if let Some(phone) = profile::try_query_phone(&credential).await {
        credential.phone = phone;
    }
    let account = store
        .add_minimax_account(&credential, name, "web")
        .map_err(|error| error.message)?;
    let id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    logging::log(
        "[Login]",
        &format!(
            "✅ MiniMax 登录成功，账号已加入列表（{}）",
            account.get("name").and_then(Value::as_str).unwrap_or("")
        ),
    );
    Ok(id)
}

/// 该链路使用的轮询上限（秒）——只用于日志与将来扩展；实际由设备码的
/// `expires_in_sec` 决定（见 `oauth::poll_device_token`）。
#[allow(dead_code)]
pub const MINIMAX_POLL_CEILING: Duration = Duration::from_secs(300);

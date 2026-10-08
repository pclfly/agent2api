//! LobsterAI 网页登录与通用 start / wait / cancel 状态表的衔接。
//!
//! 结构与 `login/trae.rs` 同构（另开一张待办表的理由、取消必须显式 close
//! 的理由、同一时刻只允许一轮的语义都一致）—— 本家多出来的一条是
//! **回调带 state**：回调服务器只把 state 逐字匹配的请求投递进来，
//! 不匹配的回 400 并继续等（见 `providers::lobsterai::oauth` 的模块头）。
//!
//! ── 超时取任务表那一条 ────────────────────────────────────
//! 参考实现的登录 TTL 是 10 分钟，但网关所有 provider 共用
//! `LOGIN_TIMEOUT_MS`（5 分钟）与 `/wait` 协议；给一家单独延长会让界面
//! 「登录中」的计时与轮询节奏出现两种口径。5 分钟内没点完就重新发起一次。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::providers::lobsterai::oauth::Session;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

impl LoginService {
    /// 发起一次 LobsterAI 网页登录。
    ///
    /// 同步部分只做「建任务 + 登记 + 起后台」：授权地址要绑一个本机回调端口
    /// （回调 URL 的端口就是它），不能在 HTTP 处理线程上等，所以由后台任务
    /// 造好后回填，调用方用 `wait_for_auth_url` 等它落地。
    pub fn start_lobsterai_login(&self) -> Result<LoginTaskHandle, String> {
        let info = crate::server::core::endpoints::resolve_edition(Some(
            crate::server::core::endpoints::DEFAULT_EDITION,
        ));
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::LobsterAi));
        let state = format!("lobsterai-{}", logging::now_ms());
        handle.update(|task| {
            task.state = Some(state.clone());
        });
        // 先收掉上一轮（close = 释放它占着的回调端口，等待方随之退出），
        // 再把新的登记进去 —— 顺序反了就会出现两轮并存、回调落到旧端口。
        self.close_lobsterai_login();
        self.tasks.register(&state, handle.clone());
        let service = self.clone();
        let task = handle.clone();
        crate::spawn_task(async move {
            service.run_lobsterai_login(task).await;
        });
        logging::log("[Login]", "发起 LobsterAI 网页登录（等待浏览器回调…）");
        Ok(handle)
    }

    /// 后台那半个登录：起监听 + 造授权地址 → 等回调 → 换凭据 → 落账号。
    async fn run_lobsterai_login(&self, handle: LoginTaskHandle) {
        let state = handle.snapshot().state.unwrap_or_default();
        let session = match Session::begin().await {
            Ok(session) => Arc::new(session),
            Err(error) => {
                // 起不来监听 = 这一轮根本没有可等的东西。待办条目也要撤，
                // 否则界面显示「登录中」而永远等不到回调。
                self.close_lobsterai_login();
                finish_task_error(&handle, &error);
                return;
            }
        };
        if handle.snapshot().canceled {
            session.close();
            self.close_lobsterai_login();
            return;
        }
        let login_url = session.login_url.clone();
        self.lobsterai_login()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(state.clone(), session.clone());
        handle.update(|task| {
            task.auth_url = Some(login_url.clone());
        });

        let outcome = tokio::time::timeout(
            Duration::from_millis(LOGIN_TIMEOUT_MS),
            session.complete(),
        )
        .await;
        let credential = match outcome {
            Ok(Ok(credential)) => credential,
            Ok(Err(error)) => {
                self.close_lobsterai_login();
                if handle.snapshot().canceled {
                    // 取消那一步由 `/cancel` 收尾（任务已从表里移除），这里不再
                    // 补一条错误 —— 否则两条路径会给出两个互相矛盾的结论。
                    return;
                }
                finish_task_error(&handle, &error.message);
                return;
            }
            Err(_) => {
                self.close_lobsterai_login();
                finish_task_error(&handle, "LobsterAI 网页登录超时（5 分钟），请重新发起");
                return;
            }
        };
        // 落账号与取消共用同一把任务锁：取消先发生就不落账号，落盘先发生则
        // 视为已完成（与 trae / zcode / qoder 三条链逐字相同的处置）。
        let mut task = handle.lock();
        if task.done || task.canceled {
            drop(task);
            self.close_lobsterai_login();
            return;
        }
        match self.store.add_lobsterai_account(&credential, None, "web") {
            Ok(account) => {
                task.session = Some(json!({
                    "accountUid": account.get("id"),
                    "nickname": account.get("name"),
                    "edition": task.edition,
                    "provider": kind_id(ProviderKind::LobsterAi),
                }));
                logging::log(
                    "[Login]",
                    &format!(
                        "✅ LobsterAI 网页登录完成，账号已加入列表（{}）",
                        account.get("name").and_then(Value::as_str).unwrap_or("")
                    ),
                );
            }
            Err(error) => task.error = Some(error.message),
        }
        task.done = true;
        task.finished_at = Some(logging::now_ms());
        drop(task);
        self.close_lobsterai_login();
    }

    /// 关掉所有进行中的 LobsterAI 登录（本家同一时刻只有一轮，所以不传 state）。
    ///
    /// 顺序是「先摘表、后 close」：close 会让等待方以 `None` 退出，那条路径
    /// 也会走到这里（幂等），表里已经没有它了，就不会出现「边遍历边改」的锁重入。
    pub(crate) fn close_lobsterai_login(&self) {
        let orphaned: Vec<Arc<Session>> = self
            .lobsterai_login()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in orphaned {
            session.close();
        }
    }
}

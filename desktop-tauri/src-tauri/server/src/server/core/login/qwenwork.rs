//! QwenWork device login orchestration; sensitive PKCE state stays in the worker.
use super::{LoginService, LoginTaskHandle};
use crate::server::{
    core::{
        account_store::AccountStore,
        providers::qwenwork::{credentials::Credentials, login},
    },
    errors::GatewayError,
    logging,
};
use serde_json::json;

impl LoginService {
    pub fn start_qwenwork_login(&self, name: Option<String>) -> Result<LoginTaskHandle, String> {
        let start = login::DeviceAuthorization::new().map_err(|e| e.message)?;
        let handle = self.new_handle_for_provider(
            crate::server::core::endpoints::resolve_edition(Some("intl")),
            "qwenwork",
        );
        handle.update(|task| {
            task.state = Some(start.state.clone());
            task.auth_url = Some(start.auth_url.clone());
        });
        self.tasks.register(&start.state, handle.clone());
        let task = handle.clone();
        let store = self.store.clone();
        logging::log(
            "[Login]",
            "发起 QwenWork 设备确认登录，请在官方授权页完成确认",
        );
        crate::spawn_task(async move {
            let outcome = login::complete(&start, || task.snapshot().canceled).await;
            let completed = finish(&task, &store, outcome, name.as_deref());
            // Never log the auth URL, nonce, verifier or arbitrary response body.
            if let Some(success) = completed {
                logging::log(
                    "[Login]",
                    if success {
                        "QwenWork 登录成功，账号已加入列表"
                    } else {
                        "QwenWork 登录未完成，请查看界面提示"
                    },
                );
            }
        });
        Ok(handle)
    }
}

/// Commit and cancellation share the handle lock: cancellation that wins the lock
/// prevents storage, while an already committed success is not reported as canceled.
/// No await/logging while holding this lock (account storage acquires the DB lock).
fn finish(
    task: &LoginTaskHandle,
    store: &AccountStore,
    outcome: Result<Credentials, GatewayError>,
    name: Option<&str>,
) -> Option<bool> {
    let mut guard = task.lock();
    if guard.canceled || guard.done {
        return None;
    }
    let result = outcome.and_then(|c| {
        store
            .add_qwenwork_account(&c.payload(), name, "web")
            .map_err(|e| GatewayError::with_status(e.status_code, e.message))
    });
    let success = result.is_ok();
    match result {
        Ok(account) => {
            guard.session = Some(
                json!({"accountUid":account.get("id"), "nickname":account.get("name"), "edition":"intl", "provider":"qwenwork"}),
            )
        }
        Err(e) => guard.error = Some(e.message),
    }
    guard.done = true;
    guard.finished_at = Some(logging::now_ms());
    Some(success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::login::{LoginTaskState, LoginTasks};
    use std::sync::{Arc, Mutex};

    fn handle() -> LoginTaskHandle {
        LoginTaskHandle {
            inner: Arc::new(Mutex::new(LoginTaskState {
                state: Some("test-state".into()),
                provider: "qwenwork".into(),
                ..Default::default()
            })),
            ticket: 1,
        }
    }
    fn credential() -> Credentials {
        Credentials::from_value(&json!({"token":"synthetic-login-token","refreshToken":"synthetic-login-refresh","userId":"login-user","oauthClientId":"test-client","oauthRedirectUri":"qwenwork://"})).unwrap()
    }
    #[test]
    fn qwenwork_login_cancel_prevents_storage_and_success_is_public_and_idempotent() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("qwenwork-login");
        let store = AccountStore::with_db(Some(db));
        let task = handle();
        let tasks = LoginTasks::new();
        tasks.register("test-state", task.clone());
        assert!(tasks.cancel("test-state"));
        assert_eq!(finish(&task, &store, Ok(credential()), None), None);
        assert!(store.qwenwork_account_record("").is_none());
        let task = handle();
        tasks.register("test-state", task.clone());
        assert_eq!(
            finish(&task, &store, Ok(credential()), Some("My QwenWork")),
            Some(true)
        );
        assert!(!tasks.cancel("test-state"));
        let response = task.snapshot().to_wait_response();
        assert_eq!(response["done"], true);
        assert_eq!(response["session"]["provider"], "qwenwork");
        assert_eq!(response["session"]["nickname"], "My QwenWork");
        assert!(!response.to_string().contains("synthetic-login"));
        let id = response["session"]["accountUid"].as_str().unwrap();
        let record = store.qwenwork_account_record(id).unwrap();
        assert_eq!(record["source"], "web");
        assert_eq!(record["refreshManaged"], true);
        assert_eq!(record["oauthClientId"], "test-client");
        assert_eq!(finish(&task, &store, Ok(credential()), None), None);
        assert_eq!(
            store.qwenwork_account_record(id).unwrap()["credentialRevision"],
            record["credentialRevision"]
        );
        let next = handle();
        assert_eq!(finish(&next, &store, Ok(credential()), None), Some(true));
        assert_eq!(next.snapshot().session.unwrap()["accountUid"], id);
        assert_eq!(
            store.qwenwork_account_record(id).unwrap()["name"],
            "My QwenWork"
        );
    }
    #[test]
    fn qwenwork_login_failure_is_terminal_without_a_session() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("qwenwork-login-error");
        let store = AccountStore::with_db(Some(db));
        let task = handle();
        assert_eq!(
            finish(
                &task,
                &store,
                Err(GatewayError::new("synthetic failure")),
                None
            ),
            Some(false)
        );
        let response = task.snapshot().to_wait_response();
        assert_eq!(response["done"], true);
        assert_eq!(response["error"], "synthetic failure");
        assert!(response.get("session").is_none());
        assert!(store.qwenwork_account_record("").is_none());
    }
}

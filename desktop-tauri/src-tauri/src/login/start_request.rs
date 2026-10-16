//! Local start reservations keep deferred gateway responses out of later login rounds.
use std::future::Future;
use std::time::Duration;

use serde_json::Value;

use crate::state::ActiveLogin;

pub const PREFIX: &str = "local-start:";

pub fn reserve(slot: &mut Option<ActiveLogin>, active: ActiveLogin) -> Result<(), String> {
    if slot.is_some() {
        return Err("已有登录流程在进行，请先完成或取消当前登录".into());
    }
    *slot = Some(active);
    Ok(())
}

pub fn promote(slot: &mut Option<ActiveLogin>, expected: &str, server_state: &str) -> bool {
    let Some(active) = slot.as_mut().filter(|active| active.state == expected) else {
        return false;
    };
    active.state = server_state.to_string();
    true
}

/// Dropping a request could lose the newly created server state. Keep only its
/// cleanup worker alive after cancellation; never let it open a browser or touch a successor.
pub async fn wait<F, C, D, DF>(request: F, current: C, cleanup: D) -> Result<Option<Value>, String>
where
    F: Future<Output = Result<Value, String>> + Send + 'static,
    C: Fn() -> bool,
    D: Fn(Value) -> DF + Send + 'static,
    DF: Future<Output = ()> + Send + 'static,
{
    let mut request = tokio::spawn(request);
    loop {
        tokio::select! {
            result = &mut request => {
                let result = result.map_err(|_| "登录启动任务异常结束".to_string())?;
                if !current() {
                    if let Ok(started) = result { tokio::spawn(cleanup(started)); }
                    return Ok(None);
                }
                return result.map(Some);
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if !current() {
                    tokio::spawn(async move {
                        if let Ok(Ok(started)) = request.await { cleanup(started).await; }
                    });
                    return Ok(None);
                }
            }
        }
    }
}

/// Once a state is known, dropping /wait is safe. Check cancellation before and
/// after the response so a late success cannot escape into a canceled UI round.
pub async fn while_current<T>(
    future: impl Future<Output = T>,
    current: impl Fn() -> bool,
    deadline: tokio::time::Instant,
) -> Result<Option<T>, String> {
    tokio::pin!(future);
    loop {
        if !current() {
            return Ok(None);
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => return Err("登录等待超时（5 分钟）".into()),
            result = &mut future => return Ok(current().then_some(result)),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}

pub async fn finish<F, C>(
    result: Result<Value, String>,
    cleanup: C,
    timeout: Duration,
) -> Result<Value, String>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<(), String>>,
{
    if result
        .as_ref()
        .map(|v| v.get("ok").and_then(Value::as_bool))
        == Ok(Some(true))
    {
        return result;
    }
    if !matches!(tokio::time::timeout(timeout, cleanup()).await, Ok(Ok(()))) {
        let warning = "已停止本地等待，但未确认服务端取消；请勿继续授权，任务将自动过期";
        return Err(match result {
            Err(error) => format!("{error}；{warning}"),
            Ok(_) => warning.to_string(),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn active(state: &str) -> ActiveLogin {
        ActiveLogin {
            state: state.into(),
            edition: "intl".into(),
            mode: "external".into(),
            provider: "qwenwork".into(),
        }
    }

    #[tokio::test]
    async fn canceled_deferred_start_is_cleaned_without_overwriting_retry() {
        let slot = Arc::new(Mutex::new(None));
        reserve(&mut slot.lock().unwrap(), active("local-start:1")).unwrap();
        assert!(reserve(&mut slot.lock().unwrap(), active("local-start:2")).is_err());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (clean_tx, clean_rx) = tokio::sync::oneshot::channel();
        let clean_tx = Arc::new(Mutex::new(Some(clean_tx)));
        let check_slot = slot.clone();
        let task = tokio::spawn(wait(
            async move { Ok(rx.await.unwrap()) },
            move || {
                check_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|a| a.state == "local-start:1")
            },
            move |value| {
                let clean_tx = clean_tx.clone();
                async move {
                    clean_tx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send(value)
                        .unwrap();
                }
            },
        ));
        *slot.lock().unwrap() = None;
        reserve(&mut slot.lock().unwrap(), active("local-start:2")).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        tx.send(json!({"state":"old-server-state","authUrl":"https://example.invalid/"}))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), clean_rx)
                .await
                .unwrap()
                .unwrap()["state"],
            "old-server-state"
        );
        assert!(!promote(
            &mut slot.lock().unwrap(),
            "local-start:1",
            "old-server-state"
        ));
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().state,
            "local-start:2"
        );
        assert!(promote(
            &mut slot.lock().unwrap(),
            "local-start:2",
            "new-server-state"
        ));
    }

    #[tokio::test]
    async fn promoted_wait_cancels_and_cleanup_never_holds_a_successor() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let current = Arc::new(AtomicBool::new(true));
        let signal = current.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            while_current(
                async move {
                    entered_tx.send(()).unwrap();
                    std::future::pending::<Value>().await
                },
                || signal.load(Ordering::SeqCst),
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await
        });
        // Wait until the synthetic HTTP future has actually been polled.
        entered_rx.await.unwrap();
        current.store(false, Ordering::SeqCst);
        assert!(tokio::time::timeout(Duration::from_millis(200), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        current.store(true, Ordering::SeqCst);
        let late = while_current(
            async {
                current.store(false, Ordering::SeqCst);
                json!({"ok":true})
            },
            || current.load(Ordering::SeqCst),
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(
            late.is_none(),
            "a ready response still must recheck the round"
        );
        for result in [
            Ok(json!({"canceled":true})),
            Err("synthetic login failure".into()),
        ] {
            let original_failed = result.is_err();
            let finished = tokio::time::timeout(
                Duration::from_millis(100),
                finish(result, || std::future::pending(), Duration::from_millis(5)),
            )
            .await
            .unwrap();
            let error = finished.unwrap_err();
            assert!(error.contains("未确认服务端取消"));
            if original_failed {
                assert!(error.contains("synthetic login failure"));
            }
        }
        assert!(finish(
            Ok(json!({"ok":true})),
            || async { panic!("success is not canceled") },
            Duration::from_millis(5)
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn current_start_returns_response_and_failure_without_cleanup() {
        let response = wait(
            async { Ok(json!({"state":"current"})) },
            || true,
            |_| async { panic!("current task must not be cleaned up") },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response["state"], "current");
        assert!(wait(
            async { Err("synthetic failure".into()) },
            || true,
            |_| async { panic!("failed request has no state") }
        )
        .await
        .is_err());
    }
}

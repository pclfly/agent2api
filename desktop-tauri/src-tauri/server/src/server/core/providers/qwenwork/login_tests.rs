use super::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

fn grant(start: &DeviceAuthorization) -> Value {
    json!({"token":"synthetic-access", "refresh_token":"synthetic-refresh", "expires_in":3600,
        "nonce":start.nonce,"code_challenge":start.challenge,"code_challenge_method":"S256"})
}
fn profile() -> Value {
    json!({"data":{"user":{"id":"synthetic-user","username":"Example","email":"test@example.invalid"}}})
}

#[test]
fn international_qwenwork_login_contract_and_pkce_are_bound() {
    let start = DeviceAuthorization::new().unwrap();
    let other = DeviceAuthorization::new().unwrap();
    assert_ne!(start.state, other.state);
    assert_ne!(start.nonce, other.nonce);
    assert_ne!(start.verifier, other.verifier);
    assert_ne!(start.state, start.nonce);
    assert_eq!(start.verifier.len(), 43);
    assert_eq!(
        start.challenge,
        URL_SAFE_NO_PAD.encode(Sha256::digest(start.verifier.as_bytes()))
    );
    let url = reqwest::Url::parse(&start.auth_url).unwrap();
    assert_eq!(url.origin().ascii_serialization(), auth::BASE);
    assert_eq!(url.path(), "/device/selectAccounts");
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "cc65e5fc-05bd-4f5d-a4e8-0df19aa3d75a");
    assert_eq!(query["redirect_uri"], "qwenwork://");
    assert_eq!(query["challenge"], start.challenge);
    assert_eq!(query["nonce"], start.nonce);
    assert_eq!(query["challenge_method"], "S256");
    assert_eq!(query["machine_id"].len(), 36);
    assert!(!start.auth_url.contains(&start.verifier));
    assert!(!start.auth_url.contains(&start.state));
    let client = reqwest::Client::new();
    let poll = poll_request(&client, &start).unwrap().build().unwrap();
    assert_eq!(poll.url().path(), "/api/v1/deviceToken/poll");
    assert_eq!(poll.method(), reqwest::Method::GET);
    assert!(poll.body().is_none());
    assert!(!poll.headers().contains_key("authorization"));
    let query: std::collections::HashMap<_, _> = poll.url().query_pairs().collect();
    assert_eq!(query["verifier"], start.verifier);
    assert_eq!(query["nonce"], start.nonce);
    assert_eq!(query["challenge_method"], "S256");
    assert_eq!(poll.headers()["x-qwenwork-channel"], "stable");
    assert_eq!(
        poll.headers()["x-qwenwork-release-version"],
        "1.0.4-26090501"
    );
    let user = profile_request(&client, "synthetic-access")
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(user.headers()["authorization"], "Bearer synthetic-access");
    assert_eq!(
        user.url().as_str(),
        format!("{}{}", auth::BASE, auth::ACCOUNT)
    );
}

#[test]
fn qwenwork_login_rejects_mismatched_bindings_and_bad_responses() {
    let start = DeviceAuthorization::new().unwrap();
    assert!(parse_poll(&start, 404, b"not json").unwrap().is_none());
    assert!(parse_poll(&start, 200, b"{}").unwrap().is_none());
    assert!(
        parse_poll(&start, 200, grant(&start).to_string().as_bytes())
            .unwrap()
            .is_some()
    );
    for key in [
        "nonce",
        "code_challenge",
        "code_challenge_method",
        "refresh_token",
    ] {
        let mut bad = grant(&start);
        bad[key] = json!("wrong");
        if key == "refresh_token" {
            bad[key] = Value::Null;
        }
        let err = parse_poll(&start, 200, bad.to_string().as_bytes())
            .err()
            .unwrap();
        assert!(!err.message.contains("synthetic"));
    }
    for (status, body) in [
        (403, "synthetic-secret"),
        (500, "invalid token synthetic-secret"),
        (302, "redirect"),
        (200, "not-json"),
        (200, "[]"),
        (200, "{\"success\":false}"),
        (200, "{\"token\":123}"),
    ] {
        let err = parse_poll(&start, status, body.as_bytes()).err().unwrap();
        assert!(!err.message.contains("synthetic-secret"));
    }
}

#[test]
fn qwenwork_login_requires_profile_identity_and_keeps_oauth_refresh_pair() {
    let start = DeviceAuthorization::new().unwrap();
    let c = credentials_from(&grant(&start), &profile(), 1000).unwrap();
    assert_eq!(c.user_id, "synthetic-user");
    assert_eq!(c.name, "Example");
    assert_eq!(c.expires_at, 3_601_000);
    assert_eq!(c.client_id, CLIENT_ID);
    assert_eq!(c.redirect_uri, REDIRECT_URI);
    for p in [
        json!({}),
        json!({"data":{"user":{"email":"a"}}}),
        json!({"success":false,"user":{"id":"u"}}),
    ] {
        assert!(credentials_from(&grant(&start), &p, 1000).is_err());
    }
    let mut wrong = grant(&start);
    wrong["user_id"] = json!("other-user");
    assert!(credentials_from(&wrong, &profile(), 1000).is_err());
}

#[tokio::test]
async fn qwenwork_login_pending_success_failure_and_cancel_are_bounded() {
    let start = DeviceAuthorization::new().unwrap();
    let mut calls = 0;
    let c = complete_with(
        || {
            calls += 1;
            std::future::ready(Ok(if calls < 3 { None } else { Some(grant(&start)) }))
        },
        |_| std::future::ready(Ok(profile())),
        || false,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(c.user_id, "synthetic-user");
    let mut calls = 0;
    let error = complete_with(
        || {
            calls += 1;
            std::future::ready(Err(PollFailure::Network))
        },
        |_| std::future::ready(Ok(profile())),
        || false,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(calls, 5);
    assert!(!error.message.contains("http"));
    let canceled = Arc::new(AtomicBool::new(false));
    let signal = canceled.clone();
    let profile_calls = AtomicUsize::new(0);
    let result = complete_with(
        || async {
            signal.store(true, Ordering::SeqCst);
            Ok(Some(grant(&start)))
        },
        |_| {
            profile_calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(profile()))
        },
        || canceled.load(Ordering::SeqCst),
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(profile_calls.load(Ordering::SeqCst), 0);
    let timed = complete_with(
        || std::future::pending::<Result<Option<Value>, PollFailure>>(),
        |_| std::future::ready(Ok(profile())),
        || false,
        Duration::from_millis(5),
        Duration::from_millis(1),
    )
    .await;
    assert!(timed.err().unwrap().message.contains("超时"));
}

#[tokio::test]
async fn qwenwork_login_interrupts_inflight_poll_and_profile_on_cancel_or_deadline() {
    for in_profile in [false, true] {
        for cancel in [false, true] {
            let start = DeviceAuthorization::new().unwrap();
            let canceled = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(tokio::sync::Notify::new());
            let notifier = entered.clone();
            let signal = canceled.clone();
            let trigger = tokio::spawn(async move {
                notifier.notified().await;
                if cancel {
                    signal.store(true, Ordering::SeqCst);
                }
            });
            let result = complete_with(
                || async {
                    if in_profile {
                        Ok(Some(grant(&start)))
                    } else {
                        entered.notify_one();
                        std::future::pending().await
                    }
                },
                |_| async {
                    entered.notify_one();
                    std::future::pending().await
                },
                || canceled.load(Ordering::SeqCst),
                if cancel {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(30)
                },
                Duration::from_millis(1),
            )
            .await;
            trigger.await.unwrap();
            assert!(result.err().unwrap().message.contains(if cancel {
                "取消"
            } else {
                "超时"
            }));
        }
    }
}

#[tokio::test]
async fn qwenwork_login_pending_resets_consecutive_network_failure_limit() {
    let start = DeviceAuthorization::new().unwrap();
    let mut calls = 0;
    let result = complete_with(
        || {
            calls += 1;
            std::future::ready(match calls {
                5 => Ok(None),
                10 => Ok(Some(grant(&start))),
                _ => Err(PollFailure::Network),
            })
        },
        |_| std::future::ready(Ok(profile())),
        || false,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(calls, 10);
    assert_eq!(result.user_id, "synthetic-user");
}

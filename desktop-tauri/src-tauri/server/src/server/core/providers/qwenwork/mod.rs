//! QwenWork: stateless COSY-signed chat, explicit credentials, model catalog and balance.
//! Daily check-in has NO upstream endpoint on the international host (verified live,
//! 2026-10-15/16: every `/sash/*` path and 60+ credit/reward candidates return 404).
//! The daily credits are granted server-side on first real use; the gateway can only
//! read the balance and compare it against the last cross-day reading
//! (`daily_gift.rs`). `checkin.rs` keeps the statically verified `/sash` claim path
//! as a dormant fallback.
pub mod auth;
pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod daily_gift;
pub mod login;
pub mod models;
pub mod protocol;
pub mod refresh;
pub mod stream;
use crate::server::{
    core::{
        account_store::AccountStore,
        providers::{
            adapter::{
                ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
                UpstreamResponse,
            },
            ProviderKind,
        },
    },
    errors::GatewayError,
};
use axum::http::HeaderMap;
use serde_json::{json, Value};
use std::{future::Future, pin::Pin};
type Async<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub struct QwenWorkAdapter;
pub static QWENWORK_ADAPTER: QwenWorkAdapter = QwenWorkAdapter;
impl ProviderAdapter for QwenWorkAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::QwenWork
    }
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let c = credentials::Credentials::from_value(
            &json!({"token":account.pointer("/auth/accessToken"),"userId":account.pointer("/account/uid"),"user":{"name":account.pointer("/account/nickname")},"email":account.get("email")}),
        )?;
        let body = protocol::build(&c, body, headers)?;
        let mut headers = auth::cosy_headers(&c, auth::CHAT, &body.to_string())?;
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push(("Accept".into(), "text/event-stream".into()));
        Ok(ChatRequestPlan {
            url: format!("{}{}", auth::BASE, auth::CHAT),
            headers,
            body,
            response: UpstreamResponse::QwenWorkSse,
        })
    }
    fn classify_error(&self, status: u16, body: &Value) -> UpstreamErrorClass {
        let status = auth::error_status(status, &body.to_string());
        let message = format!("QwenWork 上游请求失败（{status}）");
        match status {
            401 => UpstreamErrorClass::TokenExpired { message },
            429 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            },
            _ => UpstreamErrorClass::Fatal {
                status,
                message,
                upstream_code: None,
            },
        }
    }
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        id: &'a str,
    ) -> Async<'a, Result<String, GatewayError>> {
        Box::pin(async move { refresh::ensure(store, id, false).await.map(|c| c.token) })
    }
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        id: &'a str,
    ) -> Async<'a, Result<String, GatewayError>> {
        Box::pin(async move { refresh::ensure(store, id, true).await.map(|c| c.token) })
    }
    fn supports_refresh(&self) -> bool {
        true
    }
    fn credentials_expiring(&self, store: &AccountStore, id: &str) -> bool {
        store
            .qwenwork_account_record(id)
            .filter(|r| r.get("refreshManaged") != Some(&Value::Bool(false)))
            .and_then(|r| credentials::Credentials::from_value(&r).ok())
            .is_some_and(|c| !c.refresh_token.is_empty() && c.expiring())
    }
    fn supports_model_refresh(&self) -> bool {
        true
    }
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        id: &'a str,
        force: bool,
    ) -> Async<'a, ModelRefreshOutcome> {
        Box::pin(async move {
            if id.is_empty() && store.qwenwork_account_record("").is_none() {
                ModelRefreshOutcome::unchanged()
            } else {
                models::fetch(store, id, force).await
            }
        })
    }
    fn supports_usage(&self) -> bool {
        true
    }
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        id: &'a str,
    ) -> Async<'a, Result<Value, GatewayError>> {
        Box::pin(balance::query(store, id))
    }
    fn sse_model_rewrite(&self) -> bool {
        true
    }
}

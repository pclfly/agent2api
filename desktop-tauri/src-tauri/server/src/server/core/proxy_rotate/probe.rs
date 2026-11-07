//! 出口健康探测：发一个不带凭据的 `HEAD origin/`，证明「这条出口能到那个上游」。
//!
//! ── 与冷却表的分工 ──────────────────────────────────────────
//! `health` 记的是**事后**结论（这次请求失败了 → 冷却）；本模块做的是**事前**
//! 判断（这条出口现在真的能到上游吗）。两者要一起看：
//!   · 冷却说「刚失败过，先别用」——但它到期时**并不能保证**出口已恢复；
//!   · 探测说「确实能到」——于是冷却到期后不必盲试，可以先用一次探测确认。
//!
//! ── 探什么、不探什么 ────────────────────────────────────────
//! 探 `HEAD origin/`：
//!   · **不带**账号凭证（不泄露 token，也不消耗额度）；
//!   · **不跟随**重定向（否则探测会沿着上游的跳转跑到别的地方）；
//!   · **任何** HTTP 状态码都算「通」—— 401/403/404 恰恰证明 TLS 握手与
//!     路由都正常，只是这个路径要鉴权。只有连不上才算不通。
//!
//! ── 结果缓存 ────────────────────────────────────────────────
//! 成功缓存 30 秒、失败 1 秒。失败缓存短得多：一次抖动不该让出口被判定
//! 「不通」半分钟。同一 `(出口, origin)` 的并发探测用 singleflight 合并 ——
//! 否则十个并发请求会同时向同一上游打十个 HEAD。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;

/// 单次探测的总预算。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 成功结果的缓存时长。
const SUCCESS_TTL: Duration = Duration::from_secs(30);
/// 失败结果的缓存时长（短：一次抖动不该判半小时）。
const FAILURE_TTL: Duration = Duration::from_secs(1);
/// 缓存条目上限。
const MAX_PROBES: usize = 1024;

type Key = (String, String);

/// 探测结论。
#[derive(Clone, Debug)]
pub struct ProbeError {
    pub message: String,
    /// 是超时（而不是立刻失败）
    pub timeout: bool,
    /// 是本机的错（地址不合法 / 建客户端失败），不是出口的问题
    pub local: bool,
}

struct CachedResult {
    until: Instant,
    result: Result<(), ProbeError>,
}

struct Entry {
    gate: Arc<tokio::sync::Mutex<Option<CachedResult>>>,
    used: Instant,
}

fn cache() -> &'static Mutex<HashMap<Key, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gate(key: &Key) -> Arc<tokio::sync::Mutex<Option<CachedResult>>> {
    let now = Instant::now();
    let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
    // 淘汰：超过成功 TTL 且没人在等
    cache.retain(|_, entry| {
        now.saturating_duration_since(entry.used) < SUCCESS_TTL
            || Arc::strong_count(&entry.gate) > 1
    });
    if let Some(entry) = cache.get_mut(key) {
        entry.used = now;
        return entry.gate.clone();
    }
    if cache.len() >= MAX_PROBES {
        let oldest = cache
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.gate) == 1)
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            cache.remove(&oldest);
        }
    }
    let gate = Arc::new(tokio::sync::Mutex::new(None));
    if cache.len() < MAX_PROBES {
        cache.insert(
            key.clone(),
            Entry {
                gate: gate.clone(),
                used: now,
            },
        );
    }
    gate
}

/// 让某个 `(出口, origin)` 的缓存失效（冷却恢复后想立刻重探时用）。
pub fn invalidate(route: &str, origin: &str) {
    cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|(identity, target), _| identity != route || target != origin);
}

/// 清空全部探测缓存。
pub fn clear() {
    cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// 探测一条出口能否到达某个上游 origin。
///
/// `route` 是调用方给的出口标识（一般用 `health::proxy_identity` 的哈希，
/// 或池条目 id）—— 只用于缓存分组，不参与请求。
pub async fn check(
    route: String,
    origin: &str,
    proxy: &ResolvedProxy,
    accept_any_cert: bool,
    deadline: tokio::time::Instant,
) -> Result<(), ProbeError> {
    // 目标规范化到 origin + '/'：不带路径、不带 query（否则会把账号路径写进探测）
    let target = url::Url::parse(origin)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .and_then(|url| url::Url::parse(&format!("{}/", url.origin().ascii_serialization())).ok())
        .ok_or_else(|| ProbeError {
            message: "探测目标无效（需要 http/https origin）".to_string(),
            timeout: false,
            local: true,
        })?;
    let key = (route, target.origin().ascii_serialization());
    let gate = gate(&key);
    let until = deadline.min(tokio::time::Instant::now() + PROBE_TIMEOUT);
    let mut state = tokio::time::timeout_at(until, gate.lock())
        .await
        .map_err(|_| ProbeError {
            message: "等待出口探测超时".to_string(),
            timeout: true,
            local: true,
        })?;
    if let Some(cached) = state.as_ref().filter(|cached| cached.until > Instant::now()) {
        return cached.result.clone();
    }
    if tokio::time::Instant::now() >= until {
        return Err(ProbeError {
            message: "出口探测预算已耗尽".to_string(),
            timeout: true,
            local: true,
        });
    }
    // 探测客户端：**只**用这条出口，且不跟随重定向
    let client = egress::client_without_redirects(Some(proxy));
    // 任何 HTTP 响应都证明隧道与目标握手正常（见模块头）
    let result = match tokio::time::timeout_at(until, client.head(target).send()).await {
        Ok(Ok(_response)) => Ok(()),
        Ok(Err(error)) => Err(ProbeError {
            message: egress::describe_error_detail(&error),
            timeout: error.is_timeout(),
            local: error.is_builder(),
        }),
        Err(_) => Err(ProbeError {
            message: "出口探测超时（最多 5 秒）".to_string(),
            timeout: true,
            local: false,
        }),
    };
    let ttl = if result.is_ok() { SUCCESS_TTL } else { FAILURE_TTL };
    *state = Some(CachedResult {
        until: Instant::now() + ttl,
        result: result.clone(),
    });
    let _ = accept_any_cert; // 当前统一走严格校验（见模块头）
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn endpoint(
        status: u16,
        headers: &'static str,
    ) -> (
        ResolvedProxy,
        Arc<AtomicUsize>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read_u8().await {
                        Ok(byte) => request.push(byte),
                        Err(_) => return,
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
                let response = format!(
                    "HTTP/1.1 {status} Test\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        (
            ResolvedProxy {
                source: format!("probe-{port}"),
                protocol: "http".into(),
                host: "127.0.0.1".into(),
                port: Some(port),
                username: String::new(),
                password: String::new(),
                label: "probe".into(),
            },
            hits,
            rx,
            task,
        )
    }

    fn deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(2)
    }

    /// 任何 HTTP 状态码都算通；且探测请求不带凭据、不跟跳转、不带路径。
    #[tokio::test]
    async fn any_http_status_proves_the_tunnel_without_credentials() {
        for status in [200, 301, 401, 403, 404, 500] {
            let (proxy, hits, mut rx, task) =
                endpoint(status, "Location: http://must-not-follow.invalid/\r\n").await;
            let route = format!("probe-{}-{status}", proxy.port.unwrap());
            check(
                route.clone(),
                "http://target.invalid/private?secret=must-not-send",
                &proxy,
                false,
                deadline(),
            )
            .await
            .unwrap();
            let request = rx.recv().await.unwrap();
            assert!(request.starts_with("HEAD http://target.invalid/ HTTP/1.1\r\n"), "{request}");
            for forbidden in ["authorization:", "cookie:", "secret=", "/private", "must-not-follow"] {
                assert!(
                    !request.to_ascii_lowercase().contains(forbidden),
                    "探测请求不该包含 {forbidden}: {request}"
                );
            }
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            invalidate(&route, "http://target.invalid");
            task.abort();
        }
    }

    #[tokio::test]
    async fn successful_probes_are_single_flight_and_scoped() {
        let (proxy, hits, _, task) = endpoint(401, "").await;
        let route = format!("singleflight-{}", proxy.port.unwrap());
        let (a, b) = tokio::join!(
            check(route.clone(), "http://one.invalid", &proxy, false, deadline()),
            check(route.clone(), "http://one.invalid", &proxy, false, deadline()),
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "并发探测应当合并成一次");
        check(route.clone(), "http://one.invalid", &proxy, false, deadline())
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "30 秒内复用缓存");
        check(route.clone(), "http://two.invalid", &proxy, false, deadline())
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "换 origin 是另一个缓存键");
        invalidate(&route, "http://one.invalid");
        check(route.clone(), "http://one.invalid", &proxy, false, deadline())
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3, "invalidate 之后要重新探");
        invalidate(&route, "http://one.invalid");
        invalidate(&route, "http://two.invalid");
        task.abort();
    }

    #[tokio::test]
    async fn budget_already_spent_sends_nothing() {
        let (proxy, hits, _, task) = endpoint(200, "").await;
        let error = check(
            format!("expired-{}", proxy.port.unwrap()),
            "http://target.invalid",
            &proxy,
            false,
            tokio::time::Instant::now() - Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(error.timeout && error.local);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "预算已耗尽时不该发出探测");
        task.abort();
    }

    #[tokio::test]
    async fn invalid_target_is_a_local_error() {
        let (proxy, hits, _, task) = endpoint(200, "").await;
        let error = check(
            format!("bad-target-{}", proxy.port.unwrap()),
            "ftp://target.invalid",
            &proxy,
            false,
            deadline(),
        )
        .await
        .unwrap_err();
        assert!(error.local);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        task.abort();
    }
}

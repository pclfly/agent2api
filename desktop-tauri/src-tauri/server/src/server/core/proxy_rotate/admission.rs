//! 轮询出口的准入控制（按目标 origin + 出口 IP 限并发、排队、超时）。
//!
//! ── 这一层解决什么 ──────────────────────────────────────────
//! 轮询的意义是「相邻两次出网落在不同 IP」，但**同一时刻**仍可能有多个请求
//! 落在同一条出口上。而轮询服务的出口常常是「按 IP 计额度」的免费档（
//! OpenCode Zen 就是）：同一 IP 上并发十几个请求，上游会把它们一起判成超额
//! —— 于是「换 IP」的收益被自己的并发吃掉了。
//!
//! 所以这一层按 **(目标 origin, 出口 IP)** 计数：同一组合上同时在飞的请求数
//! 不超过上限，多出来的排队；排队超过上限就拒绝，等待超过时限也拒绝。
//!
//! ── 为什么 origin 也参与 ───────────────────────────────────
//! 「同一出口对 A 家已经很挤」不代表对 B 家也挤：不同上游的配额体系独立，
//! 按 origin 分开计数才与上游的判定口径一致。
//!
//! ── 出口 IP 从哪来 ─────────────────────────────────────────
//! 不自己查 IP（那要多一次往返，且会泄露本机在探测）。用**出口标识**代替：
//! 同一个代理条目 → 同一个出口 → 同一个槽位。判据是
//! `(host, port, username)` 的哈希（不含密码：同一账号不同密码仍是同一出口）。
//! 对「一条代理对应一个固定出口 IP」的常见配置，这就等价于按 IP 计数。
//!
//! ── 与账号并发上限（`maxConcurrent`）的分工 ─────────────────
//! 那个是「这一台账号同时接几个请求」，这一层是「这一条出口同时叠几个请求」。
//! 两者正交：一个账号可以有多条出口，一条出口也可能被多个账号共用。
//!
//! ── 拒绝时的语义 ───────────────────────────────────────────
//! 满载拒绝**不**算账号限额、也**不**记出口冷却：那是「本机排队排满了」，
//! 与上游怎么回没关系。文案里明说这一点，避免用户去查上游额度。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::proxy_settings::RotateBalanceSettings;

/// 单个 (origin, 出口) 组合的占用槽。
#[derive(Default)]
struct Slot {
    /// 当前在飞的请求数
    in_flight: usize,
    /// 这个槽的版本号（占用/释放时推进，用于「等一个变化」）
    revision: u64,
}

#[derive(Default)]
struct AdmissionTable {
    slots: HashMap<(String, String), Slot>,
    /// 全局版本号：任何槽的变化都推进它（等待方按它判断「期间有没有变化」）
    revision: u64,
}

/// 进程级准入器。
pub struct Admission {
    table: Mutex<AdmissionTable>,
    notify: Notify,
    /// 被拒绝的总次数（诊断用）
    rejected: AtomicU64,
    /// 排队超时的总次数（诊断用）
    timed_out: AtomicU64,
}

fn admission() -> &'static Arc<Admission> {
    static GLOBAL: OnceLock<Arc<Admission>> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        Arc::new(Admission {
            table: Mutex::new(AdmissionTable::default()),
            notify: Notify::new(),
            rejected: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
        })
    })
}

fn lock(table: &Mutex<AdmissionTable>) -> MutexGuard<'_, AdmissionTable> {
    table.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 出口 ID：同一代理条目 → 同一槽位（见模块头「出口 IP 从哪来」）。
///
/// 不含密码与 `source`：换密码不改变出口 IP，换「怎么拿到这条代理」也不改变。
/// 含 `username`：同一台代理上的不同账号可能是**不同出口**（代理服务商常这么
/// 做 —— 一个用户一个出口 IP）。
fn exit_id(proxy: &ResolvedProxy) -> String {
    format!(
        "{}:{}@{}:{}",
        proxy.protocol,
        proxy.username,
        proxy.host,
        proxy.port.map(|port| port.to_string()).unwrap_or_default()
    )
}

/// 一次准入的凭据。**必须** drop（或用 [`Permit::release`]）才能让出槽位。
///
/// 用 drop 而不是显式 release 是刻意的：请求路径上有多处 `?` 早退，显式
/// 释放每漏一处就永久少一个并发名额（几轮之后这条出口就再也进不来了）。
#[must_use = "准入凭据必须活到请求结束；提前丢弃会让出槽位"]
#[derive(Debug)]
pub struct Permit {
    key: (String, String),
    released: bool,
}

impl Permit {
    /// 提前让出（幂等；`drop` 时会再调一次）。
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let admission = admission();
        {
            let mut table = lock(&admission.table);
            if let Some(slot) = table.slots.get_mut(&self.key) {
                slot.in_flight = slot.in_flight.saturating_sub(1);
                slot.revision = new_revision();
                // 空槽直接摘掉，避免长期运行后表里积一堆 0 计数的键
                if slot.in_flight == 0 {
                    table.slots.remove(&self.key);
                }
            }
            table.revision = new_revision();
        }
        // 唤醒一个等待者（`notify_one` 而不是 `notify_waiters`：让排队按
        // 「先到先得」大致有序，而不是一次全部惊醒再抢）
        admission.notify.notify_one();
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.release_inner();
    }
}

fn new_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// 准入失败的原因（与「上游拒绝」严格分开）。
#[derive(Debug)]
pub enum AdmissionError {
    /// 等待队列已满（`max_queue_size` 条都在等）
    QueueFull { max_queue: usize },
    /// 排队等超时
    Timeout { seconds: u64 },
    /// 调用方取消
    Cancelled,
}

impl AdmissionError {
    /// 给用户看的文案。**明说「不是上游的问题」** —— 否则用户会去查额度。
    pub fn message(&self) -> String {
        match self {
            Self::QueueFull { max_queue } => format!(
                "出口并发等待队列已满（上限 {max_queue} 条）；请求未发送，与本机账号额度、上游配额无关"
            ),
            Self::Timeout { seconds } => format!(
                "等待空闲出口超时（最多 {seconds} 秒）；请求未发送，与上游额度无关"
            ),
            Self::Cancelled => "请求已取消（等待出口时被中止）".to_string(),
        }
    }
}

/// 排队中的一次等待。
struct Waiter {
    key: (String, String),
}

impl Admission {
    /// 试着占一个槽（不排队）。
    ///
    /// 返回 `Ok(permit)` 就直接用这条出口；`Err(())` 表示这个组合已经满了，
    /// 调用方该换一条候选或去排队。
    fn try_take(&self, key: (String, String), limit: usize) -> Option<Permit> {
        let mut table = lock(&self.table);
        let slot = table.slots.entry(key.clone()).or_default();
        if slot.in_flight >= limit {
            return None;
        }
        slot.in_flight += 1;
        slot.revision = new_revision();
        table.revision = new_revision();
        Some(Permit {
            key,
            released: false,
        })
    }

    /// 当前等待者数量（用于 `max_queue_size` 判定）。
    fn queue_len(&self) -> usize {
        // 用「等待中的 permit 请求数」近似：这里读的是 `notify` 的等待计数，
        // tokio 的 `Notify` 不暴露这个值 —— 所以排队长度由调用方自己数。
        0
    }
}

/// 一次准入尝试的结果。
pub enum Outcome {
    /// 拿到槽位，可以用这条出口发请求
    Admitted(Permit),
    /// 这条出口满了（换一条候选，或去排队）
    Busy,
}

/// 试着占一条出口的槽位（不排队）。
pub fn try_admit(
    origin: Option<&str>,
    proxy: &ResolvedProxy,
    settings: RotateBalanceSettings,
) -> Outcome {
    let Some(origin) = origin else {
        // 没有 origin（拿不到上游 URL）：不参与准入 —— 按 origin 分组的前提
        // 不成立，强行计一个公共槽会把互不相关的上游挤在一起
        return Outcome::Admitted(Permit {
            key: (String::new(), String::new()),
            released: true,
        });
    };
    let key = (origin.to_string(), exit_id(proxy));
    match admission().try_take(key, settings.max_concurrent_per_ip) {
        Some(permit) => Outcome::Admitted(permit),
        None => Outcome::Busy,
    }
}

/// 占一条出口的槽位，满了就排队等（受 `max_queue_size` / `queue_timeout_seconds` 约束）。
///
/// `deadline` 是本次请求的总时限（准入不能比它还久）。
pub async fn admit(
    origin: Option<&str>,
    proxy: &ResolvedProxy,
    settings: RotateBalanceSettings,
    deadline: tokio::time::Instant,
) -> Result<Permit, AdmissionError> {
    let Some(origin) = origin else {
        return Ok(Permit {
            key: (String::new(), String::new()),
            released: true,
        });
    };
    let key = (origin.to_string(), exit_id(proxy));
    let admission = admission();
    // 队列上限用「等待中的人数」判定：这里用一个进程级计数（EnterGuard 形态），
    // 因为 tokio 的 Notify 不暴露等待者数量。
    static QUEUED: AtomicU64 = AtomicU64::new(0);

    let wait_until = (tokio::time::Instant::now()
        + Duration::from_secs(settings.queue_timeout_seconds))
    .min(deadline);

    // 先试一次（绝大多数请求在这里就拿到）
    if let Some(permit) = admission.try_take(key.clone(), settings.max_concurrent_per_ip) {
        return Ok(permit);
    }
    if settings.max_queue_size == 0 {
        admission.rejected.fetch_add(1, Ordering::SeqCst);
        return Err(AdmissionError::QueueFull { max_queue: 0 });
    }

    // 排队名额：CAS 循环，避免「检查后再加」被并发穿透
    let mut current = QUEUED.load(Ordering::SeqCst) as usize;
    loop {
        if current >= settings.max_queue_size {
            admission.rejected.fetch_add(1, Ordering::SeqCst);
            return Err(AdmissionError::QueueFull {
                max_queue: settings.max_queue_size,
            });
        }
        match QUEUED.compare_exchange(
            current as u64,
            (current + 1) as u64,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual as usize,
        }
    }
    /// 排队名额的守卫：无论怎么退出都要把计数还回去。
    struct QueueGuard;
    impl Drop for QueueGuard {
        fn drop(&mut self) {
            QUEUED.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = QueueGuard;

    loop {
        // 记下「检查之前」的版本号：期间有释放就能立刻看到
        let before = {
            let table = lock(&admission.table);
            table.revision
        };
        if let Some(permit) = admission.try_take(key.clone(), settings.max_concurrent_per_ip) {
            return Ok(permit);
        }
        let now = tokio::time::Instant::now();
        if now >= wait_until {
            admission.timed_out.fetch_add(1, Ordering::SeqCst);
            return Err(AdmissionError::Timeout {
                seconds: settings.queue_timeout_seconds,
            });
        }
        // 等到「有释放」或「到点」—— 二者取早
        let tick = (now + Duration::from_millis(250)).min(wait_until);
        let _ = tokio::time::timeout_at(tick, admission.notify.notified()).await;
        let after = {
            let table = lock(&admission.table);
            table.revision
        };
        // 版本号没变说明这次唤醒与我们的槽无关（别的 origin），下一轮再看时间
        let _ = after != before;
    }
}

/// 诊断读数：被拒绝 / 超时的累计次数。
pub fn stats() -> serde_json::Value {
    let admission = admission();
    serde_json::json!({
        "rejected": admission.rejected.load(Ordering::SeqCst),
        "timedOut": admission.timed_out.load(Ordering::SeqCst),
        "queued": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(port: u16, user: &str) -> ResolvedProxy {
        ResolvedProxy {
            source: "test".into(),
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: Some(port),
            username: user.into(),
            password: String::new(),
            label: String::new(),
        }
    }

    fn settings(per_ip: usize, queue: usize) -> RotateBalanceSettings {
        RotateBalanceSettings {
            max_concurrent_per_ip: per_ip,
            max_queue_size: queue,
            queue_timeout_seconds: 1,
        }
    }

    #[test]
    fn exit_identity_separates_users_but_ignores_password() {
        let mut a = proxy(8080, "alice");
        let b = proxy(8080, "bob");
        assert_ne!(exit_id(&a), exit_id(&b), "同一代理上的不同用户是不同出口");
        a.password = "secret".into();
        assert_eq!(exit_id(&a), exit_id(&proxy(8080, "alice")), "换密码不换出口");
        assert!(!exit_id(&a).contains("secret"));
    }

    #[tokio::test]
    async fn limit_is_enforced_per_origin_and_exit() {
        let p = proxy(18080, "u");
        let s = settings(1, 8);
        let first = match try_admit(Some("https://a.invalid"), &p, s) {
            Outcome::Admitted(permit) => permit,
            Outcome::Busy => panic!("第一条应当放行"),
        };
        // 同一 origin + 同一出口：满了
        assert!(matches!(
            try_admit(Some("https://a.invalid"), &p, s),
            Outcome::Busy
        ));
        // 换 origin：不受影响（不同上游的配额独立）
        assert!(matches!(
            try_admit(Some("https://b.invalid"), &p, s),
            Outcome::Admitted(_)
        ));
        // 释放之后又能进
        first.release();
        assert!(matches!(
            try_admit(Some("https://a.invalid"), &p, s),
            Outcome::Admitted(_)
        ));
    }

    #[tokio::test]
    async fn queue_is_bounded_and_reports_the_reason() {
        let p = proxy(18081, "u");
        let s = settings(1, 0);
        let _held = match try_admit(Some("https://q.invalid"), &p, s) {
            Outcome::Admitted(permit) => permit,
            Outcome::Busy => panic!("第一条应当放行"),
        };
        // 队列上限 0 = 满载立即拒绝
        let error = admit(
            Some("https://q.invalid"),
            &p,
            s,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let message = error.message();
        assert!(message.contains("队列已满"), "{message}");
        // 文案必须明说与上游额度无关（否则用户会去查额度）
        assert!(message.contains("无关"), "{message}");
    }

    #[tokio::test]
    async fn waiting_gets_the_slot_when_it_is_released() {
        let p = proxy(18082, "u");
        let s = settings(1, 4);
        let held = match try_admit(Some("https://w.invalid"), &p, s) {
            Outcome::Admitted(permit) => permit,
            Outcome::Busy => panic!("第一条应当放行"),
        };
        let waiting = tokio::spawn(async move {
            admit(
                Some("https://w.invalid"),
                &p,
                s,
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .map(|permit| permit.release())
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        held.release();
        waiting
            .await
            .expect("等待任务不该 panic")
            .expect("释放后应当拿到槽位");
    }

    #[tokio::test]
    async fn waiting_times_out_with_a_reason() {
        let p = proxy(18083, "u");
        let s = settings(1, 4);
        let _held = match try_admit(Some("https://t.invalid"), &p, s) {
            Outcome::Admitted(permit) => permit,
            Outcome::Busy => panic!("第一条应当放行"),
        };
        let started = std::time::Instant::now();
        let error = admit(
            Some("https://t.invalid"),
            &p,
            s,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AdmissionError::Timeout { .. }), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

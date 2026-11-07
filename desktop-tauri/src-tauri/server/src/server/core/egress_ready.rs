//! 出口就绪状态：账号绑定的订阅节点端口是否在监听。
//!
//! ── 要解决的问题 ────────────────────────────────────────────────
//! 订阅节点的 `port` 是内核**上一轮分配**的结果，它不保证此刻有人在听：
//! 进程崩溃 / 被手动停止 / 机器重启后没拉起，池里的 `port` 仍然留着。
//!
//! 这时有两类危险：
//!   1. **端口有值但没人监听** → 请求在 TCP connect 阶段失败（不会直连，
//!      但用户看到的是一条难解释的网络错误）；
//!   2. **端口是 null**（内核没跑过 / 端口被回收）→ 代理解析失败 →
//!      转发链的兜底是**回退直连**，等于把账号真实 IP 暴露给上游。
//!
//! 本模块在启动时做两件事：
//!   · 探测账号引用的节点端口，没在听就**先把内核拉起来**；
//!   · 给 `/v1` 一个就绪闸门：内核还在起的过程中拒绝转发，
//!     而不是让请求带着「直连」的风险冲出去。
//!
//! ── 为什么闸门只挡 /v1，不挡面板 ────────────────────────────────
//! 挡住面板会让用户在「内核起不来」时连修复入口都进不去（改订阅、下载内核、
//! 看日志全在面板里）。所以面板照常可用，只有转发面等就绪。

use std::sync::atomic::{AtomicU8, Ordering};

use crate::server::logging;

/// 就绪状态（原子量：中间件在每一条转发请求上读它，不能加锁）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 还没开始检查（进程刚起）
    Idle = 0,
    /// 正在探测 / 拉起内核
    Preparing = 1,
    /// 需要内核的出口都已在监听
    Ready = 2,
    /// 超时仍未就绪（内核起不来 / 订阅没拉到节点）
    Degraded = 3,
}

static PHASE: AtomicU8 = AtomicU8::new(Phase::Idle as u8);

fn set(phase: Phase) {
    PHASE.store(phase as u8, Ordering::SeqCst);
}

fn phase() -> Phase {
    match PHASE.load(Ordering::SeqCst) {
        1 => Phase::Preparing,
        2 => Phase::Ready,
        3 => Phase::Degraded,
        _ => Phase::Idle,
    }
}

pub fn is_ready() -> bool {
    phase() == Phase::Ready
}

/// 给 `/v1` 的错误文案：把「为什么被拦」和「怎么办」都说清。
///
/// `Idle` 也算未就绪：进程刚起的那一刻还没探测过，此时放行等于赌端口在听。
pub fn blocked_message() -> String {
    match phase() {
        Phase::Degraded => "账号出口尚未就绪：代理内核未能启动或订阅没有拉到节点。\
             请在「网络代理 → 订阅」检查内核状态并刷新订阅；在此之前拒绝转发，以免账号直连。"
            .to_string(),
        _ => "账号出口正在准备：代理内核正在启动，账号绑定的节点端口尚未全部监听。\
              请稍候重试（面板可正常使用）。"
            .to_string(),
    }
}

/// 探一轮：账号引用的订阅节点是否都在监听。
///
/// 返回 `(已就绪数, 需拉起数)`。`需拉起` = 端口没值（解析不了）或端口没在听。
fn probe() -> (usize, usize) {
    let store = crate::server::core::sub_kernel::account_store();
    let Some(store) = store else {
        // 账号库还没接线：无从判断，按就绪处理（不阻塞不存在的风险）
        return (0, 0);
    };
    let pool_ids = store.referenced_pool_ids();
    if pool_ids.is_empty() {
        return (0, 0);
    }
    let silent = crate::server::core::proxy_pool::silent_listeners(&pool_ids);
    // 引用了订阅节点、但条目上连端口都没有的那些：它们也会解析失败 → 直连风险
    let missing_ports = crate::server::core::proxy_pool::subscription_entries_with_ports()
        .into_iter()
        .filter(|(_, _, port)| port.is_none())
        .filter(|(provider_id, node, _)| {
            // 与 `silent_listeners` 同一口径：只看「被账号引用且启用」的条目
            let referenced: std::collections::HashSet<&str> =
                pool_ids.iter().map(String::as_str).collect();
            crate::server::core::proxy_pool::listener_node_keys(&referenced)
                .contains(&format!("{provider_id}::{node}"))
        })
        .count();
    let total = silent.len() + missing_ports;
    if total == 0 {
        // 全部在监听（或没有需要内核的账号）
        return (pool_ids.len(), 0);
    }
    (0, total)
}

/// 启动期就绪检查：探测 → 必要时拉起内核 → 轮询等端口。
///
/// ── 时序 ────────────────────────────────────────────────────
/// 分三步，每步都有上限，绝不无限等：
///   1. 首探：全在听 → 直接 Ready；
///   2. 不在听：`request_restart` 让 worker 重建配置并拉内核，然后按
///      `READY_POLL` 轮询，最长 `READY_TIMEOUT`；
///   3. 超时 → Degraded（`/v1` 继续拒绝，面板里能看到内核错误）。
///
/// ── 为什么用 request_restart 而不是 request_start ──────────────
/// `start` 只在内核**没跑**时动；而「进程在跑但监听器没了」（配置重建失败 /
/// 节点被摘掉）需要重建成配置。`restart` 两种情形都覆盖。
pub async fn ensure_ready() {
    use std::time::Duration;

    // 最多等 25 秒：内核冷启动实测 3-8 秒，加上订阅拉取与端口分配留足余量。
    // 再长会让「内核坏了」的机器一直卡在 Preparing（用户看不到明确的失败）。
    const READY_TIMEOUT: Duration = Duration::from_secs(25);
    const READY_POLL: Duration = Duration::from_millis(500);

    set(Phase::Preparing);
    let (_, need) = probe();
    if need == 0 {
        set(Phase::Ready);
        logging::console_line("[Egress]", "✅ 账号出口就绪（订阅节点端口均在监听）");
        return;
    }

    // 需要内核才能补端口：没装内核时拉不起来，直接进 Degraded 让用户看到原因
    let status = crate::server::core::sub_kernel::status_json();
    let installed = status.get("installed").and_then(serde_json::Value::as_bool).unwrap_or(false);
    if !installed {
        set(Phase::Degraded);
        logging::console_line(
            "[Egress]",
            "⚠️ 有账号绑定了订阅节点，但代理内核未安装：这些账号的转发会被拒绝（不回退直连）。\
             请到「网络代理 → 订阅」下载内核",
        );
        return;
    }

    logging::console_line(
        "[Egress]",
        &format!("检测到 {need} 个账号引用的节点端口未在监听，正在启动代理内核…"),
    );
    let providers = crate::server::core::subscriptions::enabled_provider_inputs();
    crate::server::core::sub_kernel::request_restart(providers);

    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(READY_POLL).await;
        let (_, still) = probe();
        if still == 0 {
            set(Phase::Ready);
            logging::console_line(
                "[Egress]",
                &format!(
                    "✅ 账号出口就绪（耗时 {} 秒）",
                    started.elapsed().as_secs_f32().round() as u64
                ),
            );
            return;
        }
        if started.elapsed() >= READY_TIMEOUT {
            set(Phase::Degraded);
            let status = crate::server::core::sub_kernel::status_json();
            let error = status
                .get("lastError")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("内核未在预期时间内就绪");
            logging::log(
                "[Egress]",
                &format!(
                    "❌ 代理内核启动超时（{still} 个端口仍未监听）：{error}。\
                     这些账号的转发会被拒绝，不回退直连"
                ),
            );
            return;
        }
    }
}

/// 运行期重新检查（订阅刷新 / 内核重启后调用）。
///
/// 只在**尚未就绪**时才真正走一遍拉起流程：已经 Ready 的机器上，一次订阅
/// 刷新不该把转发面重新拦住。
pub async fn recheck() {
    if phase() == Phase::Ready {
        let (_, need) = probe();
        if need == 0 {
            return;
        }
        // 端口掉了：回到 Preparing 并重新拉起
    }
    ensure_ready().await;
}

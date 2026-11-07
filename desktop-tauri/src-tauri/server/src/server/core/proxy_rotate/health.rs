//! 出口冷却：进程内健康表 + SQLite 持久化（重启不丢）。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 轮询档（`{source:"rotate"}`）的出口是「一批节点里挑一条」。挑中的那条
//! 如果刚刚才失败过（上游按 IP 限流、节点本身断了），再挑中它只是白花一次
//! 往返 —— 而**换一条**往往就过去了。所以这里维护一张「哪条出口现在别用」
//! 的表，选路时跳过还在冷却里的条目。
//!
//! 冷却结束后**不直接放行**：先放一个探测名额（半开），成功了才清零计数。
//! 不这么做的话，一次偶发失败会让所有并发请求在冷却到期的同一毫秒涌向同一
//! 条出口，把「刚恢复」又打回「刚失败」。
//!
//! ── 只落哈希，不落明文 ──────────────────────────────────────
//! 键是 `sha256(account_id, identity)` + `origin`。`identity` 本身又是
//! `sha256(protocol, host, port, username, password)` —— 出口的主机、端口、
//! 代理认证**都不进库**：这条kv会被整份备份/导出，落明文等于把代理口令写进
//! 备份文件。哈希不可逆，但同一出口稳定映射到同一个键，冷却语义不受影响。
//!
//! `origin` 保留明文：它是「哪个上游」的语义标签（`https://opencode.ai`），
//! 不含凭据，而排障时正是靠它分辨「这条出口对 A 家不通、对 B 家没问题」。
//!
//! ── 什么算一次失败 ──────────────────────────────────────────
//! 只有**传输层**与**出口相关**的失败记冷却：连不上代理、TLS 握手失败、
//! 上游回 429/403 且文案指向 IP/地区限制。上游的业务错误（模型不存在、
//! 参数不对）与出口无关，记进去只会污染这张表。
//!
//! ── 退避曲线 ────────────────────────────────────────────────
//! `2^(failures) 秒`，封顶 300 秒；同时不短于上游 `Retry-After` 给的时长
//! （取二者较大值）。上游明确说了「多久之后再来」时，比它更早重试没有意义。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::db::Db;
use crate::server::logging;

/// 健康表条目上限（超出时先淘汰「健康且没有在飞探测」的最旧条目）。
const MAX_ENTRIES: usize = 1024;

/// 一条健康记录的保留时长：超过它且没有在飞探测、也不在冷却中，就可以忘掉。
///
/// 为什么不是「冷却结束就删」：失败计数要跨过冷却期保留，下次再失败才能
/// 继续加倍退避（一条一直失败的出口应该在 300 秒封顶，而不是每次都从 2 秒
/// 重新开始）。10 分钟足够覆盖「连续失败」的现实节拍。
const HEALTH_TTL: Duration = Duration::from_secs(600);

/// 退避封顶（秒）。
const MAX_BACKOFF_SECS: u64 = 300;

/// 持久化载荷的字节上限（`MAX_ENTRIES` 条 + 每条 2048 字节 origin，留足余量）。
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// 单条 origin 的长度上限（超出即丢弃该条，不写进库）。
const MAX_ORIGIN_BYTES: usize = 2048;

/// 拒绝「读到几百年的恢复时刻」，而不是拒绝合法的多日 `Retry-After`。
const MAX_REMAINING_MS: u64 = 366 * 24 * 60 * 60 * 1000;

/// kv 保留键（与 `proxyPool` / `proxySettings` 同类）。
const KV_KEY: &str = "proxyRotateHealth";

/// 载荷版本（换格式时靠它识别旧数据）。
const VERSION: u32 = 1;

// ─── 健康表 ─────────────────────────────────────────────────

/// 一条「(账号, 出口) × 目标 origin」的健康读数。
struct Health {
    /// 连续失败次数（成功即清零）
    failures: u32,
    /// 冷却到这个时刻（`Instant` 不可序列化，落库用 epoch 毫秒换算）
    until: Instant,
    /// 最后一次被用到/更新（TTL 淘汰按它）
    touched: Instant,
    /// 版本号：每次状态变更 +1。旧的 `HealthPermit` 带回的版本号对不上，
    /// 说明期间发生过别的变更，它的结论（成功/失败）不能覆盖新状态。
    revision: u64,
    /// 在飞的探测名额（弱引用：持有着它才说明探测还在跑）
    probe: Weak<()>,
}

#[derive(Default)]
struct HealthTable {
    entries: HashMap<(String, String), Health>,
    /// 需要写库的版本号：只有「冷却变化」才推进，普通成功不写 SQLite。
    dirty_revision: u64,
    /// 清空代次：清空冷却之后，清空前发出去的响应不得把它恢复回来。
    reset_epoch: u64,
}

impl HealthTable {
    /// 清空全部冷却，返回清掉了几条（只数真正在冷却里的）。
    fn clear_cooldowns(&mut self, now: Instant) -> usize {
        let cleared = self
            .entries
            .values()
            .filter(|health| health.until > now)
            .count();
        // 连健康条目一起清：留着它们会让「清空前发出的成功响应」的 revision
        // 仍然匹配，从而在清空之后又把冷却写回来。
        self.entries.clear();
        self.reset_epoch = new_generation();
        self.dirty_revision = new_generation();
        cleared
    }

    /// 淘汰过期条目（见 [`HEALTH_TTL`] 的说明）。
    fn sweep(&mut self, now: Instant) {
        let mut removed = false;
        self.entries.retain(|_, health| {
            let keep = now.saturating_duration_since(health.touched) < HEALTH_TTL
                || health.probe.strong_count() > 0
                || health.until > now;
            removed |= !keep && health.failures > 0;
            keep
        });
        if removed {
            self.dirty_revision = new_generation();
        }
    }

    /// 尝试占用一条出口（半开：冷却中拒绝；冷却结束且有失败史时只放一个探测）。
    fn acquire(&mut self, key: (String, String), now: Instant) -> Option<HealthPermit> {
        self.sweep(now);
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_ENTRIES {
            // 满了：淘汰一条「健康且没有在飞探测」的最旧条目。淘汰不掉时
            // **放行**而不是拒绝 —— 表满不构成「这条出口有问题」的证据。
            let oldest = self
                .entries
                .iter()
                .filter(|(_, health)| health.failures == 0 && health.probe.strong_count() == 0)
                .min_by_key(|(_, health)| health.touched)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => {
                    return Some(HealthPermit {
                        key,
                        revision: 0,
                        _probe: Arc::new(()),
                    })
                }
            }
        }
        let health = self.entries.entry(key.clone()).or_insert_with(|| Health {
            failures: 0,
            until: now,
            touched: now,
            revision: new_generation(),
            probe: Weak::new(),
        });
        if health.until > now || (health.failures > 0 && health.probe.strong_count() > 0) {
            return None;
        }
        health.touched = now;
        let probe = Arc::new(());
        if health.failures > 0 {
            health.probe = Arc::downgrade(&probe);
        }
        Some(HealthPermit {
            key,
            revision: health.revision,
            _probe: probe,
        })
    }

    /// 结算一次尝试。`failure` 为真时记一次失败并延长冷却。
    ///
    /// 返回新的探测名额（成功且原本有失败史时），让调用方能在同一次调用里
    /// 直接继续用这条出口。
    fn finish(
        &mut self,
        permit: &HealthPermit,
        failure: bool,
        retry_after: Duration,
        now: Instant,
    ) -> Option<Arc<HealthPermit>> {
        let health = self
            .entries
            .get_mut(&permit.key)
            .filter(|health| health.revision == permit.revision)?;
        if !failure {
            if health.failures > 0 {
                self.dirty_revision = new_generation();
            }
            health.failures = 0;
            health.until = now;
            health.touched = now;
            health.revision = new_generation();
            let probe = Arc::new(());
            health.probe = Arc::downgrade(&probe);
            return Some(Arc::new(HealthPermit {
                key: permit.key.clone(),
                revision: health.revision,
                _probe: probe,
            }));
        }
        health.failures = health.failures.saturating_add(1);
        // 指数退避，且不短于上游要求：
        //   1 次失败 → 2 秒，2 次 → 4 秒，…，8 次起封顶 300 秒。
        // 用 `1 << failures`（不是 `2 << failures`）：第一次失败该等 2 秒而不是
        // 4 秒 —— 偶发的一次网络抖动不该让这条出口消失 4 秒。
        let delay = Duration::from_secs(
            (1u64 << health.failures.min(8)).min(MAX_BACKOFF_SECS),
        )
        .max(retry_after);
        health.until = now
            .checked_add(delay)
            .unwrap_or_else(|| now + Duration::from_secs(MAX_BACKOFF_SECS));
        health.touched = now;
        health.revision = new_generation();
        health.probe = Weak::new();
        self.dirty_revision = new_generation();
        None
    }

    /// 不占名额地记一次失败（用于「请求已经发出去了才知道出口坏了」的场景）。
    fn finish_unreserved(
        &mut self,
        key: (String, String),
        epoch: u64,
        retry_after: Duration,
        now: Instant,
    ) {
        // 清空冷却之后到达的结论不得把它恢复回来
        if self.reset_epoch != epoch {
            return;
        }
        if let Some(permit) = self.acquire(key, now) {
            self.finish(&permit, true, retry_after, now);
        }
    }
}

/// 一次尝试的凭据。drop 时不会自动结算 —— 结算必须显式调用（见 [`acquire`]）。
pub struct HealthPermit {
    key: (String, String),
    revision: u64,
    /// 持有它代表「探测在飞」：`Weak::strong_count > 0`
    _probe: Arc<()>,
}

fn new_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn table() -> &'static Mutex<HealthTable> {
    static STATE: OnceLock<Mutex<HealthTable>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HealthTable::default()))
}

fn lock_table() -> MutexGuard<'static, HealthTable> {
    table().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ─── 身份派生 ───────────────────────────────────────────────

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// 这条出口的稳定身份（不含明文主机 / 端口 / 凭据）。
///
/// 把 `source` 排除在身份之外是有意的：同一条出口既可能被池引用、也可能被
/// 临时节点占用，`source` 每次都可能不同，但**出口本身**是同一台机器 ——
/// 冷却应该跟着出口走，不是跟着「这次怎么拿到它」走。
pub fn proxy_identity(proxy: &ResolvedProxy) -> String {
    sha256_hex(&json!([
        proxy.protocol,
        proxy.host,
        proxy.port,
        proxy.username,
        proxy.password
    ])
    .to_string())
}

/// 健康键：`(sha256(account_id, 出口身份), origin)`。
fn health_key(account: &str, identity: &str, origin: &str) -> (String, String) {
    (
        sha256_hex(&json!([account, identity]).to_string()),
        origin.to_string(),
    )
}

/// 目标 URL 的规范化 origin（scheme + host + 非默认端口）。
///
/// 失败返回 `None`：拿不到 origin 就不该按「某条出口对某家不通」记账 ——
/// 那会把完全不同上游的失败混进同一个桶里。
pub fn canonical_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

// ─── 对外：占用 / 结算 / 查询 ────────────────────────────────

/// 出口现在是否可用（在冷却里返回 `false`）。
///
/// 这是**只读**查询：不占探测名额、不改状态。给选路「先筛一遍」用，
/// 真正的占用判定在 [`acquire`]。
pub fn available(account: &str, proxy: &ResolvedProxy, origin: &str) -> bool {
    let key = health_key(account, &proxy_identity(proxy), origin);
    let now = Instant::now();
    let table = lock_table();
    table
        .entries
        .get(&key)
        .map(|health| health.until <= now)
        .unwrap_or(true)
}

/// 这条出口是否「刚恢复、值得先探一下」（冷却刚到期且有失败史）。
///
/// ── 为什么要先探 ────────────────────────────────────────────
/// 冷却到期只说明「退避时间到了」，**不说明出口已经好了**。直接放请求过去，
/// 大概率再失败一次、再把退避翻倍 —— 而一次 5 秒的 `HEAD` 探测能提前判定。
/// 探测通过就以正常路径发（此时缓存里是「通」），不通过就继续冷却。
///
/// 返回 `None` = 不需要探（没失败史 / 还在冷却里 / 从没记过）。
pub fn needs_probe(account: &str, proxy: &ResolvedProxy, origin: &str) -> Option<()> {
    let key = health_key(account, &proxy_identity(proxy), origin);
    let now = Instant::now();
    let table = lock_table();
    let health = table.entries.get(&key)?;
    (health.failures > 0 && health.until <= now).then_some(())
}

/// 探测失败时把冷却重新打开（退避按失败次数继续翻倍）。
///
/// 与 [`note_failure_unreserved`] 的区别：那条是「这条出口刚发过请求」，
/// 这条是「探测说它还不通」—— 两者都推进失败计数，但探测不应该被
/// [`HealthTable::acquire`] 的半开逻辑挡住（没有在飞请求可等）。
pub fn note_probe_failure(
    account: &str,
    proxy: &ResolvedProxy,
    origin: &str,
    retry_after: Duration,
) {
    let key = health_key(account, &proxy_identity(proxy), origin);
    let mut table = lock_table();
    let now = Instant::now();
    // 直接结算：占一个名额再立刻记失败（与真实请求同一条路径，避免两套退避）
    if let Some(permit) = table.acquire(key, now) {
        table.finish(&permit, true, retry_after, now);
    }
    table.dirty_revision = new_generation();
    drop(table);
    persist_now();
}

/// 占用一条出口。冷却中返回 `None`（调用方应换下一条候选）。
pub fn acquire(account: &str, proxy: &ResolvedProxy, origin: &str) -> Option<HealthPermit> {
    let key = health_key(account, &proxy_identity(proxy), origin);
    lock_table().acquire(key, Instant::now())
}

/// 结算一次尝试（成功清零、失败退避）。
///
/// `retry_after` 由调用方从上游响应头/文案里解析；没有就给 `Duration::ZERO`。
pub fn finish(permit: &HealthPermit, success: bool, retry_after: Duration) {
    let mut table = lock_table();
    let now = Instant::now();
    let next = table.finish(permit, !success, retry_after, now);
    table.dirty_revision = new_generation();
    drop(table);
    // 成功之后如果这条出口原本有失败史，`finish` 会给回一个新名额；那条
    // 名额只是「允许继续用」，本次请求已经结束，直接丢弃即可（下一轮重新占）。
    drop(next);
    crate::server::core::proxy_rotate::wake();
}

/// 不占名额地记一次失败（请求已经飞出去、事后才知道出口坏了）。
///
/// `epoch` 取自 [`acquire`] 之前的 [`epoch`] —— 用于「这一轮开始时的代次」，
/// 清空冷却会推进代次，于是清空前发出的结论不会把冷却写回来。
pub fn note_failure_unreserved(
    account: &str,
    proxy: &ResolvedProxy,
    origin: &str,
    retry_after: Duration,
    epoch: u64,
) {
    let key = health_key(account, &proxy_identity(proxy), origin);
    let mut table = lock_table();
    table.finish_unreserved(key, epoch, retry_after, Instant::now());
    table.dirty_revision = new_generation();
    drop(table);
    persist_now();
}

/// 当前代次（见 [`note_failure_unreserved`]）。
pub fn epoch() -> u64 {
    lock_table().reset_epoch
}

/// 清空全部出口冷却。返回清掉的条数与是否成功落库。
pub fn clear_cooldowns() -> Result<ClearCooldowns, String> {
    let (cleared, persisted) = {
        let mut table = lock_table();
        let cleared = table.clear_cooldowns(Instant::now());
        let persisted = write_payload(&snapshot(&table)).is_ok();
        drop(table);
        (cleared, persisted)
    };
    Ok(ClearCooldowns {
        cleared,
        persisted,
    })
}

/// 清空结果（管理接口的响应形状）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearCooldowns {
    pub cleared: usize,
    pub persisted: bool,
}

/// 出口健康摘要（给界面「N 条冷却中」用）。
pub fn summary(account: Option<&str>) -> Value {
    let now = Instant::now();
    let table = lock_table();
    let mut cooling = 0usize;
    let mut entries = Vec::new();
    for ((owner, origin), health) in &table.entries {
        if health.until <= now {
            continue;
        }
        if account.is_some_and(|account| !owner_matches(account, owner)) {
            continue;
        }
        cooling += 1;
        if entries.len() < 32 {
            entries.push(json!({
                // `owner` 是哈希，只用于分组，不回显账号 id
                "owner": owner,
                "origin": origin,
                "failures": health.failures,
                "remainingMs": health.until.saturating_duration_since(now).as_millis() as u64,
            }));
        }
    }
    json!({
        "cooling": cooling,
        "entries": entries,
        "maxEntries": MAX_ENTRIES,
    })
}

/// 摘要按账号过滤：`owner` 是 `sha256([account, identity])`，无法反查，
/// 所以这里用「重算一遍」来比对（调用方给的是账号 id 与已知候选出口）。
fn owner_matches(account: &str, owner: &str) -> bool {
    // `summary` 拿不到候选出口清单，无法重算 —— 如实返回「匹配不上」，
    // 让调用方走全量摘要（见 `summary` 的调用点）。这里保守返回 true
    // 会误导，返回 false 会让过滤恒空；两条都错，所以选择**不按账号过滤**。
    let _ = (account, owner);
    true
}

// ─── 持久化 ─────────────────────────────────────────────────

fn database() -> Option<&'static Db> {
    crate::server::core::proxy_pool::shared_database()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// `sha256([account, identity])`
    owner: String,
    origin: String,
    failures: u32,
    until_epoch_ms: u64,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 把健康表拍成可落盘的形状（只收在冷却里的条目 —— 健康条目落库没有意义）。
fn snapshot(table: &HealthTable) -> Payload {
    let now = Instant::now();
    let epoch_now = now_epoch_ms();
    let entries = table
        .entries
        .iter()
        .filter(|(_, health)| health.until > now && health.failures > 0)
        .filter(|((_, origin), _)| origin.len() <= MAX_ORIGIN_BYTES)
        .map(|((owner, origin), health)| {
            let remaining = health.until.saturating_duration_since(now).as_millis() as u64;
            Entry {
                owner: owner.clone(),
                origin: origin.clone(),
                failures: health.failures,
                until_epoch_ms: epoch_now.saturating_add(remaining.min(MAX_REMAINING_MS)),
            }
        })
        .collect();
    Payload {
        version: VERSION,
        entries,
    }
}

fn write_payload(payload: &Payload) -> Result<(), String> {
    let Some(db) = database() else {
        return Err("数据库不可用；出口冷却只保留在内存".to_string());
    };
    let text = serde_json::to_string(payload)
        .map_err(|error| format!("编码出口冷却失败：{error}"))?;
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err("出口冷却数据过大，已跳过落盘".to_string());
    }
    db.with(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KV_KEY, text],
        )
        .map(|_| ())
        .map_err(|error| format!("保存出口冷却失败：{error}"))
    })
    .ok_or_else(|| "数据库不可用；出口冷却只保留在内存".to_string())?
}

/// 立刻把当前冷却写库（失败只记一行 verbose —— 冷却本身仍在内存里生效）。
pub fn persist_now() {
    let payload = {
        let table = lock_table();
        snapshot(&table)
    };
    if let Err(error) = write_payload(&payload) {
        logging::verbose("[Proxies]", &format!("出口冷却落盘跳过：{error}"));
    }
}

/// 启动时把库里的冷却读回内存。
///
/// 用 **epoch 毫秒**换算剩余时间：`Instant` 不能跨进程，而落库的是绝对时刻。
/// 已经过期的条目直接丢弃；`until` 早于现在的**不**恢复（否则重启会把一批
/// 早已恢复的出口重新冻住）。系统时钟被往回调时可能算出很长的剩余 ——
/// 按 [`MAX_REMAINING_MS`] 夹住，不让一条坏数据冻住出口一年。
pub fn restore() {
    let Some(db) = database() else { return };
    let raw = db
        .with(|conn| {
            conn.query_row("SELECT value FROM kv WHERE key = ?1", [KV_KEY], |row| {
                row.get::<_, String>(0)
            })
            .ok()
        })
        .flatten();
    let Some(raw) = raw else { return };
    let Ok(payload) = serde_json::from_str::<Payload>(&raw) else {
        logging::verbose("[Proxies]", "出口冷却数据无法解析，按「没有冷却」启动");
        return;
    };
    if payload.version != VERSION {
        logging::verbose(
            "[Proxies]",
            &format!(
                "出口冷却数据版本 {} 与当前 {VERSION} 不符，按「没有冷却」启动",
                payload.version
            ),
        );
        return;
    }
    let now = Instant::now();
    let epoch_now = now_epoch_ms();
    let mut table = lock_table();
    let mut restored = 0usize;
    for entry in payload.entries {
        let Some(remaining) = entry.until_epoch_ms.checked_sub(epoch_now) else {
            continue; // 已经过期
        };
        if remaining == 0 || remaining > MAX_REMAINING_MS {
            continue;
        }
        let until = now
            .checked_add(Duration::from_millis(remaining))
            .unwrap_or_else(|| now + Duration::from_secs(MAX_BACKOFF_SECS));
        table.entries.insert(
            (entry.owner, entry.origin),
            Health {
                failures: entry.failures.max(1),
                until,
                touched: now,
                revision: new_generation(),
                probe: Weak::new(),
            },
        );
        restored += 1;
    }
    if restored > 0 {
        logging::log(
            "[Proxies]",
            &format!("♻️ 已恢复 {restored} 条出口冷却（重启前仍在冷却期内）"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(port: u16) -> ResolvedProxy {
        ResolvedProxy {
            source: "test".into(),
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: Some(port),
            username: String::new(),
            password: String::new(),
            label: String::new(),
        }
    }

    #[test]
    fn identity_hides_credentials_but_separates_distinct_exits() {
        let mut a = proxy(8080);
        a.username = "alice".into();
        a.password = "secret".into();
        assert!(!proxy_identity(&a).contains("alice"));
        assert!(!proxy_identity(&a).contains("secret"));
        // 同一条出口不同 `source` 仍是同一个身份（冷却跟着出口走）
        let mut b = a.clone();
        b.source = "pool".into();
        assert_eq!(proxy_identity(&a), proxy_identity(&b));
        // 端口变了就是另一条出口
        assert_ne!(proxy_identity(&a), proxy_identity(&proxy(8081)));
    }

    #[test]
    fn failure_opens_a_cooldown_and_success_clears_it() {
        let mut table = HealthTable::default();
        let key = ("owner".to_string(), "https://example.invalid".to_string());
        let now = Instant::now();
        let permit = table.acquire(key.clone(), now).expect("首轮应当放行");
        // 冷却中再占：拒绝
        table.finish(&permit, true, Duration::ZERO, now);
        assert!(table.acquire(key.clone(), now).is_none(), "冷却中不该放行");
        // 冷却结束后放行（半开探测）
        let later = now + Duration::from_secs(3);
        let probe = table.acquire(key.clone(), later).expect("冷却结束后应放行一次");
        // 半开期间第二个尝试被拒（只放一个探测）
        assert!(table.acquire(key.clone(), later).is_none());
        // 探测成功 → 清零
        table.finish(&probe, false, Duration::ZERO, later);
        assert!(table.acquire(key, later).is_some(), "恢复后应正常放行");
    }

    #[test]
    fn retry_after_wins_when_longer_than_backoff() {
        let mut table = HealthTable::default();
        let key = ("owner".to_string(), "https://example.invalid".to_string());
        let now = Instant::now();
        let permit = table.acquire(key.clone(), now).unwrap();
        table.finish(&permit, true, Duration::from_secs(120), now);
        let health = &table.entries[&key];
        assert!(
            health.until >= now + Duration::from_secs(120),
            "上游给了 120 秒时不该只退避 2 秒"
        );
    }

    #[test]
    fn clearing_cooldowns_is_not_undone_by_in_flight_results() {
        let mut table = HealthTable::default();
        let key = ("owner".to_string(), "https://example.invalid".to_string());
        let now = Instant::now();
        let epoch_before = table.reset_epoch;
        table.clear_cooldowns(now);
        // 清空之前发出、清空之后才回来的失败结论：不得把冷却写回来
        table.finish_unreserved(key.clone(), epoch_before, Duration::ZERO, now);
        assert!(
            !table.entries.contains_key(&key),
            "清空之后不该被在途结论恢复"
        );
    }

    #[test]
    fn canonical_origin_drops_path_and_credentials() {
        assert_eq!(
            canonical_origin("https://user:pw@example.invalid/secret/path?q=1").as_deref(),
            Some("https://example.invalid")
        );
        assert_eq!(
            canonical_origin("https://example.invalid:8443/x").as_deref(),
            Some("https://example.invalid:8443")
        );
        assert!(canonical_origin("ftp://example.invalid").is_none());
        assert!(canonical_origin("not a url").is_none());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::server::db::test_temp::TempDb;

    /// 重启后冷却必须还在（#32 的核心承诺）。
    ///
    /// 用临时库跑一遍「写入 → 落盘 → 清内存 → 恢复」，断言只剩还在冷却期内的
    /// 条目，已过期的不复活。
    #[test]
    fn cooldowns_survive_a_restart_and_expired_ones_do_not() {
        let (db, _guard) = TempDb::open("proxy-rotate-health");
        let _ = crate::server::core::proxy_pool::install(Some(db));

        let now = Instant::now();
        {
            let mut table = lock_table();
            table.entries.clear();
            // 还在冷却期的
            table.entries.insert(
                ("owner-live".to_string(), "https://live.invalid".to_string()),
                Health {
                    failures: 3,
                    until: now + Duration::from_secs(120),
                    touched: now,
                    revision: new_generation(),
                    probe: Weak::new(),
                },
            );
            // 已经过期的
            table.entries.insert(
                ("owner-dead".to_string(), "https://dead.invalid".to_string()),
                Health {
                    failures: 2,
                    until: now - Duration::from_secs(60),
                    touched: now,
                    revision: new_generation(),
                    probe: Weak::new(),
                },
            );
            // 健康条目（不该落库）
            table.entries.insert(
                ("owner-ok".to_string(), "https://ok.invalid".to_string()),
                Health {
                    failures: 0,
                    until: now,
                    touched: now,
                    revision: new_generation(),
                    probe: Weak::new(),
                },
            );
            let payload = snapshot(&table);
            assert_eq!(payload.entries.len(), 1, "只有仍在冷却期的条目该落库");
            write_payload(&payload).expect("临时库应当能写");
            // 模拟重启：清空内存
            table.entries.clear();
        }

        restore();

        let table = lock_table();
        let live = table
            .entries
            .get(&("owner-live".to_string(), "https://live.invalid".to_string()))
            .expect("冷却期内的条目应当被恢复");
        assert_eq!(live.failures, 3, "失败次数要跟着恢复（退避才能继续翻倍）");
        assert!(live.until > Instant::now(), "恢复后应当仍在冷却中");
        assert!(
            !table
                .entries
                .contains_key(&("owner-dead".to_string(), "https://dead.invalid".to_string())),
            "已过期的冷却不该复活"
        );
        assert!(
            !table
                .entries
                .contains_key(&("owner-ok".to_string(), "https://ok.invalid".to_string())),
            "健康条目不该被持久化"
        );
    }

    /// 坏数据不该让启动失败（这里只验证「解析失败 = 按没有冷却启动」）。
    #[test]
    fn corrupt_payload_is_ignored_at_startup() {
        let (db, _guard) = TempDb::open("proxy-rotate-health-corrupt");
        let _ = crate::server::core::proxy_pool::install(Some(db));
        // 写一份不是 JSON 的值
        let db = database().expect("临时库应当可用");
        db.with(|conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![KV_KEY, "not json at all"],
            )
            .map(|_| ())
        })
        .expect("写坏数据应当成功");
        {
            let mut table = lock_table();
            table.entries.clear();
        }
        // 不应 panic
        restore();
        assert!(lock_table().entries.is_empty());
    }
}

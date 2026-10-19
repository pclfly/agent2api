//! Sticky account egress. Reads never rotate; only failed network attempts advance.
//! Rotation changes routing, not provider/account entitlements.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use serde_json::Value;
use super::{proxies::ResolvedProxy, proxy_pool};

pub const MAX_IDS: usize = 64;
const MAX_CURSORS: usize = 512;
pub(crate) const MAX_ATTEMPTS: usize = 5;
const MAX_TEMPORARY_ATTEMPTS: usize = 2;
const MAX_HEALTH: usize = 1024;
const HEALTH_TTL: std::time::Duration = std::time::Duration::from_secs(600);
#[derive(Default)]
struct HealthTable { entries: HashMap<(String, String), Health> }
struct Health {
    failures: u32,
    until: std::time::Instant,
    touched: std::time::Instant,
    revision: u64,
    probe: std::sync::Weak<()>,
}
struct HealthPermit { key: (String, String), revision: u64, _probe: Arc<()> }
fn health() -> &'static Mutex<HealthTable> {
    static STATE: OnceLock<Mutex<HealthTable>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HealthTable::default()))
}
impl HealthTable {
    fn acquire(&mut self, key: (String, String), now: std::time::Instant) -> Option<HealthPermit> {
        self.entries.retain(|_, h| now.saturating_duration_since(h.touched) < HEALTH_TTL || h.probe.strong_count() > 0 || h.until > now);
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_HEALTH {
            if let Some(oldest) = self.entries.iter().filter(|(_,h)| h.failures == 0 && h.probe.strong_count() == 0).min_by_key(|(_,h)| h.touched).map(|(k,_)| k.clone()) {
                self.entries.remove(&oldest);
            } else {
                // A full health cache is not evidence that a previously unseen exit is bad.
                return Some(HealthPermit { key, revision:0, _probe:Arc::new(()) });
            }
        }
        let h = self.entries.entry(key.clone()).or_insert_with(|| Health { failures:0, until:now, touched:now, revision: generation(), probe:std::sync::Weak::new() });
        if h.until > now || (h.failures > 0 && h.probe.strong_count() > 0) { return None; }
        h.touched = now;
        let probe = Arc::new(());
        if h.failures > 0 { h.probe = Arc::downgrade(&probe); }
        Some(HealthPermit { key, revision:h.revision, _probe:probe })
    }
    fn finish(&mut self, permit: &HealthPermit, failure: bool, retry_after: std::time::Duration, now: std::time::Instant) -> Option<Arc<HealthPermit>> {
        let Some(h) = self.entries.get_mut(&permit.key).filter(|h| h.revision == permit.revision) else { return None };
        if !failure {
            h.failures = 0; h.until = now; h.touched = now; h.revision = generation();
            let probe = Arc::new(()); h.probe = Arc::downgrade(&probe);
            return Some(Arc::new(HealthPermit { key:permit.key.clone(), revision:h.revision, _probe:probe }));
        }
        h.failures = h.failures.saturating_add(1);
        let delay = std::time::Duration::from_secs((2u64 << h.failures.min(7)).min(300)).max(retry_after);
        h.until = now + delay; h.touched = now; h.revision = generation(); h.probe = std::sync::Weak::new();
        None
    }
}
#[derive(Clone)]
struct Selected {
    generation: u64,
    id: String,
    proxy: ResolvedProxy,
    lease: Option<Arc<super::sub_kernel::runtime::TemporaryNode>>,
    touched: std::time::Instant,
}
const STICKY_IDLE: std::time::Duration = std::time::Duration::from_secs(120);
fn expire_idle(state: &mut HashMap<String, Selected>, key: &str, generation: u64, now: std::time::Instant) -> bool {
    if state.get(key).is_some_and(|s| s.generation == generation && now.saturating_duration_since(s.touched) >= STICKY_IDLE) { state.remove(key); }
    state.get(key).is_some_and(|s| s.generation == generation)
}
fn reap_idle() {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    struct Guard;
    impl Drop for Guard { fn drop(&mut self) { RUNNING.store(false, std::sync::atomic::Ordering::SeqCst); } }
    // One bounded reaper, not one task per selection. Intentionally no spawn_task:
    // inheriting the request context would pin the very lease being reclaimed.
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) { return; }
        let guard = Guard;
        runtime.spawn(async move {
            let _guard = guard;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let now = std::time::Instant::now();
                let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
                let stale: Vec<_> = state.iter().filter(|(_,s)| s.lease.is_some()).map(|(k,s)|(k.clone(),s.generation)).collect();
                for (key,generation) in stale { expire_idle(&mut state, &key, generation, now); }
            }
        });
    }
}
fn touch(key: &str, generation: u64) {
    if let Some(s) = selections().lock().unwrap_or_else(|e| e.into_inner()).get_mut(key).filter(|s| s.generation == generation) { s.touched = std::time::Instant::now(); }
}
fn selections() -> &'static Mutex<HashMap<String, Selected>> {
    static STATE: OnceLock<Mutex<HashMap<String, Selected>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn is_all(config: &Value) -> bool { config.get("all").and_then(Value::as_bool) != Some(false) }
fn text<'a>(v: &'a Value, key: &str) -> &'a str { v.get(key).and_then(Value::as_str).unwrap_or("") }
fn configured_ids(config: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(items) = config.get("proxyIds").and_then(Value::as_array) {
        for id in items.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            if !ids.iter().any(|old| old == id) { ids.push(id.to_string()); }
            if ids.len() == MAX_IDS { break; }
        }
    }
    ids
}
pub fn candidate_ids(config: &Value) -> Vec<String> {
    let items = proxy_pool::raw_items();
    ordered_ids(config, &items)
}
fn ordered_ids(config: &Value, items: &[Value]) -> Vec<String> {
    let configured = configured_ids(config);
    let mut candidates: Vec<&Value> = if is_all(config) {
        items.iter().filter(|item| proxy_pool::is_enabled(item)).collect()
    } else {
        configured.iter().filter_map(|id| items.iter().find(|item| text(item, "id") == id && proxy_pool::is_enabled(item))).collect()
    };
    // Stable partition: ready listeners first; unopened subscription nodes last.
    candidates.sort_by_key(|item| unopened(item));
    candidates.into_iter().map(|v| text(v, "id").to_string()).filter(|s| !s.is_empty()).collect()
}
fn unopened(item: &Value) -> bool {
    text(item, "source") == "subscription" && item.get("port").and_then(Value::as_u64).filter(|p| *p > 0 && *p <= 65535).is_none()
}
fn generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
fn remember(key: &str, selected: Selected) -> Selected {
    if key.is_empty() || super::sub_kernel::runtime::temporary_stopping() { return selected; }
    let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
    if super::sub_kernel::runtime::temporary_stopping() { return selected; }
    if state.len() >= MAX_CURSORS && !state.contains_key(key) { state.clear(); }
    // Publish only into a vacant slot: a slow resolver must not overwrite a winner.
    let fresh = !state.contains_key(key);
    let chosen = state.entry(key.to_string()).or_insert(selected).clone();
    drop(state);
    if fresh && chosen.lease.is_some() { reap_idle(); }
    chosen
}
pub(crate) fn retire(key: &str) {
    selections().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
}
pub(crate) fn shutdown() {
    selections().lock().unwrap_or_else(|e| e.into_inner()).clear();
    health().lock().unwrap_or_else(|e| e.into_inner()).entries.clear();
}
fn current(config: &Value, key: &str) -> Option<Selected> {
    let ids = candidate_ids(config);
    let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
    if state.get(key).is_some_and(|s| ids.contains(&s.id)) { state.get(key).cloned() }
    else { state.remove(key); None }
}
/// Compatibility name: this is a sticky read, never an advancing cursor.
pub fn next(config: &Value, key: &str) -> Result<ResolvedProxy, String> {
    if let Some(selected) = current(config, key) { return Ok(selected.proxy); }
    for id in candidate_ids(config) {
        if let Ok(proxy) = proxy_pool::resolve_reference(&id) {
            return Ok(remember(key, Selected { generation: generation(), id, proxy, lease: None, touched: std::time::Instant::now() }).proxy);
        }
    }
    Err("轮询没有已就绪出口；转发时将按需尝试未分配端口的订阅节点".into())
}
pub fn first_usable(config: &Value) -> Result<ResolvedProxy, String> { next(config, "") }
pub struct Summary { pub all: bool, pub total: usize, pub usable: usize, pub error: Option<String> }
pub fn summary(config: &Value) -> Summary {
    let items = proxy_pool::raw_items();
    let ids = ordered_ids(config, &items);
    let usable = ids.iter().filter(|id| items.iter().any(|v| text(v,"id") == *id && !unopened(v))).count();
    Summary { all: is_all(config), total: ids.len(), usable, error: if ids.is_empty() { Some("轮询没有启用的候选出口".into()) } else { None } }
}

/// Per-request attempt set prevents wrapping back to a failed exit. Strong leases keep
/// retired sidecars alive until all requests/streams that used them have finished.
#[derive(Clone, Default)]
pub(crate) struct Attempts {
    key: String,
    config: Option<Value>,
    tried: HashSet<String>,
    selected: Option<Selected>,
    leases: Vec<Arc<super::sub_kernel::runtime::TemporaryNode>>,
    pub network_started: bool,
    pub local_failure: bool,
    origin: String,
    permit: Option<Arc<HealthPermit>>,
    stream_health: Option<Arc<HealthPermit>>,
    pub deadline: Option<tokio::time::Instant>,
    used: usize,
    temporary_used: usize,
    exchanged: bool,
    #[cfg(test)]
    items: Option<Vec<Value>>,
}
impl Attempts {
    pub fn configure(&mut self, key: &str, config: Option<&Value>) {
        self.leases.clear();
        self.permit = None; self.stream_health = None; self.origin.clear(); self.used = 0; self.temporary_used = 0; self.exchanged = false;
        self.deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(crate::server::config::timeout_settings().headers_ms()));
        self.network_started = false;
        self.local_failure = false;
        self.key = key.to_string();
        self.config = config.filter(|v| super::proxies::is_rotate_config(v)).cloned();
        self.tried.clear();
        self.selected = self.config.as_ref().and_then(|c| current(c, key));
    }
    pub fn active(&self) -> bool { self.config.is_some() }
    pub fn all_errors(&self) -> bool { self.config.as_ref().and_then(|v| v.get("failureMode")).and_then(Value::as_str) == Some("all_errors") }
    pub fn set_origin(&mut self, url: &url::Url) { self.origin = url.origin().ascii_serialization(); }
    pub fn remaining(&self) -> std::time::Duration {
        self.deadline.map(|d| d.saturating_duration_since(tokio::time::Instant::now())).unwrap_or_default()
    }
    pub fn finish_health(&mut self, failure: bool, retry_after: std::time::Duration) {
        if let Some(permit) = self.permit.take() { self.stream_health = health().lock().unwrap_or_else(|e| e.into_inner()).finish(&permit, failure, retry_after, std::time::Instant::now()); }
    }
    fn acquire(&mut self, id: &str) -> bool {
        if self.origin.is_empty() { return true; }
        self.permit = health().lock().unwrap_or_else(|e| e.into_inner()).acquire((id.to_string(), self.origin.clone()), std::time::Instant::now()).map(Arc::new);
        self.permit.is_some()
    }
    pub fn begin_exchange(&mut self) {
        self.permit = None; self.stream_health = None;
        if self.exchanged || self.deadline.is_none() {
            self.deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(crate::server::config::timeout_settings().headers_ms()));
            self.used = 0; self.temporary_used = 0;
        }
        self.exchanged = true;
        self.tried.clear();
        #[cfg(test)]
        if self.items.is_some() { return; }
        if let Some(config) = &self.config { self.selected = current(config, &self.key).or(self.selected.take()); }
    }
    fn items(&self) -> Vec<Value> {
        #[cfg(test)]
        if let Some(items) = &self.items { return items.clone(); }
        proxy_pool::raw_items()
    }
    #[cfg(test)]
    pub fn mock_aggressive(key: &str, items: Vec<Value>) -> Self {
        let mut attempts = Self::mock(key,items);
        attempts.config.as_mut().unwrap()["failureMode"] = serde_json::json!("all_errors"); attempts
    }
    #[cfg(test)]
    pub fn mock(key: &str, items: Vec<Value>) -> Self {
        Self { key:key.into(), config:Some(serde_json::json!({"source":"rotate","all":true})), items:Some(items), ..Self::default() }
    }
    pub fn release_exchange_lease(&mut self) { self.selected = None; self.leases.clear(); }
    pub fn adopt(&mut self, exchange: &Self) {
        if exchange.selected.as_ref().map(|s| s.generation).unwrap_or(0) >= self.selected.as_ref().map(|s| s.generation).unwrap_or(0) {
            self.selected = exchange.selected.clone();
            self.origin = exchange.origin.clone();
            self.stream_health = exchange.stream_health.clone();
        }
        self.leases = exchange.leases.clone();
    }
    pub fn retire_failed_stream(&mut self) {
        if let Some(old) = self.selected.take() {
            let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
            if state.get(&self.key).is_some_and(|s| s.generation == old.generation) {
                state.remove(&self.key);
            }
        }
        // Sticky ownership and stream health are independent: idle retirement or a
        // concurrent switch must not hide a real failure of this stream's exit.
        // Only its saved successful-header revision may fail, never a newer result.
        if let Some(permit) = self.stream_health.take() {
            health().lock().unwrap_or_else(|e| e.into_inner()).finish(&permit, true, std::time::Duration::ZERO, std::time::Instant::now());
        }
    }
    pub fn lease(&self) -> Option<Arc<super::sub_kernel::runtime::TemporaryNode>> {
        self.selected.as_ref().and_then(|s| s.lease.clone())
    }
    pub async fn select(&mut self, failed: bool) -> Result<Option<ResolvedProxy>, String> {
        let Some(config) = self.config.clone() else { return Ok(None) };
        if failed {
            self.permit = None;
            if let Some(old) = self.selected.take() {
                self.tried.insert(old.id.clone());
                let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
                // A late failure must not retire a newer concurrent selection.
                if state.get(&self.key).is_some_and(|s| s.generation == old.generation) { state.remove(&self.key); }
                else if let Some(newer) = state.get(&self.key).filter(|s| !self.tried.contains(&s.id)).cloned() {
                    self.selected = Some(newer);
                }
                // The transport has buffered the failed HTTP body before retiring this
                // request's lease. Other in-flight owners keep their sidecars alive.
                if let Some(lease) = old.lease { self.leases.retain(|l| !Arc::ptr_eq(l, &lease)); }
            }
        }
        if let Some(s) = self.selected.clone() {
            if !self.tried.contains(&s.id) && (self.permit.is_some() || self.acquire(&s.id)) {
                if failed || self.used == 0 {
                    if self.used >= MAX_ATTEMPTS { return Err("轮询尝试预算已耗尽（5 次）".into()); }
                    self.used += 1;
                }
                touch(&self.key, s.generation);
                self.leases.clear();
                if let Some(l) = &s.lease { self.leases.push(l.clone()); }
                return Ok(Some(s.proxy));
            }
            self.tried.insert(s.id.clone());
            self.selected = None;
            let mut state = selections().lock().unwrap_or_else(|e| e.into_inner());
            if state.get(&self.key).is_some_and(|old| old.generation == s.generation) { state.remove(&self.key); }
        }
        let items = self.items();
        let mut last = "轮询出口已耗尽".to_string();
        for id in ordered_ids(&config, &items) {
            if self.tried.contains(&id) { continue; }
            let Some(item) = items.iter().find(|v| text(v,"id") == id) else { continue };
            if self.used >= MAX_ATTEMPTS { return Err("轮询尝试预算已耗尽（5 次）".into()); }
            if unopened(item) && self.temporary_used >= MAX_TEMPORARY_ATTEMPTS { continue; }
            if !self.acquire(&id) { self.tried.insert(id); continue; }
            self.used += 1;
            if unopened(item) { self.temporary_used += 1; }
            let result = if unopened(item) {
                super::sub_kernel::runtime::temporary_node(&self.key, text(item,"providerId"), text(item,"node")).await
                    .map(|lease| (lease.proxy.clone(), Some(lease)))
            } else {
                proxy_pool::resolve_item(item).and_then(|r| r.resolved().cloned())
                    .ok_or_else(|| "代理节点不可用".to_string()).map(|p| (p, None))
            };
            match result {
                Ok((proxy, lease)) => {
                    let selected = remember(&self.key, Selected { generation: generation(), id: id.clone(), proxy, lease, touched: std::time::Instant::now() });
                    if self.tried.contains(&selected.id) { return Err("并发选路仍指向本请求已失败出口".into()); }
                    if selected.id != id && !self.acquire(&selected.id) { return Err("并发选路出口正在冷却".into()); }
                    self.leases.clear();
                    if let Some(l) = &selected.lease { self.leases.push(l.clone()); }
                    let proxy = selected.proxy.clone();
                    self.selected = Some(selected);
                    return Ok(Some(proxy));
                }
                Err(error) => { self.finish_health(true, std::time::Duration::ZERO); self.tried.insert(id); last = error; }
            }
        }
        Err(last)
    }
}

tokio::task_local! {
    pub(crate) static ATTEMPTS: Arc<tokio::sync::Mutex<Attempts>>;
}
pub(crate) fn context() -> Option<Arc<tokio::sync::Mutex<Attempts>>> { ATTEMPTS.try_with(Arc::clone).ok() }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn stream_failure_survives_idle_or_sticky_switch_but_not_new_health_result() {
        for mode in ["idle","switch","new-success","new-failure"] {
            let key=format!("stream-revision-{mode}");let id=format!("node-{mode}");
            let items=vec![json!({"id":id,"source":"manual","host":"127.0.0.1","port":32111})];
            let mut stream=Attempts::mock(&key,items);stream.set_origin(&url::Url::parse("https://revision.test/").unwrap());
            stream.select(false).await.unwrap();stream.finish_health(false,std::time::Duration::ZERO);
            let health_key=(id.clone(),stream.origin.clone());
            let mut newer=None;
            if mode=="idle" {let old=stream.selected.as_ref().unwrap();expire_idle(&mut selections().lock().unwrap(),&key,old.generation,old.touched+STICKY_IDLE);}
            else {let mut replacement=stream.selected.clone().unwrap();replacement.generation=generation();replacement.id="another-node".into();newer=Some(replacement.generation);selections().lock().unwrap().insert(key.clone(),replacement);}
            if mode.starts_with("new-") {
                let mut table=health().lock().unwrap();let now=std::time::Instant::now();let p=table.acquire(health_key.clone(),now).unwrap();
                table.finish(&p,mode=="new-failure",std::time::Duration::ZERO,now);
            }
            let revision=health().lock().unwrap().entries[&health_key].revision;
            stream.retire_failed_stream();
            let table=health().lock().unwrap();let h=&table.entries[&health_key];
            if mode=="new-success" {assert_eq!(h.failures,0);assert_eq!(h.revision,revision);}
            else if mode=="new-failure" {assert_eq!(h.failures,1);assert_eq!(h.revision,revision);}
            else {assert_eq!(h.failures,1);assert_ne!(h.revision,revision);}
            drop(table);
            if let Some(generation)=newer {assert_eq!(selections().lock().unwrap()[&key].generation,generation);}
            retire(&key);
        }
    }
    #[test]
    fn health_origin_backoff_half_open_cancel_and_success_reset() {
        use std::time::{Duration, Instant};
        let mut table = HealthTable::default(); let now = Instant::now();
        let key = ("node".into(), "https://one.test".into());
        let first = table.acquire(key.clone(), now).unwrap();
        let late = table.acquire(key.clone(), now).unwrap();
        table.finish(&first, true, Duration::ZERO, now);
        table.finish(&late, false, Duration::ZERO, now); // stale success cannot reset newer failure
        assert!(table.acquire(key.clone(), now).is_none());
        assert!(table.acquire(("node".into(), "https://two.test".into()), now).is_some());
        let probe = table.acquire(key.clone(), now + Duration::from_secs(4)).unwrap();
        assert!(table.acquire(key.clone(), now + Duration::from_secs(4)).is_none());
        drop(probe); // cancellation immediately releases the half-open reservation
        let probe = table.acquire(key.clone(), now + Duration::from_secs(4)).unwrap();
        table.finish(&probe, true, Duration::ZERO, now + Duration::from_secs(4));
        assert!(table.acquire(key.clone(), now + Duration::from_secs(11)).is_none());
        let probe = table.acquire(key.clone(), now + Duration::from_secs(12)).unwrap();
        table.finish(&probe, false, Duration::ZERO, now + Duration::from_secs(12));
        assert!(table.acquire(key.clone(), now + Duration::from_secs(12)).is_some());
        table.finish(&late, true, Duration::ZERO, now + Duration::from_secs(12));
        assert_eq!(table.entries[&key].failures, 0); // stale failure cannot retire new health generation
    }
    #[test]
    fn health_capacity_ttl_and_retry_after() {
        use std::time::{Duration, Instant};
        let mut table = HealthTable::default(); let now = Instant::now();
        for i in 0..MAX_HEALTH { assert!(table.acquire((i.to_string(), "origin".into()), now).is_some()); }
        assert!(table.acquire(("overflow".into(), "origin".into()), now).is_some());
        assert_eq!(table.entries.len(), MAX_HEALTH);
        for h in table.entries.values_mut() { h.failures=1; h.until=now+std::time::Duration::from_secs(900); }
        assert_eq!(table.acquire(("uncached".into(),"origin".into()),now).unwrap().revision,0);
        assert_eq!(table.entries.len(),MAX_HEALTH);
        table.entries.clear();
        let key = ("after-ttl".into(), "origin".into());
        let p = table.acquire(key.clone(), now + HEALTH_TTL).unwrap();
        assert_eq!(table.entries.len(), 1);
        table.finish(&p, true, Duration::from_secs(900), now + HEALTH_TTL);
        assert!(table.acquire(key, now + HEALTH_TTL + Duration::from_secs(600)).is_none());
    }
    #[tokio::test]
    async fn selection_budget_and_temporary_budget_are_bounded() {
        let items: Vec<_> = (0..20).map(|i| json!({"id":format!("budget-{i}"),"source":"manual","host":"127.0.0.1","port":32200+i})).collect();
        let mut a = Attempts::mock("budget-test", items); a.begin_exchange();
        for i in 0..MAX_ATTEMPTS { assert!(a.select(i > 0).await.unwrap().is_some()); }
        assert!(a.select(true).await.is_err()); assert_eq!(a.used, MAX_ATTEMPTS);
        retire("budget-test");
        // Exhausting temporary budget must skip cold items without touching subscription DB.
        let mut a = Attempts::mock("temp-budget", vec![json!({"id":"cold","source":"subscription"})]);
        a.temporary_used = MAX_TEMPORARY_ATTEMPTS;
        assert!(a.select(false).await.is_err()); assert_eq!(a.used, 0);
    }
    #[tokio::test]
    async fn origin_cooldown_skips_sticky_exit_without_repeating_candidates() {
        let items = vec![json!({"id":"cool-a","source":"manual","host":"127.0.0.1","port":32105}),json!({"id":"cool-b","source":"manual","host":"127.0.0.1","port":32106})];
        let mut a = Attempts::mock("cool-test", items.clone());
        a.set_origin(&url::Url::parse("https://cool.test/path?secret=hidden").unwrap());
        assert_eq!(a.select(false).await.unwrap().unwrap().port, Some(32105));
        a.finish_health(true, std::time::Duration::ZERO);
        let mut b = Attempts::mock("cool-test", items); b.selected = a.selected.clone(); b.origin = a.origin.clone();
        assert_eq!(b.select(false).await.unwrap().unwrap().port, Some(32106));
        assert!(b.tried.contains("cool-a")); b.finish_health(false,std::time::Duration::ZERO);
        retire("cool-test");
    }
    #[tokio::test]
    async fn idle_expiry_is_generation_guarded_and_does_not_drop_active_owner() {
        let mut a=Attempts::mock("idle-expiry",vec![json!({"id":"idle-node","source":"manual","host":"127.0.0.1","port":32109})]);
        a.select(false).await.unwrap();let held=a.selected.clone().unwrap();
        let mut state=HashMap::from([("idle-expiry".to_string(),held.clone())]);
        assert!(expire_idle(&mut state,"idle-expiry",held.generation,held.touched+STICKY_IDLE-std::time::Duration::from_millis(1)));
        assert!(!expire_idle(&mut state,"idle-expiry",held.generation+1,held.touched+STICKY_IDLE));
        assert!(state.contains_key("idle-expiry"));
        assert!(!expire_idle(&mut state,"idle-expiry",held.generation,held.touched+STICKY_IDLE));
        assert_eq!(a.selected.unwrap().proxy.port,Some(32109));retire("idle-expiry");
    }
    #[test]
    fn failure_mode_schema_roundtrips_and_defaults_to_smart() {
        for all in [true,false] {
            for mode in ["smart","all_errors"] {
                let cfg=json!({"source":"rotate","all":all,"proxyIds":["a"],"failureMode":mode});
                let out=super::super::proxies::normalize_account_proxy(&cfg).unwrap().unwrap();
                assert_eq!(out["failureMode"],mode);
            }
        }
        assert_eq!(super::super::proxies::normalize_account_proxy(&json!({"source":"rotate"})).unwrap().unwrap()["failureMode"],"smart");
        assert!(super::super::proxies::normalize_account_proxy(&json!({"source":"rotate","failureMode":"invalid"})).is_err());
    }
    #[tokio::test]
    async fn late_failure_uses_newer_selection_and_reads_are_sticky() {
        let items = vec![json!({"id":"a", "source":"manual","host":"127.0.0.1","port":32101}), json!({"id":"b", "source":"manual","host":"127.0.0.1","port":32102})];
        let mut a = Attempts::mock("late-failure", items.clone());
        assert_eq!(a.select(false).await.unwrap().unwrap().port, Some(32101));
        let mut b = Attempts::mock("late-failure", items);
        b.selected = a.selected.clone();
        assert_eq!(b.select(true).await.unwrap().unwrap().port, Some(32102));
        assert_eq!(a.select(true).await.unwrap().unwrap().port, Some(32102));
        for _ in 0..3 { assert_eq!(a.select(false).await.unwrap().unwrap().port, Some(32102)); }
        retire("late-failure");
    }
    #[tokio::test]
    async fn failed_partial_stream_is_not_replayed_and_next_exchange_skips_exit() {
        let items=vec![json!({"id":"stream-a","source":"manual","host":"127.0.0.1","port":32103}),json!({"id":"stream-b","source":"manual","host":"127.0.0.1","port":32104})];
        let mut a=Attempts::mock("stream-failure",items.clone());
        a.set_origin(&url::Url::parse("https://stream.test/").unwrap());
        assert_eq!(a.select(false).await.unwrap().unwrap().port,Some(32103));
        a.finish_health(false,std::time::Duration::ZERO);
        a.retire_failed_stream();
        let mut different=Attempts::mock("stream-other",items.clone());different.set_origin(&url::Url::parse("https://other.test/").unwrap());
        assert_eq!(different.select(false).await.unwrap().unwrap().port,Some(32103));retire("stream-other");
        let mut single=Attempts::mock("stream-single",vec![items[0].clone()]);single.set_origin(&url::Url::parse("https://stream.test/").unwrap());
        assert!(single.select(false).await.is_err());
        health().lock().unwrap().entries.get_mut(&("stream-a".into(),"https://stream.test".into())).unwrap().until=std::time::Instant::now();
        single.begin_exchange();assert_eq!(single.select(false).await.unwrap().unwrap().port,Some(32103));
        single.finish_health(true,std::time::Duration::ZERO);retire("stream-single");
        let mut next=Attempts::mock("stream-failure",items);next.set_origin(&url::Url::parse("https://stream.test/").unwrap());next.begin_exchange();
        assert_eq!(next.select(false).await.unwrap().unwrap().port,Some(32104));
        retire("stream-failure");
    }
    #[test]
    fn assigned_first_disabled_excluded_and_explicit_order_preserved() {
        let items = vec![json!({"id":"cold", "source":"subscription", "enabled":true}), json!({"id":"ready", "source":"subscription", "enabled":true,"port":1234}), json!({"id":"off","enabled":false,"port":1235})];
        assert_eq!(ordered_ids(&json!({"all":true}), &items), vec!["ready", "cold"]);
        assert_eq!(ordered_ids(&json!({"all":false,"proxyIds":["cold","off","ready","ready"]}), &items), vec!["ready", "cold"]);
    }
}

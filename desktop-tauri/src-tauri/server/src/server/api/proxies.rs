//! 代理管理：手动地址 CRUD、Clash Verge 端点同步、启停与连通性测试。
//! 写操作返回最新代理列表，测试失败以 `success:false` 返回 200。

use axum::body::Bytes;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::normalize_account_proxy;
use crate::server::core::{proxy_pool, proxy_settings};
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

use super::accounts::proxy_error;

/// 账号写入代理之后：把被引用的池条目**启用**，并在必要时惊动内核。
///
/// ── 为什么要与写入分两步 ────────────────────────────────────
/// 账号写入（`AccountStore::apply_patch` / `plugin_upsert_*`）持着账号那把锁，
/// 而代理池有它自己的一次读-改-写。并发场景下「账号指向它」与「它刚被禁用」
/// 可能交叉发生：先启用后写账号，则那一下启用可能被旧快照盖掉；先写账号再启用，
/// 账号里存的就是用户刚选的那条、而池条目以**最后落盘的那次**为准。选后者：
/// 启用失败得不阻断账号写入（条目被删 / Clash 侧停用都有既有的报错口径）。
///
/// ── 何时调它 ──────────────────────────────────────────────
/// 只在代理**确实发生变化**时调（调用方用写入结果里的变更项判定）。界面每次
/// 保存都会把 proxy 原样发回来，按「提交即启用」处理会跟用户的后续操作打架：
/// 用户在「网络代理」页手动禁了一条，之后回账号页改个备注名（表单顺手带着旧
/// 代理）就又被打开了。
///
/// 返回一句给用户看的说明（没有变更时为 None），归入响应里的 `changes`。
pub(super) fn enable_referenced_proxy(next: Option<&Value>) -> Option<String> {
    let enabled = proxy_pool::enable_for_account_proxy(next);
    if enabled.is_empty() {
        return None;
    }
    let names: Vec<&str> = enabled.iter().map(|item| item.name.as_str()).collect();
    Some(format!("代理「{}」原为禁用，已自动启用", names.join("」、「")))
}

/// 对账结果：账号引用的订阅节点缺端口时该做什么、以及做没做成。
///
/// 为什么要分三态而不是 `Option<usize>`：`None` 会把「本来就没缺口」
/// 「内核没装」「用户手动关了内核」三件事混成一个答案，而它们在界面上要
/// 说的话完全不同 —— 第一件事不必提示，第三件事必须告诉用户去点「启动内核」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ListenerReconcile {
    /// 没有缺口（或没有账号引用订阅节点）：什么都不用做
    Settled,
    /// 发现 N 个缺口，已请求内核补建
    Requested(usize),
    /// 有缺口但**不能**补：内核没装 / 没有启用订阅 / 用户手动关闭了内核
    Blocked,
}

/// 账号引用的订阅节点**没有监听端口**时，让内核补建。
///
/// ── 要解决的问题 ────────────────────────────────────────────
/// 订阅节点的端口由内核按「账号引用中的启用条目」分配（lazy listeners，见
/// `proxy_pool::listener_node_keys`）。但这张表是**某一轮启动时算出来的**：
///
///   · 账号先选了一个从未分配端口的节点 → 表里没有它 → 转发报「尚未分配
///     监听端口」；
///   · 内核在跑、用户改完账号后没有别的动作触发重建 → 一直是那个状态。
///
/// 所以每次账号写代理之后都要对一次账：把「该开监听器却没有端口」的节点找
/// 出来，有缺口就投一次内核重建。`needs_listener` 与内核侧同口径（见它的
/// 说明），因此不会出现「判定说没有缺口、内核却不建」。
///
/// ── 什么情况下**不**补建 ────────────────────────────────
/// 内核未安装 / 没有启用订阅 / 用户手动关闭了内核：这三种都不该被一次账号
/// 编辑静默推翻（尤其最后一条 —— 用户刚按下「关闭内核」）。返回
/// [`ListenerReconcile::Blocked`]，由界面按既有文案提示内核状态。
pub(super) fn reconcile_listeners(
    store: &crate::server::core::account_store::AccountStore,
) -> ListenerReconcile {
    let pool_ids = store.referenced_pool_ids();
    if pool_ids.is_empty() {
        return ListenerReconcile::Settled;
    }
    let borrowed: std::collections::HashSet<&str> = pool_ids.iter().map(String::as_str).collect();
    // 口径与内核侧完全一致：该开监听器的节点集合减去已经有端口的那些
    let wanted = proxy_pool::listener_node_keys(&borrowed);
    let missing = missing_listener_count(
        &wanted,
        proxy_pool::subscription_entries_with_ports(),
    );
    if missing == 0 {
        return ListenerReconcile::Settled;
    }
    // 缺口先算出来再判能不能补：即使这次补不了，也要如实说出「有几个缺」
    // —— 界面据此提示用户去点「启动内核」而不是让那句「未分配端口」孤零零挂着。
    if !crate::server::core::sub_kernel::installed()
        || crate::server::core::sub_kernel::is_manually_halted()
    {
        return ListenerReconcile::Blocked;
    }
    let providers = crate::server::core::subscriptions::enabled_provider_inputs();
    if providers.is_empty() {
        return ListenerReconcile::Blocked;
    }
    logging::log(
        "[Proxies]",
        &format!("发现 {missing} 个账号引用的订阅节点还没有监听端口，正在请求内核补建"),
    );
    crate::server::core::sub_kernel::request_restart(providers);
    ListenerReconcile::Requested(missing)
}

/// 把对账结果写进响应：`listenersMissing` 是缺口数，`listenerBlocked` 说明
/// 「有缺口但这次补不了」（界面据此提示去启动内核，而不是让用户对着
/// 「未分配端口」发呆）。
pub(super) fn annotate_listener_reconcile(data: &mut Value, outcome: ListenerReconcile) {
    if let Some(object) = data.as_object_mut() {
        match outcome {
            ListenerReconcile::Settled => {}
            ListenerReconcile::Requested(missing) => {
                object.insert("listenersMissing".to_string(), json!(missing));
            }
            ListenerReconcile::Blocked => {
                object.insert("listenerBlocked".to_string(), Value::Bool(true));
            }
        }
    }
}

/// 缺端口且**确实该有端口**的节点有几个：`wanted`（该开监听器的节点键集合，
/// `<订阅id>::<节点名>`）× 池里当前的端口状态。
///
/// 抽成独立函数是为了让它可测：两个集合的口径对不齐时，症状是「一直请求重建
/// 却永远补不出端口」或者「端口挂着却没人用」，两者都不会报错。
fn missing_listener_count(
    wanted: &std::collections::HashSet<String>,
    entries: Vec<(String, String, Option<u16>)>,
) -> usize {
    entries
        .into_iter()
        .filter(|(provider_id, node, port)| {
            port.is_none() && wanted.contains(&format!("{provider_id}::{node}"))
        })
        .count()
}

pub async fn list_proxies(state: &ServerState) -> Response {
    let snapshot = state.store().list_accounts();
    let accounts: Vec<Value> = snapshot.get("accounts").and_then(Value::as_array)
        .map(|items| items.iter().map(|account| json!({
            "id": account.get("id"),
            "name": account.get("displayName").filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                .or_else(|| account.get("name")),
            "priority": account.get("priority"),
            "enabled": account.get("enabled"),
            "proxy": account.get("proxy"),
        })).collect()).unwrap_or_default();
    ok_json(json!({
        "accounts": accounts,
        "clash": crate::server::core::proxies::clash_proxy_options(),
        "note": "支持手动 HTTP / SOCKS5 地址与 Clash Verge 端点；未配置代理时直连",
    }))
}

pub async fn test_proxy(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let mut proxy = None;
    // 前置跳（`frontProxy`）：只有按账号测试时才可能带出来 —— 它是账号级配置，
    // 不属于代理池条目。
    let mut front = None;
    if let Some(id) = payload.get("id").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        let Some(creds) = state.store().get_credentials_by_id(id) else {
            return management_error(404, "账号不存在");
        };
        if let Some(error) = creds.proxy_error {
            return management_error(400, error);
        }
        match crate::server::core::proxies::ResolvedProxy::from_json(&creds.proxy) {
            Ok(resolved) => proxy = resolved,
            Err(reason) => return management_error(400, reason),
        }
        // 前置跳从账号的公开形态里取（凭证结构只给第二跳）。测试要覆盖完整
        // 链路：只测第二跳会把「前置跳配错了」测成「出口正常」。
        if let Some(session) = state.store().get_session_by_id(id) {
            match crate::server::core::upstream::resolve_front_proxy(Some(&session.session)) {
                Ok(resolved) => front = resolved,
                Err(reason) => return management_error(400, reason),
            }
        }
    } else if let Some(config_value) = payload.get("proxy") {
        let config = match normalize_account_proxy(config_value) {
            Ok(value) => value,
            Err(error) => return proxy_error(error),
        };
        if let Some(config) = config {
            if let Some(resolution) = crate::server::core::proxies::resolve_account_proxy(Some(&config)) {
                if let Some(error) = resolution.error() {
                    return management_error(400, error);
                }
                proxy = resolution.resolved().cloned();
            }
        }
    }
    // 有前置跳时测完整两跳链路；否则沿用单跳（或直连）的连通性测试。
    let result = if front.is_some() {
        match egress::test_account_route(
            proxy.as_ref(),
            front.as_ref(),
            "https://www.cloudflare.com/",
        )
        .await
        {
            value if value["success"].as_bool() == Some(true) => egress::ConnectivityResult {
                success: true,
                status: value["status"].as_u64().and_then(|code| u16::try_from(code).ok()),
                ip: String::new(),
                duration_ms: value["durationMs"].as_i64().unwrap_or(0),
                error: None,
            },
            value => egress::ConnectivityResult {
                success: false,
                status: value["status"].as_u64().and_then(|code| u16::try_from(code).ok()),
                ip: String::new(),
                duration_ms: value["durationMs"].as_i64().unwrap_or(0),
                error: value["error"].as_str().map(str::to_string),
            },
        }
    } else {
        egress::test_proxy(proxy.as_ref()).await
    };
    logging::log("[Accounts]", &format!("代理测试{}: {}",
        proxy.as_ref().map(|proxy| format!("（{}）", proxy.label)).unwrap_or_else(|| "（直连）".to_string()),
        if result.success { format!("✅ 出口 IP {}", result.ip) } else { format!("❌ {}", result.error.clone().unwrap_or_default()) },
    ));
    let mut data = result.to_json();
    data["proxy"] = egress::describe_public(proxy.as_ref());
    ok_json(data)
}

fn pool_references(state: &ServerState) -> std::collections::HashMap<String, Vec<Value>> {
    let mut references: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    let snapshot = state.store().list_accounts();
    let Some(accounts) = snapshot.get("accounts").and_then(Value::as_array) else {
        return references;
    };
    for account in accounts {
        let Some(config) = account.get("proxy").and_then(|proxy| proxy.get("config")) else { continue };
        let ids: Vec<String> = if crate::server::core::proxies::is_rotate_config(config) {
            crate::server::core::proxy_rotate::candidate_ids(config)
        } else {
            proxy_pool::usage::pool_reference_id(config).map(|id| vec![id.to_string()]).unwrap_or_default()
        };
        let entry = json!({
            "id": account.get("id"),
            "name": account.get("displayName").filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                .or_else(|| account.get("name")),
            "enabled": account.get("enabled").cloned().unwrap_or(Value::Bool(true)),
        });
        for id in ids {
            references.entry(id).or_default().push(entry.clone());
        }
    }
    references
}

fn pool_payload(state: &ServerState) -> Value {
    let references = pool_references(state);
    let service_references = proxy_pool::usage::service_references();
    // 现存订阅 id：订阅节点的「孤儿」判据（所属订阅已不存在）。
    // core 层不认识订阅存储（依赖方向是 subscriptions → proxy_pool），
    // 所以这一步只能在这里做；界面据此把孤儿单独列成一档好清理。
    let live_subs = crate::server::core::subscriptions::ids();
    let items: Vec<Value> = proxy_pool::list().into_iter().map(|mut item| {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let used = references.get(id).cloned().unwrap_or_default();
        let services: Vec<Value> = service_references.iter().filter(|(_, proxy_id)| id == proxy_id)
            .map(|(service, _)| Value::String((*service).to_string())).collect();
        item["usedBy"] = json!(used);
        item["usedByServices"] = json!(services);
        // 孤儿 = 订阅节点，但归属的订阅已经不在清单里（被删 / 存储被外部清掉）。
        // 这类条目任何同步都不会再碰它，界面要给它一条手动删除的出路。
        let orphan = item.get("source").and_then(Value::as_str) == Some("subscription")
            && !live_subs.contains(item.get("providerId").and_then(Value::as_str).unwrap_or(""));
        item["orphan"] = json!(orphan);
        item
    }).collect();
    json!({ "items": items, "clash": crate::server::core::proxies::clash_proxy_options() })
}

pub async fn pool_list(state: &ServerState) -> Response {
    let _ = proxy_pool::sync_clash(false);
    // 进入「网络代理」页时顺便对一次账：这样用户看到「未分配端口」后到本页
    // 时，端口就会自己补上，不必再回账号页改一次代理才能触发。
    // 只在有缺口时才会重建内核（见 `reconcile_listeners`）。
    let mut data = pool_payload(state);
    annotate_listener_reconcile(&mut data, reconcile_listeners(state.store()));
    ok_json(data)
}

pub async fn pool_sync_clash(state: &ServerState) -> Response {
    let report = proxy_pool::sync_clash(true);
    let mut data = pool_payload(state);
    data["changes"] = json!(report.changes);
    data["syncError"] = json!(report.error);
    ok_json(data)
}

/// `GET /api/proxies/pool/usable` —— 只列可用条目（账号侧代理下拉 / 轮询候选）。
///
/// 与 [`pool_list`] 的两处不同，都是刻意的：
///   · **不同步 Clash**：账号页画一个下拉不该触发一次镜像写入；
///   · **过滤掉已禁用的条目**：禁用的节点不该出现在「给账号挑一个出口」的
///     选项里 —— 选中它只会在转发时报「已禁用」，而用户看不出是自己关的。
///
/// 界面仍会为「当前引用的那条已不在选项里」补一个占位项，否则下拉会显示
/// 「没选中任何值」，而账号其实正引用着一个已禁用的出口。
pub async fn pool_usable(_state: &ServerState) -> Response {
    ok_json(json!({ "items": proxy_pool::list_usable() }))
}

pub async fn pool_create(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_pool::create(&payload) {
        Ok(_) => ok_json(pool_payload(state)),
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_update(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_pool::update(&payload) {
        Ok(_) => ok_json(pool_payload(state)),
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_remove(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少代理 id");
    }
    let exists = proxy_pool::list().iter().any(|item| item.get("id").and_then(Value::as_str) == Some(id));
    match proxy_pool::remove(id) {
        Ok((_, name)) => {
            let mut data = pool_payload(state);
            data["removed"] = json!(name);
            ok_json(data)
        }
        Err(message) => management_error(if exists { 400 } else { 404 }, message),
    }
}

pub async fn pool_enable(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少代理 id");
    }
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return management_error(400, "缺少 enabled（true / false）");
    };
    match proxy_pool::set_enabled(id, enabled) {
        Ok((_, change)) => {
            // 启用一条订阅节点后端口不会凭空出现（lazy listeners 要内核重建
            // 才分配）—— 顺手对一次账，否则用户点完开关仍看到「未分配端口」
            let mut data = pool_payload(state);
            data["name"] = json!(change.name);
            data["enabled"] = json!(change.enabled);
            if change.enabled {
                annotate_listener_reconcile(&mut data, reconcile_listeners(state.store()));
            }
            ok_json(data)
        }
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_test(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    // 订阅节点走**内核统一测试口**：先用控制器把测试策略组切到目标节点，再从
    // 那个固定端口发一次请求。这样「没有被账号引用、内核还没给它开专用监听器」
    // 的节点也能测 —— 而先测通再决定要不要用，正是最常见的用法。
    //
    // 手动 / Clash 条目仍走原来的 `resolve_for_test`（它们本身就是可直接连的
    // 地址，不需要内核参与）。
    let item = proxy_pool::find_item(id);
    if let Some((provider_id, node)) = item.as_ref().and_then(proxy_pool::subscription_target) {
        return pool_test_subscription(state, id, &provider_id, &node).await;
    }
    let proxy = match proxy_pool::resolve_for_test(id) {
        Ok(proxy) => proxy,
        Err(message) => return management_error(400, message),
    };
    let result = egress::test_proxy(Some(&proxy)).await;
    let write_error = proxy_pool::record_test(id, &proxy_pool::TestOutcome {
        success: result.success,
        ip: result.ip.clone(),
        duration_ms: result.duration_ms,
        error: result.error.clone(),
    }).err();
    logging::log("[Proxies]", &format!("代理测试（{}）: {}", proxy.label,
        if result.success { format!("✅ 出口 IP {}", result.ip) } else { format!("❌ {}", result.error.clone().unwrap_or_default()) },
    ));
    let mut data = result.to_json();
    data["proxy"] = egress::describe_public(Some(&proxy));
    if let Some(error) = write_error { data["saveError"] = json!(error); }
    data["items"] = pool_payload(state)["items"].clone();
    ok_json(data)
}

/// 测一个订阅节点（走内核统一测试口，不需要这个节点有专用监听端口）。
///
/// 失败原因分三档，界面上要看到的是**能行动**的那一句：
///   · 内核没装 / 被手动关闭 → 去下载或启动内核；
///   · 节点已不在订阅里 → 刷新订阅；
///   · 其余（测试口没就绪 / 请求失败）→ `select_test_node` 自己的说明。
async fn pool_test_subscription(
    state: &ServerState,
    id: &str,
    provider_id: &str,
    node: &str,
) -> Response {
    let proxy = match crate::server::core::sub_kernel::runtime::select_test_node(provider_id, node).await {
        Ok(port) => crate::server::core::proxies::ResolvedProxy {
            // 内核统一测试口是本机 http 监听器，与订阅节点在池里的形态一致
            source: "custom".to_string(),
            protocol: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: Some(port),
            username: String::new(),
            password: String::new(),
            label: node.to_string(),
        },
        Err(message) => {
            // 测试结论本身有效（「测不了」也是结论）—— 按失败返回 200，
            // 与手动条目的失败口径一致（界面读 success:false 显示原因）。
            let result = egress::ConnectivityResult {
                success: false,
                status: None,
                ip: String::new(),
                duration_ms: 0,
                error: Some(message.clone()),
            };
            let write_error = proxy_pool::record_test(id, &proxy_pool::TestOutcome {
                success: false,
                ip: String::new(),
                duration_ms: 0,
                error: Some(message.clone()),
            }).err();
            logging::log("[Proxies]", &format!("订阅节点测试（{node}）: ❌ {message}"));
            let mut data = result.to_json();
            if let Some(error) = write_error { data["saveError"] = json!(error); }
            data["items"] = pool_payload(state)["items"].clone();
            return ok_json(data);
        }
    };
    let result = egress::test_proxy(Some(&proxy)).await;
    let write_error = proxy_pool::record_test(id, &proxy_pool::TestOutcome {
        success: result.success,
        ip: result.ip.clone(),
        duration_ms: result.duration_ms,
        error: result.error.clone(),
    }).err();
    logging::log("[Proxies]", &format!("订阅节点测试（{node}，测试口 {}）: {}", proxy.label,
        if result.success { format!("✅ 出口 IP {}", result.ip) } else { format!("❌ {}", result.error.clone().unwrap_or_default()) },
    ));
    let mut data = result.to_json();
    data["proxy"] = egress::describe_public(Some(&proxy));
    if let Some(error) = write_error { data["saveError"] = json!(error); }
    data["items"] = pool_payload(state)["items"].clone();
    ok_json(data)
}

pub async fn settings_get() -> Response {
    ok_json(proxy_settings::get())
}

pub async fn settings_save(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_settings::save(&payload) {
        Ok(value) => ok_json(value),
        Err(message) => management_error(400, message),
    }
}

// ─── 出口冷却（轮询档的「哪条出口现在别用」）──────────────────

/// `GET /api/proxies/rotate/cooldowns`
///
/// 返回「当前有几条出口在冷却里、各还要等多久」。给界面显示「N 条冷却中」
/// 与排障用 —— 只回哈希后的 owner，不回账号 id 与出主机名（见
/// `proxy_rotate::health` 的模块头）。
pub async fn rotate_cooldowns(_state: &ServerState) -> Response {
    ok_json(crate::server::core::proxy_rotate::cooldown_summary(None))
}

/// `POST /api/proxies/rotate/clear-cooldowns`
///
/// 清空**全部**出口冷却。只清冷却，不动账号限额（`rateLimits`）、不重启内核
/// —— 那是三个互不相干的动作，混在一起会让排障时分不清「清掉了什么」。
///
/// 只接受空 body（或空对象）：留着筛选字段会让人以为「只清了某个节点」，
/// 而实现是全清 —— 那种误解比多一次确认更贵。
pub async fn clear_rotate_cooldowns(_state: &ServerState, body: &Bytes) -> Response {
    if !body.is_empty() {
        match parse_body(body) {
            Ok(payload) if payload.as_object().is_some_and(|object| object.is_empty()) => {}
            Ok(_) => {
                return management_error(400, "清除轮询冷却只接受空请求体或空对象 {}")
            }
            Err(_) => return management_error(400, "请求内容不是有效 JSON"),
        }
    }
    // 清除会同步提交 SQLite，不能占住异步运行时线程
    match tokio::task::spawn_blocking(crate::server::core::proxy_rotate::clear_cooldowns).await {
        Ok(Ok(result)) => {
            logging::log(
                "[ProxyRotate]",
                &format!("已清除轮询出口冷却 {} 条", result.cleared),
            );
            match serde_json::to_value(&result) {
                Ok(data) => ok_json(data),
                Err(_) => management_error(500, "清除轮询冷却结果序列化失败"),
            }
        }
        Ok(Err(message)) => management_error(500, format!("清除轮询冷却失败: {message}")),
        Err(_) => management_error(500, "清除轮询冷却任务执行失败"),
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use std::collections::HashSet;

    fn wanted(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|key| (*key).to_string()).collect()
    }

    /// 只有「该开监听器 + 还没端口」的条目算缺口。
    ///
    /// 四种组合都要分开断言：算错的两种后果都不会报错 ——
    /// 多算会让每次保存都白重建一次内核，少算会让转发一直报「未分配端口」。
    #[test]
    fn only_referenced_entries_without_ports_count_as_gaps() {
        let entries = vec![
            // 被引用 + 没端口 → 缺口
            ("sub_1".to_string(), "香港 01".to_string(), None),
            // 被引用 + 有端口 → 不缺
            ("sub_1".to_string(), "香港 02".to_string(), Some(19201)),
            // 没被引用 + 没端口 → 不缺（lazy listeners：没账号用它就不该开）
            ("sub_1".to_string(), "香港 03".to_string(), None),
            // 没被引用 + 有端口 → 不缺
            ("sub_1".to_string(), "香港 04".to_string(), Some(19202)),
        ];
        let wanted = wanted(&["sub_1::香港 01", "sub_1::香港 02"]);
        assert_eq!(missing_listener_count(&wanted, entries), 1);
    }

    /// 节点名里的空格与括号必须逐字匹配：`<订阅id>::<节点名>` 只要差一个字符
    /// 就会被当成「不在引用集合里」，而那批节点恰恰是用户正在用的。
    #[test]
    fn node_keys_match_names_verbatim() {
        let entries = vec![
            ("sub_2".to_string(), "node7 (207.57.140.129)".to_string(), None),
            ("sub_2".to_string(), "node7  (207.57.140.129)".to_string(), None),
        ];
        let wanted = wanted(&["sub_2::node7 (207.57.140.129)"]);
        assert_eq!(missing_listener_count(&wanted, entries), 1);
    }

    /// 没有账号引用任何订阅节点时不该有缺口（也不该因此去拉内核）。
    #[test]
    fn no_references_means_no_gaps() {
        let entries = vec![
            ("sub_3".to_string(), "节点 A".to_string(), None),
            ("sub_3".to_string(), "节点 B".to_string(), None),
        ];
        assert_eq!(missing_listener_count(&HashSet::new(), entries), 0);
    }

    /// 池里没有订阅条目时也是 0（手动地址与 Clash 条目不走内核）。
    #[test]
    fn manual_only_pool_has_no_gaps() {
        assert_eq!(missing_listener_count(&wanted(&["px_1"]), Vec::new()), 0);
    }
}

//! 订阅路由（「网络代理」页的「订阅」区块）。
//!
//!   GET  /api/subscriptions              订阅列表 + 内核状态
//!   POST /api/subscriptions/add          新增订阅 {name, url} 或 {name, kind:"file", sourcePath, content}
//!   POST /api/subscriptions/save         编辑订阅 {id, name, url|sourcePath, enabled}
//!   POST /api/subscriptions/refresh      强制刷新一条订阅的内容 {id}
//!   POST /api/subscriptions/remove       删除订阅 {id}
//!   POST /api/subscriptions/decode-qr    从图片解二维码 {imageBase64} → {url}
//!   POST /api/subscriptions/import-file  导入本地配置文件（壳弹选文件框后转发内容）
//!   POST /api/subscriptions/replace-file 本地文件订阅原地替换内容 {id, content, path?}
//!   GET  /api/subscriptions/kernel       内核状态（下载进度 / 运行状态）
//!   POST /api/subscriptions/kernel/download  开始下载内核（后台任务，轮询看进度）
//!   POST /api/subscriptions/kernel/restart   启动 / 重启内核（重建配置）
//!
//! ── 本地配置文件（`kind: "file"`）──────────────────────────
//! 用户可以导入一份 Clash / mihomo YAML（内含 `proxies` 列表）当订阅用。
//! 三条不变量：
//!   · **内容写进内核目录**（`sub_kernel::write_provider_file`）—— 内核直接
//!     读那份文件，原文件之后可以移动 / 删除；
//!   · **不参与定时刷新** —— 内核的 file provider 没有 interval（见
//!     `sub_kernel::runtime::build_config`），内容只在我们写入时变；
//!   · **校验在写盘前** —— YAML 必须能解析、顶层是映射、`proxies` 是非空
//!     列表。否则内核起不来（配置非法时 mihomo 会直接退出），用户看到的是
//!     「内核启动失败」而不是「你的文件有问题」，排查方向全错。
//!
//! 换成别的文件时走 `replace-file`（**原地替换**，条目 id 不变）：id 是
//! mihomo provider 名与账号引用链的一环，换 id 会让所有引用悬空、节点镜像
//! 重建（理由见 `replace_file` 的说明）。
//!
//! 文件内容由**壳**读出来随请求传（`{fileName, content, path}`）：服务端
//! 进程可能是 Docker / NAS 里的 headless 实例，与面板浏览器不在同一台机器上，
//! 「让服务端去读某个路径」只在本机桌面场景成立。`path` 只在壳是本机时给，
//! 用于「立即更新」从原路径重新导入。
//!
//! ── 分发方式 ────────────────────────────────────────────────
//! 与 proxies 同构：一条 `/api/subscriptions` 固定路径入口 + 按 (方法, 剩余
//! 段) 分发（http.rs 的路由按固定路径注册，通配段会与「已注册路径上的其它
//! 方法 → 404 信封」那条兜底互相干扰，见 http.rs 的注册注释）。
//!
//! ── 编排（core 不认识内核，api 层负责串）────────────────────
//! 写操作（add / save / remove）落库后投一条内核命令（重启重建配置 /
//! 删光订阅时停内核），**命令是异步的**：handler 立即返回最新列表 + 内核
//! 快照，节点数 / 运行状态由前端轮询（内核重启 + provider 拉取要几秒到
//! 几十秒，同步等会把本地请求挂死）。refresh 同理（provider 强制拉取的
//! 时长取决于订阅站）。
//!
//! ── decode-qr 为什么在服务端解 ──────────────────────────────
//! 二维码解码是纯逻辑（rqrr + 已有的 image 依赖），放服务端让 web 前端
//! 不必内嵌一份 JS 解码库；图片以 base64 随请求上传（本机回环，32MB 的
//! 管理面请求上限对二维码截图绰绰有余）。识别内容只接受订阅链接
//! （http/https）；单节点分享链接（vmess:// 等）明确报「暂不支持」，
//! 不静默吞掉。

use axum::body::Bytes;
use axum::response::Response;
use base64::Engine;
use serde_json::{json, Value};

use crate::server::core::sub_kernel;
use crate::server::core::subscriptions;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;

/// 列表响应：`{items, kernel}`。items 的节点数从内核快照注入（订阅存储
/// 不认识内核，见 `core::subscriptions::list` 的说明）。
fn build_payload() -> Value {
    let kernel = sub_kernel::status_json();
    let node_counts = kernel
        .get("providerSync")
        .and_then(Value::as_object)
        .map(|providers| {
            providers
                .iter()
                .filter_map(|(id, value)| {
                    value.get("count").and_then(Value::as_u64).map(|count| (id.clone(), count as usize))
                })
                .collect::<std::collections::HashMap<String, usize>>()
        })
        .unwrap_or_default();
    json!({
        "items": subscriptions::list(&node_counts),
        "kernel": kernel,
    })
}

/// 写操作 / 刷新后的统一响应（就地替换约定，与代理池写接口一致）
fn saved_payload() -> Response {
    ok_json(build_payload())
}

/// api 层编排骨架：订阅写操作之后内核该做什么。
///
/// 有启用的订阅 → 重启内核（重建 proxy-providers 与监听器，节点镜像进池）；
/// 一条启用订阅都不剩 → 停内核（留着只会空跑健康检查）。内核未安装时投
/// 命令也无害（restart 早退在「未安装」上，worker 串行消费）。
fn sync_kernel_after_change() {
    let enabled = subscriptions::enabled_provider_inputs();
    if enabled.is_empty() {
        sub_kernel::request_stop();
    } else {
        sub_kernel::request_restart(enabled);
    }
}

// ─── 入口分发 ───────────────────────────────────────────────

/// 订阅操作全部走进程级句柄（subscriptions 的库句柄 / sub_kernel 的 worker
/// 命令队列），不经过 ServerState —— 与 `core::subscriptions` 的 install
/// 形态一致，entry 也就不需要 State 提取器。
pub async fn entry(request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let full_path = request.uri().path().to_string();
    let rest = full_path
        .strip_prefix("/api/subscriptions")
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let body = match axum::body::to_bytes(request.into_body(), crate::server::http::MAX_BODY_SIZE)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => return management_error(413, format!("请求体读取失败或过大: {error}")),
    };
    match (method.as_str(), rest.as_str()) {
        ("GET", "") => ok_json(build_payload()),
        ("POST", "add") => add(&body).await,
        ("POST", "save") => save(&body).await,
        ("POST", "refresh") => refresh(&body),
        ("POST", "remove") => remove(&body).await,
        ("POST", "decode-qr") => decode_qr(&body),
        ("POST", "import-file") => import_file(&body),
        ("POST", "replace-file") => replace_file(&body).await,
        ("GET", "kernel") => ok_json(sub_kernel::status_json()),
        ("POST", "kernel/download") => kernel_download().await,
        ("POST", "kernel/restart") => kernel_restart(),
        _ => management_error(
            404,
            format!("Not found: {} {full_path}", method.as_str()),
        ),
    }
}

// ─── 订阅 CRUD ─────────────────────────────────────────────

/// POST /api/subscriptions/add
///
/// 两种形态：链接订阅 `{name, url}`；本地配置文件
/// `{name, kind:"file", sourcePath, content}` —— 后者先校验 YAML 再落库
/// （`content` 是壳读出来的原文，见模块头）。
async fn add(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    // 本地配置：先建条目（拿 id）→ 写文件 → 再落库。顺序是有意的 ——
    // 写盘失败时不留一条「看起来导入成功、内核却读不到」的条目。
    if payload.get("kind").and_then(Value::as_str) == Some("file") {
        return add_file(&payload).await;
    }
    match subscriptions::create(&payload) {
        Ok(_) => {
            sync_kernel_after_change();
            saved_payload()
        }
        Err(message) => management_error(400, message),
    }
}

/// 本地配置文件的导入：校验 → 建条目 → 写内核目录 → 重启内核。
///
/// 校验在写盘**之前**（理由见模块头）；条目在写盘**之前**建是因为文件要落在
/// `sub_<id>.yaml`（id 由 `subscriptions::create` 生成）。中间失败时把刚建的
/// 条目回删 —— 宁可让用户重新导入一次，也不要留一条读不到内容的订阅。
async fn add_file(payload: &Value) -> Response {
    let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
    if let Err(message) = validate_node_yaml(content) {
        return management_error(400, message);
    }
    let items = match subscriptions::create(payload) {
        Ok(items) => items,
        Err(message) => return management_error(400, message),
    };
    // 定位刚建的条目：按 `sourcePath` 找（`create` 的返回值里 id 不固定位置，
    // 且这个字段对 file 条目唯一 —— 重复路径在 create 里已被拒）。
    let source_path = payload
        .get("sourcePath")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(id) = items
        .iter()
        .find(|item| item.get("sourcePath").and_then(Value::as_str) == Some(source_path))
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return management_error(500, "订阅条目创建后未返回 id");
    };
    if let Err(error) = sub_kernel::write_provider_file(&id, content) {
        let _ = subscriptions::remove(&id);
        return management_error(500, error);
    }
    logging::log(
        "[Subscription]",
        &format!(
            "📄 已导入本地配置文件「{}」（{}）",
            payload.get("name").and_then(Value::as_str).unwrap_or(""),
            payload.get("sourcePath").and_then(Value::as_str).unwrap_or("")
        ),
    );
    sync_kernel_after_change();
    saved_payload()
}

/// POST /api/subscriptions/save
async fn save(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match subscriptions::update(&payload) {
        Ok(_) => {
            sync_kernel_after_change();
            saved_payload()
        }
        Err(message) => management_error(400, message),
    }
}

/// POST /api/subscriptions/refresh（强制拉取订阅内容；节点变了自动重启内核）
///
/// 本地配置文件（`kind: "file"`）在这里**多一步**：先把原路径的文件重新读
/// 一遍写进内核目录，再让内核重读 —— 用户的「立即更新」对本地文件的含义就是
/// 「我改了那个文件，重新加载」。原路径不可读时明确报错，不静默用旧内容。
fn refresh(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少订阅 id");
    }
    // 「不存在」与「被禁用」分流报错（见 subscriptions::find_enabled_state 的说明）
    match subscriptions::find_enabled_state(id) {
        None => {
            return management_error(
                404,
                format!("订阅「{id}」不存在（可能已被删除，刷新列表后重试）"),
            )
        }
        Some(false) => return management_error(400, "该订阅已禁用（先启用再刷新）"),
        Some(true) => {}
    }
    if let Some(item) = subscriptions::find(id) {
        if subscriptions::kind_of(&item) == subscriptions::KIND_FILE {
            if let Err(message) = reimport_local_file(&item) {
                return management_error(400, message);
            }
        }
    }
    sub_kernel::request_refresh(id.to_string(), subscriptions::enabled_provider_inputs());
    saved_payload()
}

/// 把本地配置文件的**原路径**重新读一遍写进内核目录。
///
/// 壳（桌面端）在导入时把路径存进条目；原路径缺失（headless 面板导入的、
/// 或用户在壳之外删了路径字段）时退回「让内核重读现有文件」—— 那条路仍然
/// 有意义（内核目录里那份文件可能被外部工具改过），只是不报「已重新导入」。
fn reimport_local_file(item: &Value) -> Result<(), String> {
    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
    let path = item.get("sourcePath").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("重新读取本地配置文件失败（{path}）: {error}"))?;
    validate_node_yaml(&content)?;
    sub_kernel::write_provider_file(id, &content)?;
    Ok(())
}

/// 校验一份「节点 YAML」能不能给内核当 file provider 用。
///
/// 三条硬要求（缺一条内核就起不来或 provider 为空）：能解析成 YAML、顶层是
/// 映射、`proxies` 是**非空**列表。**只认这一个形状**：机场订阅那种 base64
/// 文本、单节点分享链接（vmess:// 等）都明确拒绝 —— 它们要经过完整的协议
/// 解析与转换，不是「多认一种输入」能解决的（错在这里报出来，比让内核静默
/// 起不来好排查得多）。
fn validate_node_yaml(content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("配置文件是空的".to_string());
    }
    let parsed: serde_yaml::Value = serde_yaml::from_str(content)
        .map_err(|error| format!("配置文件不是有效 YAML: {error}"))?;
    if !parsed.is_mapping() {
        return Err("配置文件顶层必须是映射（含 proxies 列表的 Clash / mihomo YAML）".to_string());
    }
    let Some(proxies) = parsed.get("proxies") else {
        return Err("配置文件里没有 proxies 列表（需要 Clash / mihomo 的节点配置）".to_string());
    };
    let Some(proxies) = proxies.as_sequence() else {
        return Err("配置文件里的 proxies 必须是列表".to_string());
    };
    if proxies.is_empty() {
        return Err("配置文件里的 proxies 是空列表，没有可用节点".to_string());
    }
    Ok(())
}

/// POST /api/subscriptions/remove
async fn remove(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少订阅 id");
    }
    match subscriptions::remove(id) {
        Ok((_, name)) => {
            // 缓存文件跟着删：链接订阅的缓存与本地配置的 YAML 都在那里，
            // 留着只会在下次导入同名文件时变成「上一次的内容」
            sub_kernel::remove_provider_file(id);
            sync_kernel_after_change();
            let mut data = build_payload();
            if let Some(object) = data.as_object_mut() {
                object.insert("removed".to_string(), Value::String(name));
            }
            ok_json(data)
        }
        Err(message) => management_error(400, message),
    }
}

// ─── 二维码解码 ─────────────────────────────────────────────

/// 单节点分享链接的 scheme（识别到这些 → 明确报「暂不支持」而不是笼统的
/// 「不是订阅链接」，把排查方向点明）
const NODE_SCHEMES: &[&str] = &[
    "ss://", "ssr://", "vmess://", "vless://", "trojan://", "hysteria://", "hysteria2://",
    "hy2://", "tuic://", "socks://", "socks5://",
];

/// POST /api/subscriptions/decode-qr —— 从图片解二维码。
///
/// `imageBase64` 接受裸 base64 或 data URL（`data:image/png;base64,...`）。
/// 图里可能不止一个码（长截图），逐个试，优先取 http/https 的内容。
fn decode_qr(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let raw = payload
        .get("imageBase64")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return management_error(400, "缺少图片数据");
    }
    // data URL 前缀剥掉（`base64,` 之后才是正文）
    let encoded = raw.split_once("base64,").map(|(_, rest)| rest).unwrap_or(raw);
    let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(bytes) => bytes,
        Err(_) => return management_error(400, "图片数据不是有效的 base64"),
    };
    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(_) => return management_error(400, "图片格式无法识别（支持 PNG / JPEG / WebP）"),
    };
    let mut prepared = rqrr::PreparedImage::prepare(img.to_luma8());
    let grids = prepared.detect_grids();
    if grids.is_empty() {
        return management_error(400, "图片里没有识别到二维码");
    }
    let mut first_content: Option<String> = None;
    for grid in &grids {
        let Ok((_, content)) = grid.decode() else {
            continue;
        };
        let lower = content.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return ok_json(json!({ "url": content }));
        }
        if NODE_SCHEMES.iter().any(|scheme| lower.starts_with(scheme)) {
            let scheme = content.split("://").next().unwrap_or("").to_string();
            return management_error(
                400,
                format!("二维码里是单节点分享链接（{scheme}://），暂不支持，请使用订阅链接"),
            );
        }
        if first_content.is_none() {
            first_content = Some(content);
        }
    }
    match first_content {
        Some(content) => management_error(400, format!("二维码内容不是订阅链接：{}", content)),
        None => management_error(400, "二维码无法解码（可能是图片太模糊或码不完整）"),
    }
}

// ─── 本地配置文件导入 ────────────────────────────────────────

// ─── 本地配置文件导入 ────────────────────────────────────────

/// POST /api/subscriptions/replace-file —— 本地文件订阅**原地替换**内容。
///
/// 与「删掉重建」的差别是**条目 id 不变**：id 是 mihomo provider 名与账号
/// 引用链的一环（代理池的订阅节点镜像按 `providerId + node` 定位并沿用条目 id，
/// 账号又按池条目 id 引用），换 id 会让所有引用悬空、节点镜像重建。
/// 所以「换了台机器、配置文件搬了个位置」不该付出这个代价。
///
/// 入参 `{id, content, path?}`：`content` 是新文件原文；`path` 给了就更新
/// `sourcePath`（之后「重新导入」从新位置读），不给就保留旧路径（内容已换、
/// 位置未变的场景）。写盘成功后才落库 —— 写失败时条目保持原样（指向旧内容），
/// 不出现「库里说换了、内核读的还是旧文件」。
async fn replace_file(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少订阅 id");
    }
    let Some(item) = subscriptions::find(id) else {
        return management_error(404, format!("订阅「{id}」不存在（可能已被删除）"));
    };
    if subscriptions::kind_of(&item) != subscriptions::KIND_FILE {
        return management_error(400, "只有本地配置文件订阅可以原地替换内容");
    }
    let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
    if let Err(message) = validate_node_yaml(content) {
        return management_error(400, message);
    }
    // 换了文件位置时先查重（在写盘**之前**）：“同一个路径只能有一条订阅”这条
    // 规则在 `subscriptions::update` 里也有，但那是在写盘之后 —— 到那时才报错，
    // 内核目录里已经是新内容、库里的路径还是旧的，两者不一致。
    let new_path = payload
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if let Some(path) = new_path {
        if subscriptions::file_path_taken(path, id) {
            return management_error(400, "该本地配置文件已导入过".to_string());
        }
    }
    if let Err(error) = sub_kernel::write_provider_file(id, content) {
        return management_error(500, error);
    }
    // 更新条目：换了位置就改 `sourcePath`，其它字段原样保留。
    // 用 `update`（整份覆盖）而不是再开一个写口：条目的归一 / 去重规则只有一份。
    let update_payload = json!({
        "id": id,
        "name": item.get("name").cloned().unwrap_or(Value::String(String::new())),
        "kind": subscriptions::KIND_FILE,
        "sourcePath": new_path
            .map(str::to_string)
            .unwrap_or_else(|| item.get("sourcePath").and_then(Value::as_str).unwrap_or("").to_string()),
        "enabled": item.get("enabled").cloned().unwrap_or(Value::Bool(true)),
    });
    if let Err(message) = subscriptions::update(&update_payload) {
        // 写盘已成功、落库失败：把盘上的内容回写成条目当前指向的那份
        // （从原路径重新读一次），保持「库与内核目录一致」。读不回来时
        // 只能如实报错 —— 那种情况用户需要知道盘上已经变了。
        let rolled_back = rollback_provider_file(&item);
        let hint = if rolled_back {
            "（已回退内核目录里的文件）"
        } else {
            "（注意：内核目录里的文件已更新，但订阅记录未变）"
        };
        return management_error(400, format!("{message}{hint}"));
    }
    logging::log(
        "[Subscription]",
        &format!(
            "📄 已原地替换本地配置文件「{}」的内容{}",
            item.get("name").and_then(Value::as_str).unwrap_or(""),
            new_path.map(|path| format!("（新路径 {path}）")).unwrap_or_default()
        ),
    );
    sub_kernel::request_refresh(id.to_string(), subscriptions::enabled_provider_inputs());
    saved_payload()
}

/// 回退内核目录里那份文件：从条目当前的 `sourcePath` 重新读一次写回去。
///
/// 只服务「写盘成功、落库失败」这一条窄路径（正常流程用不到）。读不回来
/// （原路径已不可读 / 没记路径）时返回 false，调用方据此把话说清楚。
fn rollback_provider_file(item: &Value) -> bool {
    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
    let path = item.get("sourcePath").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() || path.is_empty() {
        return false;
    }
    match std::fs::read_to_string(path) {
        Ok(previous) => sub_kernel::write_provider_file(id, &previous).is_ok(),
        Err(_) => false,
    }
}

/// POST /api/subscriptions/import-file —— 壳弹选文件框后把内容转发过来。
///
/// 入参 `{path, content, name?}`：`path` 是原文件绝对路径（仅本机壳能给，
/// 用于「立即更新」重新导入与界面显示）；`content` 是文件原文；`name` 不给
/// 时用文件名当订阅名。本接口**只读文件内容并回显元信息**，不落库 —— 落库
/// 走 `/add`（`kind:"file"`），因为导入弹窗里用户还要改名字、可能还要先看看
/// 有几条节点。这样也保证「表单提交」只有一条写路径。
fn import_file(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
    let path = payload
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if let Err(message) = validate_node_yaml(content) {
        return management_error(400, message);
    }
    // 节点数：拿列表长度告诉用户「这份文件里有几条」（不是内核拉到的数量，
    // 只是文件里声明的条数 —— 文案上要区分开）
    let declared = serde_yaml::from_str::<serde_yaml::Value>(content)
        .ok()
        .and_then(|parsed| parsed.get("proxies").and_then(|list| list.as_sequence()).map(Vec::len))
        .unwrap_or(0);
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(&path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "本地配置".to_string());
    ok_json(json!({
        "path": path,
        "name": name,
        "declaredNodes": declared,
        "bytes": content.len(),
    }))
}

// ─── 内核操作 ───────────────────────────────────────────────

/// POST /api/subscriptions/kernel/download（后台下载，进度轮询看）
async fn kernel_download() -> Response {
    if sub_kernel::installed() {
        return ok_json(sub_kernel::status_json());
    }
    sub_kernel::request_download(subscriptions::enabled_provider_inputs());
    logging::log("[Subscription]", "⬇️ 开始下载代理内核");
    ok_json(sub_kernel::status_json())
}

/// POST /api/subscriptions/kernel/restart（手动启动 / 重启；先落日志便于排障）
fn kernel_restart() -> Response {
    let enabled = subscriptions::enabled_provider_inputs();
    if enabled.is_empty() {
        return management_error(400, "还没有启用的订阅（先添加一条订阅再启动内核）");
    }
    sub_kernel::request_restart(enabled);
    ok_json(sub_kernel::status_json())
}

#[cfg(test)]
mod import_file_tests {
    use super::validate_node_yaml;

    /// 合法的 Clash / mihomo 节点 YAML 通过
    #[test]
    fn a_proxies_list_is_accepted() {
        let ok = "proxies:\n  - name: hk-1\n    type: ss\n    server: 1.2.3.4\n    port: 443\n";
        assert!(validate_node_yaml(ok).is_ok());
        // 真实配置文件里常见的其它顶层键不影响判定
        let with_extras = "mixed-port: 7890\nproxies:\n  - {name: hk-1, type: ss}\nrules:\n  - MATCH,DIRECT\n";
        assert!(validate_node_yaml(with_extras).is_ok());
    }

    /// 三种不该收的形态各给一句能指向文件本身的话
    #[test]
    fn empty_broken_or_nodeless_files_are_rejected_with_a_reason() {
        let empty = validate_node_yaml("   ").unwrap_err();
        assert!(empty.contains("空"), "{empty}");
        let broken = validate_node_yaml("proxies: [").unwrap_err();
        assert!(broken.contains("YAML"), "{broken}");
        // 机场订阅常见的 base64 文本：不是 YAML 映射
        let base64 = validate_node_yaml("c3M6Ly9ZV1Z6TFRJMU5pMW5ZMjA9").unwrap_err();
        assert!(base64.contains("YAML") || base64.contains("映射"), "{base64}");
        let no_list = validate_node_yaml("mixed-port: 7890\n").unwrap_err();
        assert!(no_list.contains("proxies"), "{no_list}");
        let not_a_list = validate_node_yaml("proxies: {}\n").unwrap_err();
        assert!(not_a_list.contains("列表"), "{not_a_list}");
        let empty_list = validate_node_yaml("proxies: []\n").unwrap_err();
        assert!(empty_list.contains("空列表"), "{empty_list}");
    }

    /// 原地替换的路径查重只服务 file 条目、且排除自己 —— 与 `create` / `update`
    /// 里的去重口径同一份（`subscriptions::file_path_taken`）。
    #[test]
    fn replacing_a_file_checks_the_new_path_but_not_itself() {
        use crate::server::core::subscriptions;
        use crate::server::db::test_temp::TempDb;
        use serde_json::json;

        let (db, _guard) = TempDb::open("subscription-replace");
        subscriptions::install(Some(db));

        let items = subscriptions::create(&json!({
            "name": "本机配置",
            "kind": "file",
            "sourcePath": "C:/a/nodes.yaml",
        }))
        .expect("本地文件订阅应能建");
        let id = items
            .iter()
            .find(|item| item["sourcePath"] == json!("C:/a/nodes.yaml"))
            .and_then(|item| item["id"].as_str())
            .expect("应有 id")
            .to_string();

        // 自己占用的路径不算「被占」（原地替换保持原路径是常态）
        assert!(!subscriptions::file_path_taken("C:/a/nodes.yaml", &id));
        // 换成一条别人已在用的路径要挡住
        subscriptions::create(&json!({
            "name": "另一份",
            "kind": "file",
            "sourcePath": "C:/b/nodes.yaml",
        }))
        .expect("第二份应能建");
        assert!(subscriptions::file_path_taken("C:/b/nodes.yaml", &id));
        // 链接订阅不参与这条判据（它们按 url 去重）
        assert!(!subscriptions::file_path_taken("https://example.invalid/sub", &id));
    }
}

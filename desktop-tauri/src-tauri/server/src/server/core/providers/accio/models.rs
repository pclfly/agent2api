//! Accio 模型目录：静态兜底 + `/api/llm/config` 远程刷新。
//!
//! ── 清单从哪来 ──────────────────────────────────────────────
//!   1. **静态兜底**：`FALLBACK`（11 个可见模型，抄自客户端内置的
//!      `@ali/accio-adk-ts/model-catalog.json`，2026-09 快照）。进程启动即可用，
//!      没有账号 / 网络不通时 `/v1/models` 仍有内容。
//!   2. **远程刷新**：`POST {gw}/api/llm/config`，body `{token}`，
//!      头带 `x-package-region`（GLOBAL / CN）。上游按地区与账号权限给清单，
//!      并支持 `If-None-Match`（我们不用 304 缓存：本网关自己按 TTL 早退，
//!      少一条「快照与 etag 对不上」的状态）。
//!
//! ── 上游响应形态（2026-09 实测，解析必须按这一份）──────────────
//! ```jsonc
//! { "success": true, "code": "200", "message": "0",
//!   "data": [                       // ← **provider 数组**，不是 {providers:[…]}
//!     { "provider": "openai", "providerDisplayName": "OpenAI",
//!       "modelList": [ { "modelCode": "1Helix-G6aS8tR2qN7m", "modelName": "…",
//!                        "modelDisplayName": "GPT 6 Astra", "visible": true,
//!                        "protocol": "responses", "group": "azure", … } ] } ] }
//! ```
//! 早先的解析假设 `data` 是 `{providers:[…]}` **对象**，而它其实是数组 ——
//! 于是 `root.providers` 取不到、清单恒为空，而 HTTP 是 200（走不到错误分支），
//! 前端只看到「上游目录里没有可见模型」。数组与对象两种形态都接受。
//!
//! ── 三个名字各归其位（2026-10-05 实测复核）────────────────────
//! 上游每条模型给三个字段：
//!   - `modelCode`（`1Orbit-I9eY7YK8bW1f`）—— 不透明代号，**发给上游**用它；
//!   - `modelName`（`gpt-6-astra` / `claude-sonnet-4-6` / `qwen3.8-max@sg`）——
//!     模型标识，**官方客户端请求里用的就是它**（实测也能直接发上游）；
//!   - `modelDisplayName`（`GPT 6 Astra` / `Standard` / `专业` / `极致`）——
//!     **只给界面看**的营销名，含中文，不是模型 id。
//! 因此：`upstreamKey` 存 `modelCode`（转发永远用当前代号），对外 `id` 用
//! `modelName`（见 [`stable_id`]）。**不要**拿 `modelDisplayName` 当 id ——
//! 那会让本网关的清单与官方客户端对不上，还会把中文营销名广告成模型 id。
//!
//! ── 条目的键名口径（与另外几家对齐）──────────────────────────
//! 聚合层的 `models::list_item` 认 `id` / `name` / `maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning` / `supportsToolCall` /
//! `isDefault` / `kind`（见 `core::models::shape`）。上游给的是
//! `modelCode` / `modelDisplayName` / `contextWindow` / `multimodal` /
//! `reasoningEfforts` / `isDefault`，本模块**在归一函数里一次翻译到位**，
//! 让 `list()` 的返回值直接就是聚合层认识的形态（并保留 `upstreamKey`
//! 与 `reasoningEfforts` 两个自有键 —— `list_item` 只挑它认识的键，
//! 这两个不会被带进 `/v1/models`，但路由与思考档位要用）。
//!
//! ── 两个地区各自的清单（与 Qoder 同一处置）────────────────────
//! GLOBAL 与 CN 的目录**可能不同**（上游按 `x-package-region` 过滤），
//! 因此缓存按地区分开；`list()` 返回并集（按 id 去重，GLOBAL 优先）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁期间绝不做网络请求。

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::core::providers::catalog_cache;
use crate::server::logging;

use super::auth;
use super::credentials::Credentials;
use super::endpoints::{self, Region};

/// 远程目录缓存有效期（与 Qoder 的 1 小时同量级：目录变化不频繁，
/// 但用户点「刷新模型」时必须能真打上游 —— `force` 会绕过这里）
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// 静态兜底：客户端内置的 `model-catalog.json` 里 `visible: true` 的条目。
///
/// 字段名已经是**归一后的形态**（见模块头），`upstreamKey` 是发给上游的
/// `model` 字段值（就是 `modelCode` —— Accio 不做通道前缀那一套）。
///
/// ── 这张表现在只服务「没有账号 / 拉不到目录」的时刻 ─────────────
/// 条目名是**代号之前**的那一代，上游接口现在仍然认（逐条实测：除
/// `kimi-k2.5` 已被上游摘掉、**故已从本表删除**之外，其余都还能发）。
/// 保留它的作用是进程启动即有清单可广告、断网 / 未登录时 `/v1/models` 不为空。
/// 远程清单拉到之后这份就退居兜底。
///
/// ⚠️ 元组第一列是**对外 id**，必须与远程清单里同一个模型的 `modelName` 一致
/// （见 [`stable_id`]）—— 兜底与远程两条路给出同一个名字，用户配置才不会因为
/// 「这次拉到了 / 这次没拉到」而 404。第三列才是 `upstreamKey`。
///
/// `EffortPlacement` 一列是思考档位落点（实测口径见 [`EffortPlacement`]）：
/// 静态表里没有上游的 `protocol` 字段，所以按同一批实测逐条写死 ——
/// 远程清单走「读 `protocol`」，两条路都不再按模型名猜。
const FALLBACK: &[(&str, &str, &str, u64, bool, &[&str], bool, EffortPlacement)] = &[
    // (对外 id = 上游 modelName, upstreamKey = 上游 modelCode, 展示名, contextWindow,
    //  multimodal, reasoningEfforts, isDefault, 档位落点)
    ("gemini-3-flash-preview", "gemini-3-flash-preview", "Gemini 3 Flash", 1_000_000, true, &["low", "high"], true, EffortPlacement::Top),
    ("gemini-3.1-pro-preview", "gemini-3.1-pro-preview", "Gemini 3.1 Pro", 1_000_000, true, &["low", "high"], false, EffortPlacement::Top),
    ("qwen3.6-plus", "qwen3.6-plus", "Qwen 3.6 Plus", 991_808, false, &[], false, EffortPlacement::Top),
    ("qwen3-max-2026-01-23", "qwen3-max-2026-01-23", "Qwen 3 Max", 262_144, false, &[], false, EffortPlacement::Top),
    ("gpt-5.4", "gpt-5.4", "GPT 5.4", 1_050_000, true, &["low", "high"], false, EffortPlacement::Properties),
    ("gpt-5.2-1211", "gpt-5.2-1211", "GPT 5.2", 400_000, true, &["low", "high"], false, EffortPlacement::Properties),
    ("claude-sonnet-4-6", "claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false, EffortPlacement::Top),
    ("claude-opus-4-6", "claude-opus-4-6", "Claude Opus 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false, EffortPlacement::Top),
    ("glm-5", "glm-5", "GLM-5", 200_000, false, &[], false, EffortPlacement::Top),
    ("MiniMax-M2.5", "MiniMax-M2.5", "MiniMax M2.5", 204_800, true, &[], false, EffortPlacement::Properties),
];

/// 某条目录条目的**对外 id** = 上游的 `modelName`。
///
/// ── 为什么是 `modelName` 而不是展示名（2026-10-05 实测）──────────
/// 上游每条模型给三个名字，各有分工：
///   · `modelCode`（`1Helix-G6aS8tR2qN7m`）—— **发给上游**的串（会换）；
///   · `modelName`（`gpt-6-astra` / `claude-sonnet-4-6`）—— 模型标识，官方
///     客户端请求里用的就是它，实测也能直接发上游；
///   · `modelDisplayName`（`GPT 6 Astra` / `Standard` / `极致`）—— **只给界面看**。
///
/// 本模块原来拿 `modelDisplayName` 当对外 id，于是同一份清单在两边长得不一样：
/// 官方客户端里是 `gpt-6-astra`，到本网关变成 `GPT 6 Astra`；`@sg` 这类区域变体
/// 还要靠拼展示名后缀才分得开（`Qwen 3.8 Max @sg`）；更要命的是上游给某些条目
/// 的展示名是**中文营销词**（`Standard` / `专业` / `极致`），列表里就会冒出
/// `极致` 这种既不像模型名、也无法从上游文档查到的 id。
///
/// 改用 `modelName` 之后：对外名 = 官方客户端里那个名字 = 能直接发上游的串，
/// 一处对齐三处（列表、配置、上游），也不再需要「拼后缀消歧」。
///
/// `modelName` 缺失时才回退展示名（上游将来少给字段时不至于没名字）。
fn stable_id(code: &str, display: &str, model_name: &str) -> String {
    let name = model_name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let display = display.trim();
    if !display.is_empty() {
        return display.to_string();
    }
    code.to_string()
}

/// 一个地区的远程清单 + 拉取时刻
#[derive(Clone, Default)]
struct RegionCache {
    models: Vec<Value>,
    fetched_at: i64,
}

/// 按地区取那一格目录缓存。首次初始化时**先从持久化缓存恢复**（上次成功
/// 拉到的远程清单），没有再留空 —— 空状态的读取语义就是「回落到静态兜底」。
///
/// 两格互不覆盖的理由与 AutoClaw 那边相同：上游按 `x-package-region` 给的是
/// **两份清单**，共用一个格子会让两家互相覆盖，而时间戳还显示「刚刚更新」。
fn cache_slot(region: Region) -> &'static RwLock<RegionCache> {
    static SLOTS: OnceLock<[RwLock<RegionCache>; 2]> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| {
        [
            RwLock::new(restored_cache(Region::Global)),
            RwLock::new(restored_cache(Region::Cn)),
        ]
    });
    let index = match region {
        Region::Global => 0,
        Region::Cn => 1,
    };
    &slots[index]
}

/// 地区 → 持久化缓存的 scope（两格各一条，互不覆盖）
fn cache_scope(region: Region) -> &'static str {
    match region {
        Region::Global => catalog_cache::SCOPE_ACCIO_GLOBAL,
        Region::Cn => catalog_cache::SCOPE_ACCIO_CN,
    }
}

/// 首次初始化读一次持久化缓存（见 `providers::catalog_cache` 的模块头）。
///
/// 缓存里存的就是 `RegionCache` 的形态（`refresh` 落地的那份），所以这里只做
/// 「搬回来」：不重新归一 —— 两处各写一份映射迟早分叉。
fn restored_cache(region: Region) -> RegionCache {
    match catalog_cache::load(cache_scope(region)) {
        Some(cached) => RegionCache { models: cached.models, fetched_at: cached.fetched_at },
        None => RegionCache::default(),
    }
}

fn fallback_models() -> Vec<Value> {
    FALLBACK
        .iter()
        .map(|(id, code, name, window, multimodal, efforts, is_default, placement)| {
            let mut item = normalize_entry(&json!({
                // `modelName` 就是对外 id（见 `stable_id`）：兜底与远程两条路
                // 必须给出同一个名字，用户配置才不会因为「这次没拉到目录」而 404
                "modelName": id,
                "modelCode": code,
                "modelDisplayName": name,
                "contextWindow": window,
                "multimodal": multimodal,
                "reasoningEfforts": efforts,
                "isDefault": is_default,
            }));
            // 静态表自己带着落点（没有上游的 `protocol` 可读），写进去覆盖
            // `normalize_entry` 的兜底值
            if let Some(object) = item.as_object_mut() {
                object.insert(
                    "protocolKind".to_string(),
                    Value::String(placement.as_str().to_string()),
                );
            }
            item
        })
        .collect()
}

/// 远程条目的归一（上游字段 → 聚合层认识的键名）。
fn normalize_entry(entry: &Value) -> Value {
    let code = entry
        .get("modelCode")
        .or_else(|| entry.get("modelName"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let model_name = entry
        .get("modelName")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let display = entry
        .get("modelDisplayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&code)
        .to_string();
    let context_window = entry
        .get("contextWindow")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let multimodal = entry.get("multimodal").and_then(Value::as_bool).unwrap_or(false);
    let efforts: Vec<Value> = entry
        .get("reasoningEfforts")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(|text| Value::String(text.to_string())).collect())
        .unwrap_or_default();
    json!({
        // 对外 id = 上游的 `modelName`（见 `stable_id`）；`name` 才是展示名
        "id": stable_id(&code, &display, &model_name),
        "name": display,
        "maxInputTokens": context_window,
        // 上游没给输出上限；与 contextWindow 同量级地给一个保守值会让界面误导，
        // 因此留空（`list_item` 在缺键时不输出这个字段）
        "supportsImages": multimodal,
        "supportsReasoning": !efforts.is_empty(),
        // ADK 信封支持 functionCall / functionResponse，工具调用一律可用
        "supportsToolCall": true,
        "isDefault": entry.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
        "kind": "chat",
        // 区域可用性（上游给的 cnAvailable / enAvailable）：同一展示名的
        // 多区域变体靠它区分（如 qwen3.8-max 的 cn 版与 @sg 版）
        "cnAvailable": entry.get("cnAvailable").and_then(Value::as_bool).unwrap_or(false),
        "enAvailable": entry.get("enAvailable").and_then(Value::as_bool).unwrap_or(false),
        // 自有键（进不了 /v1/models，路由与思考档位要用）
        "upstreamKey": code,
        "reasoningEfforts": Value::Array(efforts),
        // 思考档位落点，由目录里的 `protocol` 决定（见 `EffortPlacement`）
        "protocolKind": effort_placement(entry).as_str(),
        "modelDesc": entry.get("modelDesc").cloned().unwrap_or(Value::Null),
        "usageMultiple": entry.get("usageMultiple").cloned().unwrap_or(Value::Null),
    })
}

/// 思考档位该放哪一层（`properties.reasoning_effort` 还是顶层 `reasoning_effort`）。
///
/// ── 为什么不能按模型名猜（2026-09 实测）───────────────────────
/// 上游把模型名换成了不透明代号（`1Helix-…` / `1Orbit-…`），
/// `contains("gpt")` 这种判据全部失效。改用上游自己在目录里给的
/// `protocol` 字段：
///
/// | `protocol`  | 例子           | 档位落点   | 依据（实测）                              |
/// |-------------|----------------|------------|-------------------------------------------|
/// | `responses` | GPT 系         | properties | 只有 properties 能出思考内容（80 帧 thought，顶层 0） |
/// | `openai`    | MiniMax M3     | properties | 同上（`openai` 沿同一落点）                |
/// | 空          | Claude / Gemini 系 | 顶层    | Gemini 系放 properties **直接 400**：`Unknown name "reasoning_effort"` |
///
/// 兜底取「顶层」：它是唯一在三种模型上都不会报错的落点（Gemini 系放
/// properties 是硬错误，放顶层只是不产生思考内容 —— 少个功能好过 400）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffortPlacement {
    /// `properties.reasoning_effort`（`protocol` = `responses` / `openai`）
    Properties,
    /// 顶层 `reasoning_effort`（`protocol` 为空或未知）
    Top,
}

impl EffortPlacement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Properties => "properties",
            Self::Top => "top",
        }
    }

    /// 从条目上的自有键读回落点（发送侧用）。缺键 / 值不认识 → `Top`
    /// （兜底口径与 [`effort_placement`] 一致）。
    pub fn from_entry(entry: &Value) -> Self {
        match entry.get("protocolKind").and_then(Value::as_str) {
            Some("properties") => Self::Properties,
            _ => Self::Top,
        }
    }
}

/// 上游模型条目 → 思考档位落点（见 [`EffortPlacement`]）。
///
/// 认的是目录里的 `protocol`（上游自己声明的协议）；`group`
/// （`azure` / `google` / `dashscope` …）是执行池、`thinkLevel` 是另一回事，
/// 都不参与这条判定。
fn effort_placement(entry: &Value) -> EffortPlacement {
    match entry
        .get("protocol")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "responses" | "openai" => EffortPlacement::Properties,
        _ => EffortPlacement::Top,
    }
}

/// 当前清单：远程（有的话）优先，**一个地区都没有远程清单时**才用兜底。
///
/// ── 为什么不是「这一格空了就用兜底」（2026-10-05 修）──────────
/// 两格各自回落兜底，会在「只有一边刷到过」时把兜底**混进**并集：实测国内版
/// 那格没刷过（账号只走 GLOBAL），并集里就多出 5 条上游已经下架 / 已标
/// `visible: false` 的旧名字（`qwen3.6-plus` / `qwen3-max-2026-01-23` /
/// `gpt-5.4` / `gpt-5.2-1211` / `MiniMax-M2.5`）—— 用户点了必然失败，而清单
/// 看着「有」。兜底清单的职责是「进程刚起来 / 断网 / 没账号时列表不为空」，
/// 一旦拿到过任何一份远程清单，就不该再有它的位置：清单以目录为准。
fn region_models(region: Region) -> Vec<Value> {
    let cached = {
        let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
        guard.models.clone()
    };
    if !cached.is_empty() {
        return cached;
    }
    let any_remote = Region::ALL.iter().any(|other| {
        let guard = cache_slot(*other).read().unwrap_or_else(|error| error.into_inner());
        !guard.models.is_empty()
    });
    if any_remote {
        Vec::new()
    } else {
        fallback_models()
    }
}

/// 全部地区的并集（GLOBAL 优先；同 id 去重）。聚合目录与 `resolve` 都用它。
pub fn list() -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for region in Region::ALL {
        for item in region_models(region) {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if merged.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                continue;
            }
            merged.push(item);
        }
    }
    merged
}

/// 某地区最近一次远程刷新是否成功过（模型管理页「来源」列用）
pub fn remote_models(region: Region) -> Vec<Value> {
    let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
    guard.models.clone()
}

/// 某地区最近一次刷新的时刻（毫秒；0 = 从未）
pub fn last_refreshed_at(region: Region) -> i64 {
    let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
    guard.fetched_at
}

/// 客户端点名的模型 → 上游发送名。
///
/// 与聚合目录同一口径（先 `id` 再 `name`，trim + 忽略大小写），否则会出现
/// 「目录放行、转发层认不出」的静默失配（与 AutoClaw 那次同一个坑）。
///
/// ── 三个键都要认 ────────────────────────────────────────────
/// `id`（= 上游 `modelName`，用户与官方客户端用的名字）、`name`（展示名，
/// 用户在列表里看到的名字）、`upstreamKey`（上游代号，客户端从桌面端
/// 抓包抄过来的那种输入）—— 三条都命中同一个条目，少认一条就是
/// 「列表里有、请求时 404」。展示名那一条在 id 改成 `modelName` 之后
/// 尤其不能省：老配置里可能写的是 `GPT 6 Astra` 这类展示名。
pub fn resolve(model_name: &str, region: Region) -> Option<Value> {
    let wanted = model_name.trim();
    if wanted.is_empty() {
        return None;
    }
    let candidates = {
        let own = region_models(region);
        let mut merged = own;
        for item in list() {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if !merged.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                merged.push(item);
            }
        }
        merged
    };
    candidates
        .iter()
        .find(|item| {
            ["id", "name", "upstreamKey"].iter().any(|key| {
                item.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case(wanted))
            })
        })
        .cloned()
}

/// 公开的重查入口（自检 / 路由失败时的文案用）
pub fn known_names() -> Vec<String> {
    list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 远程刷新（`POST /api/llm/config`，失败回落 `/api/llm/config/v2`）。
///
/// `force = false` 时按 TTL 早退（自动路径：客户端每次拉模型列表都真打上游既
/// 没必要也招风控）；`force = true` 是用户点「刷新模型清单」的语义，真打一次。
///
/// `proxy` 是账号级出口（由调用方从账号记录解析好，见 `mod.rs` 的
/// `refresh_models`）；`None` = 直连。
pub async fn refresh(
    credentials: &Credentials,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    let region = credentials.region;
    let fetched_at = last_refreshed_at(region);
    if !force && fetched_at > 0 && logging::now_ms() - fetched_at < CACHE_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    // v1 是完整清单（见 `endpoints::MODEL_CONFIG_PATH`），v2 只在它出意外时
    // 兜一次底 —— 两条路径的信封与解析完全一样，差别只在返回多少模型。
    let body = json!({ "token": credentials.access_token, "supportAutoModel": true });
    let mut last_error: Option<String> = None;
    let mut models: Vec<Value> = Vec::new();
    for path in [endpoints::MODEL_CONFIG_PATH, endpoints::MODEL_CONFIG_PATH_V2] {
        let response = match auth::post_json(region, path, &body, proxy).await {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(format!("目录请求失败：{}", error.message));
                continue;
            }
        };
        if !response.ok {
            last_error = Some(format!(
                "上游返回 HTTP {}（{}）",
                response.status,
                if response.status == 401 || response.status == 403 {
                    "凭证可能已失效，请重新登录"
                } else {
                    "请稍后重试"
                }
            ));
            continue;
        }
        let Some(payload) = response.payload else {
            last_error = Some("上游未返回有效 JSON".to_string());
            continue;
        };
        let parsed = parse_catalog(&payload);
        if parsed.is_empty() {
            last_error = Some("上游目录里没有可见模型".to_string());
            continue;
        }
        // 兜底那条路径成功时留一行痕：它意味着 v1 那边出了问题（上游改版 /
        // 灰度下线），排障时「这次清单为什么少了三家」要看的就是这一行
        if path == endpoints::MODEL_CONFIG_PATH_V2 {
            logging::log(
                "[Models]",
                &format!(
                    "⚠️ Accio {}目录回落到 v2 精简清单（{} 个模型）—— v1 本次不可用",
                    region.label(),
                    parsed.len()
                ),
            );
        }
        models = parsed;
        break;
    }
    if models.is_empty() {
        return ModelRefreshOutcome::failed(
            last_error.unwrap_or_else(|| "上游目录里没有可见模型".to_string()),
        );
    }
    let count = models.len();
    let now = logging::now_ms();
    // 先落持久化缓存（进程重启后由 `restored_cache` 读回）；**不在缓存格锁内**
    // —— 缓存写入要拿库连接锁，两把锁不能嵌套。scope 与格子一一对应。
    catalog_cache::save(cache_scope(region), &models, now);
    {
        let mut guard = cache_slot(region).write().unwrap_or_else(|error| error.into_inner());
        guard.models = models;
        guard.fetched_at = now;
    }
    logging::log(
        "[Models]",
        &format!("Accio {}模型目录已刷新（{count} 个模型）", region.label()),
    );
    ModelRefreshOutcome::refreshed(count)
}

/// 解析模型目录响应。
///
/// ── 两种信封都要认（2026-09 实测的第一种是当前形态）───────────
///   1. `{success, data:[{provider, modelList:[…]}, …]}` —— **现在就是这个**，
///      `data` 是 provider 数组（早先的解析按对象找 `providers`，于是恒为空）；
///   2. `{data:{providers:[{modelList:[…]}]}}` —— 旧形态，保留兼容。
///
/// ── 两道过滤 ────────────────────────────────────────────────
///   - `visible: false` 是上游给内部用的（图像 / 预览版，实测 69 条里 28 条）；
///   - [`is_chat_entry`] 挡掉专用生成模型（见那个函数的说明）。
fn parse_catalog(payload: &Value) -> Vec<Value> {
    // 先看 `data` 本身是不是 provider 数组（形态 1）
    let root = payload.get("data").unwrap_or(payload);
    let providers = root
        .as_array()
        .or_else(|| root.get("providers").and_then(Value::as_array));
    let Some(providers) = providers else {
        return Vec::new();
    };
    let mut models: Vec<Value> = Vec::new();
    for provider in providers {
        let Some(model_list) = provider.get("modelList").and_then(Value::as_array) else {
            continue;
        };
        for entry in model_list {
            if entry.get("visible").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            if !is_chat_entry(entry) {
                continue;
            }
            let normalized = normalize_entry(entry);
            let id = normalized.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if models.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                continue;
            }
            models.push(normalized);
        }
    }
    models
}

/// 这条目录条目是不是**对话**模型。
///
/// ── 为什么光看 `visible` 不够（2026-09 实测）───────────────────
/// 目录里有专用生成模型（视频生成那一组），它们的 `visible` 是 **null 而不是
/// false**，只按 `visible == false` 过滤会漏进来一条 `rlab-1.0-storyboard`
/// （「Multi-shot Video」，`contextWindow` 也是 null）—— 它会以「上下文 0」
/// 的形态出现在模型列表里，点它必然失败。这类条目的判据是
/// `supportedOperationList`：实测**对话条目一律为 null**，只有视频生成那条
/// 给了 `["VIDEO_GENERATION"]`。
///
/// 用「点名挡掉已知的非对话操作」而不是「只放行空列表」：上游将来给对话条目
/// 填上这个字段时，宁可多收一个也不要把正常模型挡在门外（少个模型是静默的，
/// 多一个奇怪的模型至少看得见）。
fn is_chat_entry(entry: &Value) -> bool {
    const NON_CHAT_OPERATIONS: &[&str] = &["VIDEO_GENERATION", "IMAGE_GENERATION", "AUDIO_GENERATION"];
    entry
        .get("supportedOperationList")
        .and_then(Value::as_array)
        .is_none_or(|operations| {
            !operations
                .iter()
                .filter_map(Value::as_str)
                .any(|operation| {
                    NON_CHAT_OPERATIONS
                        .iter()
                        .any(|blocked| operation.eq_ignore_ascii_case(blocked))
                })
        })
}

#[cfg(test)]
mod tests {
    //! 对外 id 的口径（上游 `modelName`，**不是**展示名）—— 2026-10-05 实测复核。
    //!
    //! 守的是「同一份上游清单在本网关与官方客户端长得不一样」这个症状：
    //! 曾经拿 `modelDisplayName` 当 id，于是官方客户端里的 `gpt-6-astra` 到本网关
    //! 变成 `GPT 6 Astra`，区域变体要靠拼后缀消歧，上游给中文展示名的条目
    //! （`Standard` / `专业` / `极致`）还会把中文营销词广告成模型 id。
    use super::*;

    /// 上游三条真实条目（字段按实测裁剪）：id 必须是 `modelName`，
    /// 展示名只进 `name`。
    #[test]
    fn id_comes_from_model_name_not_the_display_name() {
        let entry = normalize_entry(&json!({
            "modelCode": "1Helix-G6aS8tR2qN7m",
            "modelName": "gpt-6-astra",
            "modelDisplayName": "GPT 6 Astra",
            "contextWindow": 1_050_000,
            "multimodal": true,
        }));
        assert_eq!(Some("gpt-6-astra"), entry.get("id").and_then(Value::as_str));
        assert_eq!(Some("GPT 6 Astra"), entry.get("name").and_then(Value::as_str));
        assert_eq!(
            Some("1Helix-G6aS8tR2qN7m"),
            entry.get("upstreamKey").and_then(Value::as_str),
            "发给上游的仍是代号"
        );
    }

    /// 上游给中文展示名的两条（`Standard` = deepseek-v4-flash、
    /// `专业` = gemini-3.8-flash）：id 不能是中文营销词。
    #[test]
    fn chinese_marketing_display_names_never_become_ids() {
        let standard = normalize_entry(&json!({
            "modelCode": "1Nexus-R3wF8qJ5vB6h",
            "modelName": "deepseek-v4-flash",
            "modelDisplayName": "Standard",
            "contextWindow": 1_000_000,
        }));
        assert_eq!(Some("deepseek-v4-flash"), standard.get("id").and_then(Value::as_str));
        assert_eq!(Some("Standard"), standard.get("name").and_then(Value::as_str));

        let pro = normalize_entry(&json!({
            "modelCode": "1Orbit-I9eY4tK8bW1f",
            "modelName": "gemini-3.8-flash",
            "modelDisplayName": "专业",
            "contextWindow": 1_000_000,
        }));
        assert_eq!(Some("gemini-3.8-flash"), pro.get("id").and_then(Value::as_str));
    }

    /// 区域变体的 `@sg` 本来就在 `modelName` 里，不再靠拼展示名后缀。
    #[test]
    fn regional_variant_keeps_the_upstream_suffix() {
        let entry = normalize_entry(&json!({
            "modelCode": "1Drift-T3T9WWS9a23w",
            "modelName": "qwen3.8-max@sg",
            "modelDisplayName": "Qwen 3.8 Max",
            "contextWindow": 1_000_000,
        }));
        assert_eq!(Some("qwen3.8-max@sg"), entry.get("id").and_then(Value::as_str));
        // 主版本与 @sg 版是两个不同的 id（上游就是这么给的）
        let main = normalize_entry(&json!({
            "modelCode": "1Drift-T3TB6WS9a23w",
            "modelName": "qwen3.8-max",
            "modelDisplayName": "Qwen 3.8 Max",
            "contextWindow": 1_000_000,
        }));
        assert_ne!(entry.get("id"), main.get("id"));
    }

    /// 兜底清单与远程清单必须给出**同一批 id**（否则「这次没拉到目录」会让
    /// 用户已配好的名字 404）。
    #[test]
    fn fallback_ids_match_the_upstream_model_names() {
        let fallback = fallback_models();
        let ids: Vec<String> = fallback
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        for wanted in [
            "gemini-3-flash-preview",
            "gemini-3.1-pro-preview",
            "gpt-5.4",
            "gpt-5.2-1211",
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "glm-5",
        ] {
            assert!(ids.iter().any(|id| id == wanted), "兜底清单缺 {wanted}: {ids:?}");
        }
        // 展示名不该出现在 id 里（它是中文 / 带空格的营销名）
        assert!(!ids.iter().any(|id| id.contains(' ')), "兜底 id 里不该有空格: {ids:?}");
    }

    /// `modelName` 缺失时才回退展示名，两者都缺才用代号。
    #[test]
    fn stable_id_falls_back_in_order() {
        assert_eq!("gpt-6-astra", stable_id("1Helix-x", "GPT 6 Astra", "gpt-6-astra"));
        assert_eq!("GPT 6 Astra", stable_id("1Helix-x", "GPT 6 Astra", "  "));
        assert_eq!("1Helix-x", stable_id("1Helix-x", "", ""));
    }
}

//! WorkBuddy 模型目录（对照 Node 版 src/workbuddy-models.mjs 全量移植）。
//!
//! 模型清单内嵌于 WorkBuddy 内置 CLI 的 product.json（v5.3.14）。这里收录
//! 主要对话/多模态模型，作为 /v1/models 响应与默认模型回退。
//!
//! 运行时权威来源：`GET {endpoint}/v3/config` 的**选择器清单**
//! （`data.agents[].models`，cli agent 优先；企业账号再取
//! `{endpoint}/console/enterprises/{id}/config/models`，本模块一并实现）。
//! 注意：服务端按官方客户端 UA 区分来源，缺失 UA 时 /v3/config 的 models
//! 可能返回 null，因此 refresh 时必须带 User-Agent。
//!
//! ⚠️ **不要**改回 `data.models`（那是全量目录，国内版实测 55 条，含虚拟档位
//! 别名与专线变体）：官方桌面端选择器展示的是 `agents[].models` 那份（17 条），
//! 读全量目录会让本网关的清单比官方客户端与 workbuddy2api-hub 都宽出一倍，
//! 详见 [`picker_models`] 的说明。
//!
//! ── 远程清单的持久化（与 Node 版的唯一一处有意分叉）──────────
//! 任务书提到「只在版本变化时写盘 {config_dir}/models.json」；早先按「Node 版
//! 没有任何磁盘缓存」（全仓 grep `models.json` 零命中，workbuddy-models.mjs
//! 全文没有 fs 调用）移植时**没有实现**，目录因此是纯内存态：进程重启回到
//! 内置清单，再由启动时/每次 GET /v1/models 的异步刷新重新拉取。
//!
//! 现在补上了，形态与任务书不同：**不写独立文件，进统一库的 `kv` 表**
//! （`providers::catalog_cache`）。改主意的理由：纯内存态下，重启那次刷新
//! 若失败（网络 / 无账号 / 上游报错），用户看到的就是内置清单 —— 上次拉到的
//! 新清单整个丢失（上游新增的模型消失、已下架的模型回来被广告出去）。
//! 这个分叉是有意的：Node 版没有「多提供商 + 管理页」这套界面，清单丢失的
//! 影响面比现在小得多。缓存的形态与「为什么不留有效期」见那个模块的模块头。
//!
//! ── 并发 ──────────────────────────────────────────────────
//! 目录是「读多写少」的共享态：句柄内一把 `RwLock`，读路径（/v1/models、
//! /health、聊天路由的模型校验，以及经聚合目录间接读它的 /api/session）
//! 拿读锁，刷新落地时拿写锁。
//! 硬约束（与账号存储相同）：**持锁期间绝不做网络请求** —— 刷新先把请求
//! 发完、解析完，最后才在锁内做一次整体替换。
//!
//! ── 多提供商（Agent2API 改造 W2a-T2）后的定位 ──────────────
//! 本模块**只负责 workbuddy 这一家的清单**（内置清单 + /v3/config 远程刷新），
//! 它继续是 workbuddy 的清单事实来源。跨 provider 的合并视图在
//! `core::providers::catalog`（聚合目录），两边共用 `shape.rs` 的「来源无关」
//! 构造件（条目字段映射、响应信封、相近提示纯函数）——
//! 于是「单家清单」与「聚合清单」永远不会在字段映射上分叉。
//!
//! ── 国内版 / 国际版拆家后：**一家两份清单**（2026-10）─────────
//! WorkBuddy 的国际版是独立 provider（`workbuddy-intl`，见
//! `providers::workbuddy::region`），它有**自己的** `/v3/config` 清单与自己的
//! 持久化缓存槽。两个地区的清单在结构上是两份独立数据（上游按站点下发各自
//! 的模型表），因此本模块的目录句柄从「一个单例」变成「按地区各一个」：
//! `global_catalog(region)` 返回对应地区那一份，`ModelCatalog` 自己也记住
//! 「我是谁的清单」（provider id / 缓存 scope / 版本），于是 `owned_by`、
//! 缓存读写、默认规则种子的 provider 键都从实例上取，而不是写死常量。
//!
//! 拆家前这两份清单挤在同一个 `OnceLock` 里：用国内账号拉一次、再用国际账号
//! 拉一次会**互相覆盖**（issue #74 的根因）。现在各刷各的、各自恢复各自的缓存。
//!
//! 内置兜底清单（`builtin_models_intl` / `builtin_models_cn`）**按地区各一份**：
//! 上游两个站点下发的清单与倍率都不同（见 `builtin_models_for` 的说明），
//! 共用一份会让其中一个地区的兜底清单有一半是错的。两份表是实测快照，
//! 远程可用时以远程清单为准。
//!
//! 词法上本模块是个目录（`models/mod.rs` + `models/shape.rs`）：拆分的唯一
//! 理由是单文件行数约定（≤800 行），职责边界与拆分前完全一致。
//!
//! ── 进程级句柄 ──────────────────────────────────────────────
//! `global_catalog(region)` 是聚合层的读取口（每个地区一个 `OnceLock` 单例，
//! 与 `config::init` 同一模式）：`ServerState::bootstrap` 与聚合层
//! 拿到的是同一实例，刷新对两边同时可见。

mod shape;

use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::endpoints::{normalize_endpoint, user_agent_for_edition};
use crate::server::core::providers::workbuddy::region::Region;
use crate::server::core::providers::catalog_cache;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

// 本模块内部用（is_chat_model / apply_filters / get / apply_remote / list_response）
use shape::{js_truthy, value_text};

// 对外再导出：聚合目录（core::providers::catalog）与排障方从 `core::models`
// 取这些名字，保持与拆分前同一条导入路径。
// `shape_value_text` 是 `shape::value_text` 的别名导出：聚合目录要按 `name`
// 匹配模型（对齐 `ModelCatalog::get` 的第二段），需要与这里同一套 JS 文本化。
pub use shape::{list_item, list_response_from, model_id, suggest_from, value_text as shape_value_text};

/// 下游可见模型白名单：/v1/models 与路由解析只暴露这些模型。
///
/// 空数组 = 不限制，全量放行（/v3/config 拉回的真实清单，约 51 个模型）。
/// 与 Node 版一样保持空数组 —— 需要收窄时填入模型 id。
pub const MODEL_ALLOWLIST: &[&str] = &[];

/// 非对话模型的排除前缀，照抄 Node 版 CHAT_MODEL_EXCLUDE_RE 的九个分支。
///
/// 判定依据来自 /v3/config 下发的模型元数据（实测 2026-09-10）：
///   - 内部补全/工具模型带 supportsExtra，输出上限 ≤8192
///   - 图像模型没有 maxOutputTokens（只有 id/name/tags）
///   - 旧一代小输出对话模型（r1-0528 / v3-0324 / kimi-k2-instruct / Claude 内部 id）
///     由黑名单排除，官方客户端 UI 也不展示它们
///
/// 注意 `default-1.` 这一条会把内置清单里的 `default-1.1` / `default-1.2`
/// 一起排除 —— 这是 Node 版的既有行为（那两个是 Claude 的内部 id），
/// 移植时保持一致，不做「顺手修复」。
const CHAT_MODEL_EXCLUDE_PREFIXES: &[&str] = &[
    "completion-",
    "codewise-",
    "hunyuan-image",
    "hunyuan-3b",
    "hunyuan-7b",
    "deepseek-r1-0528",
    "deepseek-v3-0324",
    "kimi-k2-instruct",
    "default-1.",
];

/// 远程目录刷新失败时保留内置目录的说明（Node 的 reason 文案）
const REASON_NO_SESSION: &str = "缺少登录态，使用内置模型目录";

/// 内置模型目录（id → 元数据）—— **按版本两份**。
///
/// ── 为什么是两份（2026-10，参考 workbuddy2api-hub）───────────
/// 上游按站点下发两套清单：国际版（`www.workbuddy.ai`）29 个、国内版
/// （`copilot.tencent.com`）55 个；交集里的同名模型**倍率也不同**
/// （`fast-model`：国际版 x0.34 / 国内版 x0.21；`deep-model`：x3.33 / x1.20；
/// `deepseek-v4.1-flash` 只在国际版、且是 x0.00 限时免费）。
/// 只有一份内置表时，兜底清单必然有一半是错的 —— 而「管理页显示 A 版本的名字
/// 配 B 版本的倍率」正是改造前的实际症状。
///
/// 数据是 2026-10-04 用两个版本的账号各拉一次 `/v3/config` 的实测快照
/// （过滤掉非对话模型后：国际版 27 条、国内版 38 条），性质与 hub 的
/// `STATIC_INTL_MODELS` / `STATIC_CN_MODELS` 相同，只是更新。
/// `credits` 为 `x0.00` 的就是**限时免费**档：
///   国际版 → `deepseek-v4.1-flash` / `hy4-preview-f` / `hy3`；
///   国内版 → `hy3` / `hy3-b` / `hy3-c`。
///
/// `credits` 字段为桌面端展示的单价，仅作参考。
fn builtin_models_intl() -> Vec<Value> {
    json!([
        {"id": "default-model", "name": "Auto", "maxInputTokens": 176000, "maxOutputTokens": 24000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "vendor": "e", "descriptionZh": "优秀的编码模型，适合日常使用", "descriptionEn": "Excellent coding model, great for daily use", "temperature": 1, "isDefault": true},
        {"id": "fast-model", "name": "Fast", "credits": "x0.34 credits", "maxInputTokens": 200000, "maxOutputTokens": 32000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "i", "descriptionZh": "响应快，适合简单任务", "descriptionEn": "Fast responses for simple tasks", "temperature": 1},
        {"id": "balanced-model", "name": "Balanced", "credits": "x0.59 credits", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "速度与质量兼顾，日常工作首选", "descriptionEn": "Balanced speed and quality for daily working", "temperature": 1},
        {"id": "primary-model", "name": "Primary", "credits": "x3.31 credits", "maxInputTokens": 272000, "maxOutputTokens": 72000, "maxAllowedSize": 272000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "高质量输出，胜任复杂任务", "descriptionEn": "High-quality output for complex challenges"},
        {"id": "deep-model", "name": "Deep", "credits": "x3.33 credits", "maxInputTokens": 176000, "maxOutputTokens": 24000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "vendor": "e", "descriptionZh": "深度推理，适合深度分析与难题", "descriptionEn": "Deep reasoning for analysis and hard problems", "temperature": 1},
        {"id": "kimi-k2.8-preview", "name": "Kimi-K2.8-Preview", "credits": "x0.77", "maxInputTokens": 1000000, "maxOutputTokens": 32000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "擅长处理复杂的长程自主任务，前端开发能力突出，同时在知识工作与科研推理上表现出色。", "descriptionEn": "Excels at complex, long-horizon autonomous tasks, with standout front-end skills and strong knowledge work and scientific reasoning", "temperature": 1},
        {"id": "deepseek-v4.1-flash", "name": "Deepseek-V4.1-Flash", "credits": "x0.00", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口，原生多模态", "descriptionEn": "DeepSeek flagship model, supporting 1M context window, native multimodal model", "temperature": 1},
        {"id": "deepseek-v4.1-flash-sg", "name": "Deepseek-V4.1-Flash", "credits": "x0.03", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口，原生多模态", "descriptionEn": "DeepSeek flagship model, supporting 1M context window, native multimodal model", "temperature": 1},
        {"id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "credits": "x0.06", "maxInputTokens": 1000000, "maxOutputTokens": 32000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "原生多模态模型，擅长视觉理解与专业任务", "descriptionEn": "Native multimodal model, excelling at visual understanding and professional tasks", "temperature": 1},
        {"id": "gpt-6-astra", "name": "GPT-6-Astra", "credits": "x6.67", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "OpenAI 旗舰模型，擅长复杂推理与长程任务", "descriptionEn": "OpenAI's flagship model for complex reasoning and long-horizon task"},
        {"id": "hy4-preview-f", "name": "Hy4 preview", "credits": "x0.00", "maxInputTokens": 1000000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy4-preview", "name": "Hy4 preview", "credits": "x0.29", "maxInputTokens": 1000000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy3", "name": "Hy3", "credits": "x0.00", "maxInputTokens": 192000, "maxOutputTokens": 64000, "maxAllowedSize": 192000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "gpt-5.6-sol", "name": "GPT-5.6-Sol", "credits": "x3.47", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "e", "descriptionZh": "OpenAI 旗舰模型，擅长复杂推理与长程任务", "descriptionEn": "OpenAI's flagship model for complex reasoning and long-horizon task"},
        {"id": "gpt-5.6-terra", "name": "GPT-5.6-Terra", "credits": "x1.39", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "e", "descriptionZh": "OpenAI 均衡模型，兼顾能力、速度与成本", "descriptionEn": "OpenAI's balanced model for capability, speed, and cost"},
        {"id": "gpt-5.6-luna", "name": "GPT-5.6-Luna", "credits": "x0.14", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "e", "descriptionZh": "OpenAI 轻量模型，响应快速，适合日常任务", "descriptionEn": "OpenAI's lightweight model for fast responses and everyday tasks"},
        {"id": "gpt-5.5", "name": "GPT-5.5", "credits": "x3.31", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "OpenAI 旗舰编码模型，擅长长程任务", "descriptionEn": "OpenAI's flagship model, excelling at long-horizon tasks"},
        {"id": "gpt-5.4", "name": "GPT-5.4", "credits": "x1.65", "maxInputTokens": 272000, "maxOutputTokens": 72000, "maxAllowedSize": 272000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "OpenAI 旗舰编码模型，擅长长程任务", "descriptionEn": "OpenAI's flagship model, excelling at long-horizon tasks"},
        {"id": "gemini-3.5-flash", "name": "Gemini-3.5-Flash", "credits": "x0.99", "maxInputTokens": 1000000, "maxOutputTokens": 65536, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Well-rounded model for everyday use", "temperature": 1},
        {"id": "glm-5.3", "name": "GLM-5.3", "credits": "x0.79", "maxInputTokens": 1000000, "maxOutputTokens": 48000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Great for daily use", "temperature": 1},
        {"id": "glm-5.2", "name": "GLM-5.2", "credits": "x0.79", "maxInputTokens": 1000000, "maxOutputTokens": 48000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "e", "descriptionZh": "1M 上下文，擅长长程任务", "descriptionEn": "1M context, built for long-horizon tasks.", "temperature": 1},
        {"id": "kimi-k3", "name": "Kimi-K3", "credits": "x1.62", "maxInputTokens": 1000000, "maxOutputTokens": 32000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "擅长处理复杂的长程自主任务，前端开发能力突出，同时在知识工作与科研推理上表现出色。", "descriptionEn": "Excels at complex, long-horizon autonomous tasks, with standout front-end skills and strong knowledge work and scientific reasoning", "temperature": 1},
        {"id": "kimi-k2.6", "name": "Kimi-K2.6", "credits": "x0.52", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "多模态模型，适合日常任务", "descriptionEn": "A multimodal model, good for daily use.", "temperature": 1},
        {"id": "grok-4.7", "name": "Grok-4.7", "credits": "x1.90", "maxInputTokens": 500000, "maxOutputTokens": 128000, "maxAllowedSize": 500000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "SpaceXAI 打造的前沿模型，专为编程、智能体任务及知识型工作而设计。", "descriptionEn": "SpaceXAI's frontier model built for coding, agentic tasks, and knowledge work.", "temperature": 1},
        {"id": "gemini-3.8-flash", "name": "Gemini-3.8-Flash", "credits": "x0.95", "maxInputTokens": 1000000, "maxOutputTokens": 65536, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Built for long-horizon coding and autonomous agents.", "temperature": 1},
        {"id": "gpt-6-sol", "name": "GPT-6-Sol", "credits": "x1.33", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "专为复杂编码和智能体工作流构建的模型", "descriptionEn": "Built for complex coding and agentic workflows."},
        {"id": "gpt-6-luna", "name": "GPT-6-Luna", "credits": "x0.07", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "专为单一聚焦、高吞吐量任务打造的高效模型", "descriptionEn": "Most efficient model for focused, high-volume tasks."},
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

fn builtin_models_cn() -> Vec<Value> {
    json!([
        {"id": "auto", "name": "Auto", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "平衡效果与速度。自动为每个任务匹配最优模型，积分倍率随之浮动", "descriptionEn": "Balances quality and speed. Automatically selects the best model for each task, with a variable credit multiplier.", "temperature": 1, "isDefault": true},
        {"id": "fast-model", "name": "快速", "credits": "x0.21", "maxInputTokens": 300000, "maxOutputTokens": 48000, "maxAllowedSize": 300000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "优先响应速度，适合简单任务与快速问答", "descriptionEn": "Prioritizes speed for simple tasks and quick answers", "temperature": 1, "isDefault": true},
        {"id": "balanced-model", "name": "均衡", "credits": "x0.65", "maxInputTokens": 300000, "maxOutputTokens": 48000, "maxAllowedSize": 300000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "兼顾速度与质量，适合大多数日常工作", "descriptionEn": "Balances speed and quality for most everyday tasks", "temperature": 1, "isDefault": false},
        {"id": "deep-model", "name": "极致", "credits": "x1.20", "maxInputTokens": 300000, "maxOutputTokens": 48000, "maxAllowedSize": 300000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "优先深度与准确性，适合复杂分析和高要求任务", "descriptionEn": "Prioritizes depth and accuracy for complex analysis and high-stakes tasks", "temperature": 1},
        {"id": "hy3", "name": "Hy3", "credits": "x0.00", "maxInputTokens": 192000, "maxOutputTokens": 64000, "maxAllowedSize": 192000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy3-b", "name": "Hy3", "credits": "x0.00", "maxInputTokens": 192000, "maxOutputTokens": 64000, "maxAllowedSize": 192000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy3-c", "name": "Hy3", "credits": "x0.00", "maxInputTokens": 192000, "maxOutputTokens": 64000, "maxAllowedSize": 192000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy3-x", "name": "Hy3", "credits": "x0.05", "maxInputTokens": 192000, "maxOutputTokens": 64000, "maxAllowedSize": 192000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy4-preview", "name": "Hy4 preview", "credits": "x0.29", "maxInputTokens": 960000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy4-preview-dev", "name": "Hy4 preview", "credits": "x0.29", "maxInputTokens": 960000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "hy4-preview-x", "name": "Hy4 preview", "credits": "x0.29", "maxInputTokens": 960000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "descriptionZh": "混元思考模型，具有增强的推理能力", "descriptionEn": "Hunyuan's thinking model with enhanced reasoning capabilities", "temperature": 0.9},
        {"id": "minimax-m2.5", "name": "MiniMax-M2.5", "credits": "x0.18", "maxInputTokens": 200000, "maxOutputTokens": 48000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Well-rounded model for everyday use", "temperature": 1},
        {"id": "glm-5v-turbo", "name": "GLM-5v-Turbo", "credits": "x0.71", "maxInputTokens": 200000, "maxOutputTokens": 64000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "原生多模态模型", "descriptionEn": "Native Multimodal Model", "temperature": 1},
        {"id": "glm-5.3", "name": "GLM-5.3", "credits": "x0.79", "maxInputTokens": 1000000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "旗舰模型，擅长复杂软件工程与长程 Agent 任务", "descriptionEn": "Flagship model, excelling at complex software engineering and long-horizon agent tasks", "temperature": 1},
        {"id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "credits": "x0.06", "maxInputTokens": 1000000, "maxOutputTokens": 131072, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "原生多模态模型，擅长视觉理解与专业任务", "descriptionEn": "Native multimodal model, excelling at visual understanding and professional tasks", "temperature": 1},
        {"id": "glm-5.2", "name": "GLM-5.2", "credits": "x0.79", "maxInputTokens": 1000000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "1M 上下文，擅长长程任务", "descriptionEn": "1M context, built for long-horizon tasks.", "temperature": 1},
        {"id": "glm-5.1", "name": "GLM-5.1", "credits": "x0.79", "maxInputTokens": 200000, "maxOutputTokens": 48000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Great for daily use", "temperature": 1},
        {"id": "glm-5.0-turbo", "name": "GLM-5.0-Turbo", "credits": "x0.95", "maxInputTokens": 200000, "maxOutputTokens": 48000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "e", "descriptionZh": "面向 Agent 场景进行了深度优化", "descriptionEn": "Deeply optimized for agent scenario", "temperature": 1},
        {"id": "glm-4.6v", "name": "GLM-4.6V", "credits": "x0.11", "maxInputTokens": 128000, "maxOutputTokens": 32000, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "GLM-4.6V 多模态模型，支持图片输入", "descriptionEn": "GLM-4.6V multimodal model", "temperature": 1},
        {"id": "kimi-k3-1", "name": "Kimi-K3", "credits": "x1.62", "maxInputTokens": 1000000, "maxOutputTokens": 32000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "擅长处理复杂的长程自主任务，前端开发能力突出，同时在知识工作与科研推理上表现出色。", "descriptionEn": "Excels at complex, long-horizon autonomous tasks, with standout front-end skills and strong knowledge work and scientific reasoning", "temperature": 1},
        {"id": "kimi-k2.8-preview", "name": "Kimi-K2.8-Preview", "credits": "x0.77", "maxInputTokens": 1000000, "maxOutputTokens": 64000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "擅长处理复杂的长程自主任务，前端开发能力突出，同时在知识工作与科研推理上表现出色。", "descriptionEn": "Excels at complex, long-horizon autonomous tasks, with standout front-end skills and strong knowledge work and scientific reasoning", "temperature": 1},
        {"id": "kimi-k2.7", "name": "Kimi-K2.7-Code", "credits": "x0.57", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "多模态模型，适合日常任务", "descriptionEn": "A multimodal model, good for daily use.", "temperature": 1},
        {"id": "kimi-k2.6", "name": "Kimi-K2.6", "credits": "x0.52", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "多模态模型，适合日常任务", "descriptionEn": "A multimodal model, good for daily use.", "temperature": 1},
        {"id": "kimi-k2.5", "name": "Kimi-K2.5", "credits": "x0.45", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "多模态模型，适合日常任务", "descriptionEn": "A multimodal model, good for daily use.", "temperature": 1},
        {"id": "kimi-k2-thinking", "name": "Kimi-K2-Thinking", "credits": "x0.54", "maxInputTokens": 256000, "maxOutputTokens": 32000, "maxAllowedSize": 256000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "适合复杂编码任务", "descriptionEn": "Good for complex coding tasks", "temperature": 1},
        {"id": "minimax-m3", "name": "MiniMax-M3", "credits": "x0.25", "maxInputTokens": 512000, "maxOutputTokens": 64000, "maxAllowedSize": 512000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "原生多模态，擅长代码、智能体任务", "descriptionEn": "Native multimodal model for coding and agents tasks", "temperature": 1},
        {"id": "minimax-m2.7", "name": "MiniMax-M2.7", "credits": "x0.26", "maxInputTokens": 200000, "maxOutputTokens": 48000, "maxAllowedSize": 200000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "能力均衡，适合日常使用", "descriptionEn": "Well-rounded model for everyday use", "temperature": 1},
        {"id": "glm-4.6", "name": "GLM-4.6", "credits": "x0.23", "maxInputTokens": 168000, "maxOutputTokens": 32000, "maxAllowedSize": 168000, "supportsImages": true, "supportsToolCall": true, "vendor": "f", "descriptionZh": "具有强大推理能力的先进语言模型", "descriptionEn": "Advanced language model with strong reasoning capabilities", "temperature": 1},
        {"id": "deepseek-v4-flash", "name": "Deepseek-V4-Flash", "credits": "x0.17", "maxInputTokens": 1000000, "maxOutputTokens": 50000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "f", "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口", "descriptionEn": "DeepSeek flagship model, supporting 1M context window", "temperature": 1},
        {"id": "deepseek-v4.1-flash", "name": "Deepseek-V4.1-Flash", "credits": "x0.11", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口，原生多模态", "descriptionEn": "DeepSeek flagship model, supporting 1M context window, native multimodal model", "temperature": 1},
        {"id": "deepseek-v4-pro", "name": "Deepseek-V4-Pro", "credits": "x0.51", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": false, "vendor": "f", "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口", "descriptionEn": "DeepSeek flagship model, supporting 1M context window", "temperature": 1},
        {"id": "deepseek-v3-2-volc", "name": "DeepSeek-V3.2", "credits": "x0.29", "maxInputTokens": 96000, "maxOutputTokens": 32000, "maxAllowedSize": 96000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "DeepSeek-V3.2 模型，适合日常使用", "descriptionEn": "DeepSeek-V3.2, good for daily use", "temperature": 1},
        {"id": "deepseek-v3-1-volc", "name": "DeepSeek-V3-1-Terminus", "credits": "x0.52", "maxInputTokens": 96000, "maxOutputTokens": 32000, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "vendor": "f", "descriptionZh": "DeepSeek 的旗舰模型，适合规划、调试、编码等任务", "descriptionEn": "DeepSeek's flagship model, good for planning, debugging, coding, and more", "temperature": 0.8},
        {"id": "deepseek-v3-1-lkeap", "name": "DeepSeek-V3-1", "credits": "x0.52", "maxInputTokens": 96000, "maxOutputTokens": 32000, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "vendor": "f", "descriptionZh": "DeepSeek 的旗舰模型，适合规划、调试、编码等任务", "descriptionEn": "DeepSeek's flagship model, good for planning, debugging, coding, and more", "temperature": 0.8},
        {"id": "deepseek-v3-1", "name": "DeepSeek-V3.1", "credits": "x0.52", "maxInputTokens": 96000, "maxOutputTokens": 32000, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "vendor": "f", "descriptionZh": "DeepSeek 的旗舰模型，适合规划、调试、编码等任务", "descriptionEn": "DeepSeek's flagship model, good for planning, debugging, coding, and more", "temperature": 0.8},
        {"id": "hunyuan-2.0-instruct", "name": "Hunyuan-2.0-Instruct", "maxInputTokens": 128000, "maxOutputTokens": 16000, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "j", "temperature": 0.7},
        {"id": "hunyuan-chat", "name": "Hunyuan-Turbos", "maxInputTokens": 128000, "maxOutputTokens": 8192, "maxAllowedSize": 128000, "supportsImages": true, "supportsToolCall": true, "vendor": "j", "descriptionZh": "腾讯自研的轻量、快速的通用模型", "descriptionEn": "Tencent's lightweight, fast general-purpose model"},
        {"id": "space-bunny", "name": "Space-Bunny", "credits": "x0.03", "maxInputTokens": 1000000, "maxOutputTokens": 128000, "maxAllowedSize": 1000000, "supportsImages": true, "supportsToolCall": true, "supportsReasoning": true, "onlyReasoning": true, "vendor": "f", "descriptionZh": "推理速度极快，编码能力强劲，并支持原生多模态输入的匿名大模型", "descriptionEn": "Blazing-fast, anonymous model with strong coding skills and native multimodal input.", "temperature": 1},
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

/// 对话模型判定：网关只服务对话模型，非对话项（补全/内部工具/图像小模型）
/// 不进目录 —— /v1/models、/api/session（经聚合目录）与路由解析三者保持一致。
pub fn is_chat_model(model: &Value) -> bool {
    let id = model.get("id").map(value_text).unwrap_or_default();
    if CHAT_MODEL_EXCLUDE_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return false;
    }
    // `model.supportsExtra` 真值判定（任何真值都排除）
    if model.get("supportsExtra").map(js_truthy).unwrap_or(false) {
        return false;
    }
    // `typeof model.maxOutputTokens !== 'number'` → 排除
    let Some(max_output) = model.get("maxOutputTokens").and_then(Value::as_f64) else {
        return false;
    };
    max_output >= 16_000.0
}

/// 目录的内部状态：模型清单 + 刷新元信息（对应 Node 版的闭包变量）
#[derive(Clone, Debug)]
struct CatalogState {
    models: Vec<Value>,
    /// 最后一次成功的远程刷新时间（毫秒），初始 0
    last_refreshed_at: i64,
    /// 是否曾成功采用远程清单（决定 list_response 的 meta.source）
    remote_refreshed: bool,
}

/// 从 `/v3/config` 响应里取**桌面端选择器真正用的那份清单**。
///
/// ── 为什么不是 `data.models`（2026-10-05 实测）────────────────
/// 上游在同一个响应里下发**两份**模型清单：
///   - `data.models`：全量目录（国内版实测 **55** 条，含 `auto` / `fast-model`
///     这类虚拟档位、`*-volc` / `*-lkeap` 专线变体、以及官方选择器早已不展示
///     的旧条目）；
///   - `data.agents[].models`：**选择器那份**（`cli` agent，国内版实测 **17**
///     条、国际版 **25** 条）。
///
/// 本模块原来读前者，于是同一个账号在本网关看到 37/27 个模型、在
/// workbuddy2api-hub 看到 15/20 个 —— 用户按其中一份清单配客户端，换一个
/// 网关就对不上。参考实现（hub 的 `parse_remote_catalog`）读的是后者，这里
/// 对齐：**cli 命名的 agent 优先，其次取最长的那份 agent 清单**。
///
/// 元数据仍从 `data.models` 里按 id 取（`credits` / 上下文窗口 / 能力位都在
/// 那份条目上）；选择器列了、`data.models` 里没有的 id（实测国际版有
/// `gemini-3.8-flash` / `gpt-6-sol` / `gpt-6-luna`）**照样保留** —— 上游把它
/// 放进选择器就是「可用」的证据，缺元数据只影响展示，不该让它消失。
///
/// 兜底：响应里没有 agent 清单时（上游改形态 / 旧账号）回落到 `data.models`，
/// 保持改造前的行为，而不是把目录清空。
fn picker_models(payload: &Value) -> Vec<Value> {
    let data = payload.get("data").unwrap_or(payload);
    let metadata: Vec<Value> = data
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut cli: Vec<String> = Vec::new();
    let mut longest: Vec<String> = Vec::new();
    let agents = data.get("agents");
    let entries: Vec<(String, &Value)> = match agents {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|entry| {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                Some((name, entry))
            })
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, entry)| (name.trim().to_lowercase(), entry))
            .collect(),
        _ => Vec::new(),
    };
    for (name, entry) in entries {
        let ids: Vec<String> = entry
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|item| {
                        // 实测是裸 id 字符串；对象形态（`{id: ...}`）一并容忍
                        match item {
                            Value::String(text) => Some(text.trim().to_string()),
                            Value::Object(map) => map
                                .get("id")
                                .and_then(Value::as_str)
                                .map(|text| text.trim().to_string()),
                            _ => None,
                        }
                    })
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        if name == "cli" && cli.is_empty() {
            cli = ids.clone();
        }
        if ids.len() > longest.len() {
            longest = ids;
        }
    }
    let picked = if !cli.is_empty() { cli } else { longest };
    if picked.is_empty() {
        return metadata;
    }
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for id in picked {
        let key = id.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        match metadata
            .iter()
            .find(|item| {
                item.get("id")
                    .map(value_text)
                    .map(|text| text.eq_ignore_ascii_case(&id))
                    .unwrap_or(false)
            })
            .cloned()
        {
            Some(item) => out.push(item),
            // 选择器列了、元数据里没有：只留 id（`name` 由聚合层按 id 兜底）
            None => out.push(json!({ "id": id, "name": id })),
        }
    }
    out
}

/// 目录准入过滤：对话模型判定 + 白名单（空白名单全量放行）
fn apply_filters(models: Vec<Value>) -> Vec<Value> {
    models
        .into_iter()
        .filter(is_chat_model)
        .filter(|model| {
            if MODEL_ALLOWLIST.is_empty() {
                return true;
            }
            let id = model.get("id").map(value_text).unwrap_or_default();
            MODEL_ALLOWLIST.contains(&id.as_str())
        })
        .collect()
}

/// 版本 id 常量：WorkBuddy 的两条目录线（上游按站点下发两套清单）。
pub const EDITION_CN: &str = "cn";
/// 国际版（`www.workbuddy.ai`）
pub const EDITION_INTL: &str = "intl";

/// 宽松归一版本 id：只认国际版那几种写法，其余一律国内版
/// （与本仓 `endpoints::resolve_edition` 的别名表同口径）。
pub fn normalize_edition(value: Option<&str>) -> &'static str {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("intl" | "international" | "global" | "ai" | "workbuddy-ai" | "workbuddyai" | "en") => {
            EDITION_INTL
        }
        _ => EDITION_CN,
    }
}

/// 该地区对应的**内置**清单（远程拉不到时的兜底）。
///
/// 两份表是 2026-10-04 用两个地区的账号各拉一次 `/v3/config` 的实测快照：
/// 上游按站点下发两套清单，交集里的同名模型**倍率也不同**（`fast-model`：
/// 国际版 x0.34 / 国内版 x0.21；`deep-model`：x3.33 / x1.20），只有一份表时
/// 兜底清单必然有一半是错的（「管理页显示 A 版本的名字配 B 版本的倍率」）。
fn builtin_models_for(region: Region) -> Vec<Value> {
    match region {
        Region::Intl => builtin_models_intl(),
        Region::Cn => builtin_models_cn(),
    }
}

/// 对外模型名的**版本后缀**（下游想指定版本时用的那个尾巴）。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 两版都有的模型（`deepseek-v4.1-flash` 国际版 x0.00 限免 / 国内版 x0.11）
/// 在 `/v1/models` 里只能占一条 id，下游没法从名字上区分要哪个版本 ——
/// 于是对外额外广告两条带后缀的名字（见 `providers::catalog` 的
/// `append_edition_aliases`）：
///   - `deepseek-v4.1-flash-intl` → 固定走国际版账号；
///   - `deepseek-v4.1-flash-cn`   → 固定走国内版账号；
///   - 不带后缀的那条保持原语义（走队首账号所属的版本）。
pub const EDITION_SUFFIX_INTL: &str = "-intl";
/// 国内版后缀（见 [`EDITION_SUFFIX_INTL`]）
pub const EDITION_SUFFIX_CN: &str = "-cn";

/// 把带版本后缀的对外名拆成 `(清单里的真名, 版本)`。
///
/// 不带后缀 → `(原样, None)`；后缀不构成有效模型名时也不拆（避免把
/// `gpt-4-cn` 这种本来就以 `-cn` 结尾的**真名**误剥 —— 判据是「剥完还剩东西」）。
/// 路由解析（`catalog::routing::entry_id_in_manifest`）与选路
/// （`upstream::rotate::restrict_edition_candidates`）共用这一处。
pub fn split_edition_suffix(model: &str) -> (String, Option<&'static str>) {
    let trimmed = model.trim();
    let lower = trimmed.to_lowercase();
    for (suffix, edition) in [
        (EDITION_SUFFIX_INTL, EDITION_INTL),
        (EDITION_SUFFIX_CN, EDITION_CN),
    ] {
        if lower.ends_with(suffix) && trimmed.len() > suffix.len() {
            return (
                trimmed[..trimmed.len() - suffix.len()].to_string(),
                Some(edition),
            );
        }
    }
    (trimmed.to_string(), None)
}

/// 版本的展示名（对外条目的 `name` 后缀用）。
pub fn edition_label(edition: &str) -> &'static str {
    if edition == EDITION_INTL {
        "国际版"
    } else {
        "国内版"
    }
}

/// 该地区的缓存 scope。
///
/// 键名与 `providers::workbuddy::region::Region::catalog_scope` 同源（那里是
/// 唯一映射）：这里按地区取，写入与读取（[`initial_state`]）用同一个。
/// 国内版的键就是改造前那条不分版本的老键（`"workbuddy"`），老库升级因此
/// 天然把那份清单留在国内版槽里 —— 无需额外迁移读取。
fn cache_scope_of(region: Region) -> &'static str {
    region.catalog_scope()
}

/// 白名单模型必须有可用定义：远程/传入清单缺失时回退内置定义
fn allowlist_fallback(region: Region) -> Vec<Value> {
    builtin_models_for(region)
        .into_iter()
        .filter(|model| {
            MODEL_ALLOWLIST.contains(&model.get("id").map(value_text).unwrap_or_default().as_str())
        })
        .filter(is_chat_model)
        .collect()
}

/// 初始目录：**优先上次成功拉到的远程清单**（持久化缓存，见 `catalog_cache`
/// 的模块头），没有再退回内置清单（经准入过滤）——与 Node 版
/// `createModelCatalog` 的初始化相比，这里多出来的只有缓存恢复这一层。
///
/// 缓存恢复的清单**同样算「远程」**（`remote_refreshed = true`）：它确实是
/// 上游下发的清单，只是可能旧一些。管理页的「来源」列与 `/v1/models` 的
/// `meta.source` 因此显示「远程」而不是「内置」——那是事实，且时间戳
/// （`last_refreshed_at`）会如实给出它是什么时候拉的。
///
/// 缓存槽与内置兜底按**地区**分（见模块头的拆家说明）：两个地区各读各的
/// scope、各用各的内置兜底表（两地的模型与倍率都不同，见 [`builtin_models_for`]）。
///
/// 老库升级：国内版的 scope 就是改造前那条**不分版本**的键（`"workbuddy"`），
/// 那份清单是国内版账号拉的、留在国内版语义正确，因此不需要额外的迁移读取。
fn initial_state(region: Region) -> CatalogState {
    if let Some(cached) = catalog_cache::load(region.catalog_scope()) {
        return CatalogState {
            models: cached.models,
            last_refreshed_at: cached.fetched_at,
            remote_refreshed: true,
        };
    }
    let allowed = apply_filters(builtin_models_for(region));
    let models = if allowed.is_empty() { allowlist_fallback(region) } else { allowed };
    CatalogState { models, last_refreshed_at: 0, remote_refreshed: false }
}

/// 模型目录句柄：内部一把 `RwLock`，克隆共享同一份状态。
///
/// `region` 是**这一份清单属于谁**（拆家后一个地区一个实例）：
/// provider id 进 `owned_by`、缓存 scope 定位读写、版本决定 UA 与端点兜底 ——
/// 三处都从实例取，而不是像拆家前那样写死常量（那正是「两份清单挤在一个
/// 句柄里」的形态）。
#[derive(Clone)]
pub struct ModelCatalog {
    region: Region,
    inner: Arc<RwLock<CatalogState>>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new(Region::Cn)
    }
}

impl ModelCatalog {
    /// 构造目录（不刷新；刷新由启动流程与 /v1/models 触发）
    pub fn new(region: Region) -> Self {
        Self { region, inner: Arc::new(RwLock::new(initial_state(region))) }
    }

    /// 这一份清单属于哪个地区
    pub fn region(&self) -> Region {
        self.region
    }

    /// 读取状态快照；锁中毒（持锁 panic）时接管内部数据继续用，
    /// 与账号存储同一策略：宁可容忍一次中毒，也不要让网关永久不可用。
    fn read(&self) -> CatalogState {
        match self.inner.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write(&self, next: CatalogState) {
        match self.inner.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 按 id 或 name 查模型（大小写不敏感）；查不到返回 None。
    ///
    /// 顺序照抄 Node：先按 id 精确（忽略大小写），再按 name。
    pub fn get(&self, id: Option<&str>) -> Option<Value> {
        let id = id.map(str::trim).filter(|value| !value.is_empty())?;
        let lower = id.to_lowercase();
        let models = self.read().models;
        models
            .iter()
            .find(|model| value_text(&model["id"]).to_lowercase() == lower)
            .or_else(|| {
                models.iter().find(|model| {
                    model
                        .get("name")
                        .map(value_text)
                        .unwrap_or_default()
                        .to_lowercase()
                        == lower
                })
            })
            .cloned()
    }

    /// 目录里是否有该模型。
    ///
    /// **不再被对话链路调用**（Agent2API 改造 W2b-T3 起模型校验走
    /// `providers::catalog::providers_for_model`，视图是聚合目录）：这里是
    /// 「只看 workbuddy 这一家」的判定，排障时与聚合判定对比即可分辨
    /// 「是清单不对」还是「聚合/去重不对」。
    #[allow(dead_code)]
    pub fn has(&self, id: &str) -> bool {
        self.get(Some(id)).is_some()
    }

    /// 解析模型 id：只接受目录里真实存在的 id（大小写不敏感）。
    ///
    /// 未知名直接返回 None —— 绝不静默改写/回退：用户点名要 A 就必须路由到 A，
    /// 不存在就报错让下游自己改（路由层据此返回 400 与相近模型提示）。
    ///
    /// Node 版 `createModelCatalog` 导出的 `resolve(id)` 对等物（Node 里未命中会抛
    /// ModelRouteError）：本进程的请求校验走 `models().has()` + `suggest()`，
    /// 因此这条暂无调用点，保留以便与 Node 的目录 API 一一对应。
    #[allow(dead_code)]
    pub fn resolve(&self, id: &str) -> Option<String> {
        self.get(Some(id))
            .and_then(|model| model.get("id").map(value_text))
    }

    /// 仅供 400 报错信息用的「你是不是想要」提示：列出目录里与请求名最相近的 id。
    ///
    /// 结果不参与路由 —— 路由永远只认精确存在的模型。
    /// 判定逻辑在 `suggest_from`（聚合目录共用同一份，保证单一来源与聚合两种
    /// 视角下的提示口径一致）。
    ///
    /// **不再被对话链路调用**（Agent2API 改造 W2b-T3 起 400 提示走
    /// `providers::catalog::suggest_models`，数据源是聚合目录）：这里是
    /// 「只看 workbuddy 这一家」的视图，排障时与聚合提示对比即可分辨
    /// 「是清单不对」还是「聚合/去重不对」。对只有 workbuddy 一家的用户，
    /// 两者结果相同（共用 `suggest_from`）。
    #[allow(dead_code)]
    pub fn suggest(&self, id: &str, limit: usize) -> Vec<String> {
        let ids: Vec<String> = self
            .list()
            .iter()
            .filter_map(|model| model.get("id").map(value_text))
            .collect();
        suggest_from(ids, id, limit)
    }

    /// 当前目录（克隆；调用方不会看到后续刷新）。
    ///
    /// 拆家后本实例只承载**一个地区**，因此这里就是那个地区的清单 ——
    /// 两个地区的合并去重由聚合层（`providers::catalog`）负责：它按
    /// provider id 分别取两份，再对外按先到先得去重。
    pub fn list(&self) -> Vec<Value> {
        self.read().models
    }

    /// 本实例的地区是否曾成功采用远程清单（挂进响应 meta 用；聚合层据此算「来源」）
    pub fn remote_refreshed(&self) -> bool {
        self.read().remote_refreshed
    }

    /// 最后一次成功远程刷新的时间（毫秒，未刷新过为 0）
    pub fn last_refreshed_at(&self) -> i64 {
        self.read().last_refreshed_at
    }

    /// 目录条数（/health 的 models 字段）
    pub fn count(&self) -> usize {
        self.read().models.len()
    }

    /// GET /v1/models 的 OpenAI 风格响应：`{object, data: [...], meta: {...}}`
    ///
    /// 响应体本身的拼装走 `list_response_from`（聚合目录共用同一函数，
    /// 区别只在 data 是合并后的数组）—— 单家与聚合两个视角的信封字段
    /// 与顺序不可能分叉。
    ///
    /// **保留但不再被 /v1/models 调用**（Agent2API 改造 W2a-T2 起该端点走
    /// `providers::catalog::models_response`）：这里是「只看 workbuddy 这一家」
    /// 的视图，排障时与聚合输出对比即可立刻分辨「是 workbuddy 清单不对」
    /// 还是「聚合/过滤逻辑不对」。对只有 workbuddy 一家的用户，
    /// 它与聚合输出逐字段相同（聚合层复用同一个 list_item / 信封）。
    ///
    /// `owned_by` 取本实例的地区（拆家后 `workbuddy` / `workbuddy-intl`）：
    /// 写死国内版会让国际版的排障视图指错家。
    #[allow(dead_code)]
    pub fn list_response(&self) -> Value {
        let data: Vec<Value> = self
            .list()
            .iter()
            .map(|model| list_item(model, self.region.provider_id()))
            .collect();
        list_response_from(
            data,
            if self.remote_refreshed() { "remote" } else { "builtin" },
            self.last_refreshed_at(),
        )
    }

    /// 是否已采用远程清单（排障用）。
    /// Node 版 `createModelCatalog` 导出的 `get isRemote()` 对等物（Node 里同样
    /// 没有生产调用点）：`list_response` 的 `meta.source` 已表达同一事实，
    /// 这条保留给排障探针按布尔直读。
    #[allow(dead_code)]
    pub fn is_remote(&self) -> bool {
        self.remote_refreshed()
    }

    /// 远程配置刷新（与桌面端行为一致）：
    ///
    ///   1) GET {endpoint}/v3/config  → data.models 为运行时清单
    ///   2) 企业账号再取 /console/enterprises/{id}/config/models
    ///
    /// 两者都不可用时保留内置目录。拉回的清单先过 MODEL_ALLOWLIST 白名单。
    /// 返回 `{refreshed, count, source}` 或 `{refreshed:false, reason}` 语义
    /// 与 Node 版逐字一致 —— 调用方据此决定打 ✅ 还是 verbose。
    pub async fn refresh(&self, params: RefreshParams<'_>) -> RefreshOutcome {
        let base = params.endpoint.trim_end_matches('/').to_string();
        let has_auth = params.headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("authorization") && !value.trim().is_empty()
        });

        if !base.is_empty() && has_auth {
            let url = format!("{base}/v3/config");
            // 与 Node 一致：显式 Accept + 官方 UA（服务端按 UA 识别客户端通道，
            // 缺失时会只下发旧版兼容清单）。Accept 由 send_raw 统一补
            // `application/json`（Node 这里给的也是同一个值），不必重复声明 ——
            // reqwest 的 header() 是 append，重复给会变成两个 Accept 头。
            let mut headers: Vec<(String, String)> =
                vec![("User-Agent".to_string(), params.user_agent.to_string())];
            headers.extend(params.headers.iter().cloned());
            match send_raw("GET", &url, None, &headers, params.proxy, None).await {
                // 不校验 HTTP 状态：与 Node 一致 —— 先按 JSON 解析，再看有没有可用模型
                Ok(response) => match response.payload {
                    Some(payload) => {
                        let raw = picker_models(&payload);
                        let allowed = apply_filters(raw.clone());
                        if !allowed.is_empty() {
                            self.apply_remote(allowed, "auto", false);
                            return RefreshOutcome::refreshed(
                                self.count(),
                                "v3/config",
                            );
                        }
                        logging::verbose(
                            "[Models]",
                            &format!("远程清单({} 个)不含可用对话模型，保留内置目录", raw.len()),
                        );
                    }
                    None => {
                        logging::verbose("[Models]", "远程配置拉取失败: 响应不是合法 JSON");
                    }
                },
                Err(error) => {
                    logging::verbose(
                        "[Models]",
                        &format!(
                            "远程配置拉取失败: {}",
                            crate::server::core::egress::describe_error_detail(&error)
                        ),
                    );
                }
            }
        }

        if !base.is_empty() {
            if let Some(enterprise_id) = params.enterprise_id.filter(|id| !id.is_empty()) {
                let url = format!(
                    "{base}/console/enterprises/{}/config/models",
                    encode_uri_component(enterprise_id)
                );
                let headers: Vec<(String, String)> = params.headers.to_vec();
                return match send_raw("GET", &url, None, &headers, params.proxy, None).await {
                    Ok(response) => {
                        let raw = response
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("data"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let allowed = apply_filters(raw);
                        if allowed.is_empty() {
                            RefreshOutcome::not_refreshed("企业模型列表为空")
                        } else {
                            self.apply_remote(allowed, "custom:", true);
                            RefreshOutcome::refreshed(self.count(), "enterprise")
                        }
                    }
                    Err(error) => RefreshOutcome::not_refreshed(
                        crate::server::core::egress::describe_error_detail(&error),
                    ),
                };
            }
        }

        RefreshOutcome::not_refreshed(REASON_NO_SESSION)
    }

    /// 落地一份远程清单（锁内只做替换，不做任何 IO；锁外再补一次默认规则
    /// 种子，见函数尾）
    ///
    /// 默认模型统一为 auto（远程可能把 fast-model 等档位模型标为默认，
    /// 与网关默认不一致）。`enterprise` 为 true 时走企业分支：
    /// modelType 按 `custom:` 前缀区分企业定制/内置模型
    /// （判定用**原始 id**，大小写不敏感的处理只用于 isDefault 那一处）。
    fn apply_remote(
        &self,
        models: Vec<Value>,
        custom_prefix: &str,
        enterprise: bool,
    ) {
        let mapped: Vec<Value> = models
            .into_iter()
            .map(|model| {
                let raw_id = model.get("id").map(value_text).unwrap_or_default();
                let lower_id = raw_id.to_lowercase();
                let is_auto = lower_id == "auto";
                let mut object = match model {
                    Value::Object(map) => map,
                    other => return other,
                };
                object.insert("isDefault".to_string(), Value::Bool(is_auto));
                let model_type = if enterprise {
                    if raw_id.starts_with(custom_prefix) { "enterprise" } else { "built-in" }
                } else if is_auto {
                    "auto"
                } else {
                    "built-in"
                };
                object.insert("modelType".to_string(), Value::String(model_type.to_string()));
                Value::Object(object)
            })
            .collect();
        let ids: Vec<String> = mapped
            .iter()
            .filter_map(|model| model.get("id").map(value_text))
            .collect();
        let state = CatalogState {
            models: mapped,
            last_refreshed_at: logging::now_ms(),
            remote_refreshed: true,
        };
        self.write(state.clone());
        // 落持久化缓存（进程重启后由 `initial_state` 读回，见 `catalog_cache`
        // 的模块头）。必须在写锁**之外**：缓存写入要拿库连接锁，而本目录的
        // 硬约束是「持锁期间不做 IO」。槽位按地区取 —— 两个地区的清单互不
        // 覆盖（这正是拆家要修的那件事，见模块头）。
        catalog_cache::save(
            cache_scope_of(self.region),
            &state.models,
            state.last_refreshed_at,
        );
        // 远程清单首次落地时补一次 WorkBuddy 默认规则种子（默认只启用白名单内的
        // 模型，见 `model_rules::seed_workbuddy_defaults`）。必须放在写锁之外：种子
        // 要写 config.json，而「持锁期间不做任何 IO」是本目录的硬约束。
        // provider 键取本实例的地区：种子按 `(provider, id)` 记账，国际版的
        // 模型名与国内版可能同名，记错家会让两家的开关互相污染。
        if let Some(summary) =
            crate::server::core::model_rules::seed_workbuddy_defaults(
                self.region.provider_id(),
                &ids,
            )
        {
            logging::log("[Models]", &summary);
        }
    }

    /// 按当前账号的凭证/端点/UA/出口刷新一次（路由层与启动流程的入口）。
    ///
    /// ── 「当前账号」是**本地区**的队首（拆家后的口径）──────────────
    /// 本实例只承载一个地区，因此这里的「当前账号」必须收窄到本家的 provider
    /// （`current_entry_for_provider`）—— 两个地区的账号混在一个队列里取队首，
    /// 会让国内版的清单用国际版账号去拉（拆家前正是这个形态，症状是「拉一次
    /// 换一个地区」，见 issue #74）。
    ///
    /// `account_id` 非空 = 用户在「获取模型」弹窗里点名的那条：按 id 直取，
    /// **取不到就明确失败、不回落到队首**（否则清单会变成「选了 A、用的是 B」）。
    ///
    /// 返回 [`RefreshOutcome`]（本函数内部本来就算出了它，只是过去只用于打日志）：
    /// 自动路径不看返回值（照旧只写日志），**手动刷新路径**靠它如实汇报
    /// 「刷到了几个 / 为什么没刷」—— 见 `providers::adapter` 的 `refresh_models`
    /// 契约。取不到可用登录态、`/v3/config` 失败或返回的清单不含对话模型，
    /// 都走 `not_refreshed(原因)`，既有行为（保留现有清单 + verbose 日志）不变。
    pub async fn refresh_with_current_account(
        &self,
        store: &crate::server::core::account_store::AccountStore,
        auth: &crate::server::core::auth::AuthService,
        account_id: &str,
    ) -> RefreshOutcome {
        // 账号选取三档：
        //   · 点名了（`account_id` 非空）→ 按 id 直取那一条，取不到即失败（见上）；
        //   · 没点名 → 本地区的队首账号优先（多账号时与转发一致）；
        //   · 本地区一条账号都没有 → 回落到本地区的默认登录态（环境变量
        //     `WORKBUDDY[_INTL]_TOKEN` 或单账号 auth.json）。
        let requested = account_id.trim();
        let (session, proxy) = if requested.is_empty() {
            match store.current_entry_for_provider(self.region.provider_id()) {
                Some(entry) => {
                    let proxy = crate::server::core::proxies::session_proxy(&entry.session);
                    (Some(entry.session), proxy)
                }
                None => match auth.get_current_session().await {
                    Ok(session) => (session, None),
                    Err(error) => {
                        logging::verbose("[Models]", &format!("模型目录刷新失败: {}", error.message));
                        return RefreshOutcome::not_refreshed(error.message);
                    }
                },
            }
        } else {
            match store.session_for_account(requested) {
                Some(entry) => {
                    let proxy = crate::server::core::proxies::session_proxy(&entry.session);
                    (Some(entry.session), proxy)
                }
                None => {
                    logging::verbose(
                        "[Models]",
                        &format!("模型目录刷新失败：指定的账号不存在或不可用（{requested}）"),
                    );
                    return RefreshOutcome::not_refreshed("指定的账号不存在或不可用，请重新选择");
                }
            }
        };
        let Some(session) = session else {
            logging::verbose("[Models]", &format!("模型目录未更新: {REASON_NO_SESSION}"));
            return RefreshOutcome::not_refreshed(REASON_NO_SESSION);
        };

        // 端点兜底按**本地区**取（账号记录里的 endpoint 仍是最高优先的覆盖项：
        // 它带着账号自己的站点，也正是 staging / 反向代理用户的落点）。
        //
        // 「另一个地区也补刷一次」在拆家后不再需要：两个地区是两家独立
        // provider，各自在刷新循环里有自己的排期（见 `adapter::implemented_kinds`
        // 的 `WorkBuddyIntl` 条目），谁有账号谁被刷。
        let fallback_endpoint = self
            .region
            .env_endpoint_override()
            .unwrap_or_else(|| self.region.default_endpoint().to_string());
        let endpoint = session
            .get("endpoint")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(normalize_endpoint)
            .unwrap_or_else(|| normalize_endpoint(fallback_endpoint.as_str()));
        let enterprise_id = session
            .get("account")
            .and_then(|account| account.get("enterpriseId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let headers = crate::server::core::auth::AuthService::build_auth_headers(&session);
        // UA 按本地区的客户端身份（国际版是 `WorkBuddy AI/5.5.2`，国内版
        // `WorkBuddy/5.5.4`；服务端按 UA 识别通道，给错会拿到另一套清单）
        let user_agent = user_agent_for_edition(Some(self.region.id()));

        let outcome = self
            .refresh(RefreshParams {
                endpoint: &endpoint,
                enterprise_id: Some(&enterprise_id),
                headers: &headers,
                user_agent: &user_agent,
                proxy: proxy.as_ref(),
            })
            .await;
        if outcome.refreshed {
            logging::log(
                "[Models]",
                &format!("✅ 模型目录已更新（{} 个，来源 {}）", outcome.count, outcome.source),
            );
        } else {
            logging::verbose("[Models]", &format!("模型目录未更新: {}", outcome.reason));
        }
        outcome
    }
}

/// 刷新入参（对应 Node 版 refresh 的 options 对象）
pub struct RefreshParams<'a> {
    pub endpoint: &'a str,
    pub enterprise_id: Option<&'a str>,
    pub headers: &'a [(String, String)],
    pub user_agent: &'a str,
    pub proxy: Option<&'a ResolvedProxy>,
}

/// 刷新结果：成功 `{refreshed:true, count, source}`，
/// 失败 `{refreshed:false, reason}`（与 Node 版返回值同形）。
#[derive(Clone, Debug)]
pub struct RefreshOutcome {
    pub refreshed: bool,
    pub count: usize,
    pub source: String,
    pub reason: String,
}

impl RefreshOutcome {
    pub fn refreshed(count: usize, source: &str) -> Self {
        Self {
            refreshed: true,
            count,
            source: source.to_string(),
            reason: String::new(),
        }
    }

    pub fn not_refreshed(reason: impl Into<String>) -> Self {
        Self {
            refreshed: false,
            count: 0,
            source: String::new(),
            reason: reason.into(),
        }
    }
}

/// 对照 JS 的 `encodeURIComponent`（企业 id 进 URL 路径前编码）
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 供 /api/endpoints 与排障展示：内置清单（不含运行时刷新结果）。
/// Node 版 workbuddy-models.mjs 同名导出 `builtinModels()` 的对等物
/// （Node 侧注释「供 CLI/调试」，本进程没有 CLI，故只作排障口保留）。
#[allow(dead_code)]
pub fn builtin_models_snapshot() -> Value {
    json!({
        "cn": builtin_models_cn(),
        "intl": builtin_models_intl(),
    })
}

// ─── 进程级目录句柄（聚合目录的读取口）──────────────────────

/// 进程级 workbuddy 目录句柄，**按地区各一个**。**为什么要全局**：聚合目录
/// （`core::providers::catalog`）的公开 API 只收 `&AccountStore`（handler 手边
/// 就有），不该再要求调用方层层传一个 ModelCatalog —— 那会把 `list_models` 与
/// 将来 chat_completions 的签名都改一遍。全局句柄与 `config::current()` /
/// `config::current()` 是同一模式：启动时装一次，之后所有模块共用。
///
/// 取用方式是 `get_or_init`：`ServerState::bootstrap` 与聚合层拿到的是**同一个
/// 实例**（共享同一把 RwLock），即使 bootstrap 被调用多次（重启路径）也不会
/// 出现「ServerState 里刷新了、聚合层读的是另一份旧目录」这种分叉。
static WORKBUDDY_CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
/// 国际版的目录句柄（拆家后与国内版各自独立，见模块头）
static WORKBUDDY_INTL_CATALOG: OnceLock<ModelCatalog> = OnceLock::new();

/// 取某个地区的进程级目录句柄；首次调用即构造（内置清单，不读盘、不刷新）。
///
/// 构造顺序无关：任何模块在启动流程的任何阶段调它都能拿到一个可用的目录
/// （最坏情况是「还没被 `/v3/config` 刷新过」的内置清单，与改造前的初始态一致）。
pub fn global_catalog(region: Region) -> ModelCatalog {
    match region {
        Region::Cn => WORKBUDDY_CATALOG.get_or_init(|| ModelCatalog::new(region)).clone(),
        Region::Intl => WORKBUDDY_INTL_CATALOG.get_or_init(|| ModelCatalog::new(region)).clone(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 上游 `/v3/config` 的两份清单形状（按实测裁剪）：`data.models` 是全量目录，
    /// `data.agents[].models` 是选择器那份。取后者 —— 这是与 hub 对齐的关键一步。
    #[test]
    fn picker_models_prefers_the_cli_agent_list() {
        let payload = json!({
            "data": {
                "models": [
                    {"id": "auto", "name": "Auto", "maxOutputTokens": 32000},
                    {"id": "hy3", "name": "Hy3", "maxOutputTokens": 64000, "credits": "x0.00"},
                    {"id": "deepseek-v3-1", "name": "DeepSeek-V3.1", "maxOutputTokens": 64000},
                    {"id": "fast-model", "name": "快速", "maxOutputTokens": 48000},
                ],
                "agents": [
                    {"name": "cli", "models": ["hy3", "deepseek-v3-1"]},
                    {"name": "ide", "models": ["auto", "fast-model", "hy3"]},
                ],
            }
        });
        let picked = picker_models(&payload);
        let ids: Vec<String> = picked.iter().map(|item| item.get("id").map(value_text).unwrap_or_default()).collect();
        assert_eq!(vec!["hy3", "deepseek-v3-1"], ids);
        // 元数据仍从 `data.models` 取（credits / 输出上限都在那份条目上）
        assert_eq!(Some("x0.00"), picked[0].get("credits").and_then(Value::as_str));
    }

    /// cli agent 缺席时取最长的那份；两份都没有时回落到 `data.models`。
    #[test]
    fn picker_models_falls_back_to_longest_then_to_the_full_catalog() {
        let longest = json!({
            "data": {
                "models": [{"id": "a", "maxOutputTokens": 32000}],
                "agents": [
                    {"name": "ide", "models": ["a"]},
                    {"name": "web", "models": ["a", "b", "c"]},
                ],
            }
        });
        let ids: Vec<String> = picker_models(&longest)
            .iter()
            .map(|item| item.get("id").map(value_text).unwrap_or_default())
            .collect();
        assert_eq!(vec!["a", "b", "c"], ids);

        let no_agents = json!({ "data": { "models": [{"id": "only", "maxOutputTokens": 32000}] } });
        let ids: Vec<String> = picker_models(&no_agents)
            .iter()
            .map(|item| item.get("id").map(value_text).unwrap_or_default())
            .collect();
        assert_eq!(vec!["only"], ids, "没有 agent 清单时保持改造前的行为，不清空目录");
    }

    /// 选择器列了、`data.models` 里没有的 id 必须保留（上游把它放进选择器
    /// 就是可用的证据；缺元数据只影响展示）。
    #[test]
    fn picker_models_keeps_ids_without_metadata() {
        let payload = json!({
            "data": {
                "models": [{"id": "hy3", "name": "Hy3", "maxOutputTokens": 64000}],
                "agents": [{"name": "cli", "models": ["hy3", "gemini-3.8-flash"]}],
            }
        });
        let ids: Vec<String> = picker_models(&payload)
            .iter()
            .map(|item| item.get("id").map(value_text).unwrap_or_default())
            .collect();
        assert_eq!(vec!["hy3", "gemini-3.8-flash"], ids);
    }

    /// 对象形态的 agent 清单与重复 id 都要能处理。
    #[test]
    fn picker_models_handles_object_shape_and_dedups() {
        let payload = json!({
            "data": {
                "models": [{"id": "hy3", "maxOutputTokens": 64000}],
                "agents": {
                    "cli": { "models": [{"id": "hy3"}, {"id": "hy3"}, "glm-5.3"] },
                },
            }
        });
        let ids: Vec<String> = picker_models(&payload)
            .iter()
            .map(|item| item.get("id").map(value_text).unwrap_or_default())
            .collect();
        assert_eq!(vec!["hy3", "glm-5.3"], ids);
    }

    #[test]
    fn normalize_edition_recognizes_aliases() {
        assert_eq!(normalize_edition(Some("intl")), EDITION_INTL);
        assert_eq!(normalize_edition(Some("Global")), EDITION_INTL);
        assert_eq!(normalize_edition(Some("workbuddy-ai")), EDITION_INTL);
        assert_eq!(normalize_edition(Some("cn")), EDITION_CN);
        assert_eq!(normalize_edition(None), EDITION_CN);
        assert_eq!(normalize_edition(Some("nonsense")), EDITION_CN);
    }

    /// 两份内置表必须**真的是两套**：版本独有模型各在一边、同名模型的倍率不同。
    /// 这是「不能只有一份清单」的直接证据（也是这次改造的验收条件）。
    #[test]
    fn builtin_tables_are_two_different_catalogs() {
        let cn = builtin_models_cn();
        let intl = builtin_models_intl();
        assert!(!cn.is_empty() && !intl.is_empty());
        let ids = |models: &[Value]| -> Vec<String> {
            models
                .iter()
                .filter_map(|item| item.get("id").map(value_text))
                .collect()
        };
        let cn_ids = ids(&cn);
        let intl_ids = ids(&intl);
        assert!(cn_ids.iter().any(|id| id == "hy3-b"), "国内版应有 hy3-b");
        assert!(!intl_ids.iter().any(|id| id == "hy3-b"), "国际版不该有 hy3-b");
        assert!(intl_ids.iter().any(|id| id == "gpt-5.6-sol"), "国际版应有 gpt-5.6-sol");
        assert!(!cn_ids.iter().any(|id| id == "gpt-5.6-sol"), "国内版不该有 gpt-5.6-sol");
        let credits = |models: &[Value], id: &str| -> String {
            models
                .iter()
                .find(|item| item.get("id").map(value_text).as_deref() == Some(id))
                .and_then(|item| item.get("credits").and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        assert_ne!(
            credits(&cn, "fast-model"),
            credits(&intl, "fast-model"),
            "同名模型的倍率两版不同，合成一份必然有一边是错的"
        );
    }

    /// 版本归属判定：只在一份清单里出现的模型才收窄，两版都有 / 都不认识一律不收窄。
    ///
    /// 拆家后两个地区是两家独立 provider，各自的清单**天然**只含自己那版的模型
    /// ——「版本独有模型打错站」在结构上不再可能（请求按 provider 分流）。
    /// 这里守的是判据本身：两份内置表确实是两套，独有模型各在一边。
    #[test]
    fn edition_exclusive_of_splits_version_only_models() {
        let cn = builtin_models_for(Region::Cn);
        let intl = builtin_models_for(Region::Intl);
        let has = |models: &[Value], id: &str| {
            models
                .iter()
                .any(|item| item.get("id").map(value_text).as_deref() == Some(id))
        };
        assert!(has(&intl, "gpt-5.6-sol"), "国际版独有模型应在国际版表里");
        assert!(!has(&cn, "gpt-5.6-sol"), "国内版表不该有国际版独有模型");
        assert!(has(&cn, "hy3-b"), "国内版独有模型应在国内版表里");
        assert!(!has(&intl, "hy3-b"), "国际版表不该有国内版独有模型");
        // 两版都有（同一个上游名字）
        assert!(has(&cn, "fast-model") && has(&intl, "fast-model"));
    }

    /// 版本后缀的解析：下游用 `xxx-intl` / `xxx-cn` 指定版本。
    #[test]
    fn edition_suffix_is_split_off_the_requested_name() {
        assert_eq!(
            split_edition_suffix("deepseek-v4.1-flash-intl"),
            ("deepseek-v4.1-flash".to_string(), Some(EDITION_INTL))
        );
        assert_eq!(
            split_edition_suffix("deepseek-v4.1-flash-cn"),
            ("deepseek-v4.1-flash".to_string(), Some(EDITION_CN))
        );
        // 大小写不敏感（后缀与真名都可能被客户端改写）
        assert_eq!(
            split_edition_suffix("Deepseek-V4.1-Flash-INTL"),
            ("Deepseek-V4.1-Flash".to_string(), Some(EDITION_INTL))
        );
        // 不带后缀 / 空串 → 原样
        assert_eq!(split_edition_suffix("fast-model"), ("fast-model".to_string(), None));
        assert_eq!(split_edition_suffix("  "), (String::new(), None));
        // 后缀本身不构成有效名字时**不剥**（`-cn` 结尾的真名不该被误伤）
        assert_eq!(split_edition_suffix("-intl"), ("-intl".to_string(), None));
    }
}

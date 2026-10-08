//! LobsterAI（有道龙虾）产品常量：基址 / 端点路径 / 出站指纹 / 兜底模型表。
//!
//! ── 数据来源 ────────────────────────────────────────────────
//! 逐条对齐参考实现 `src/lobsterai-product.ts` / `src/lobsterai.ts`
//! （它自己又是照 `lobsterai2api` 的 Go 实现与真机实测写的）：
//!   - API 基址 / 版本号接口：`lobsterai2api/sigin.py:10-11`（实测可用）；
//!   - 端点路径与请求头：`lobsterai2api/internal/upstream/client.go`；
//!   - 兜底模型表：`lobsterai2api/internal/server/handler.go:94-114`
//!     （注释标明「2026-08-06 从 `GET /api/models/available` 实测拉取」）。
//!
//! ── 为什么这些常量固化在编译期 ──────────────────────────────
//! 参考实现面向终端用户，不让用户自行提供域名（Go 侧被脱敏成 env
//! `LB2A_UPSTREAM_BASE`，无编译期默认值 —— 那是它的分发形态所致）。
//! 本项目的其它家（workbuddy / raccoon / autoclaw…）同样把域名写成常量，
//! 保持一致。

/// 上游 API 基址（不含路径）。
///
/// `lobsterai2api/sigin.py:10` 硬编码 `https://lobsterai-server.youdao.com`。
pub const API_BASE: &str = "https://lobsterai-server.youdao.com";

/// 登录 portal 基址（不含 `/portal` 路径）。
///
/// 与 [`API_BASE`] 同一 IP、同一 CNAME 目标（`pub-g1-gz.alb.ntes53.netease.com`），
/// 是同一套服务的不同路径 —— 仍分成两个常量：不排除未来门户与 API 分离部署。
pub const PORTAL_BASE: &str = "https://lobsterai.youdao.com";

/// 客户端版本号查询端点（第三方域名）。
///
/// ⚠️ 该响应**不是**统一信封：`code`/`msg` 在**外层**，载荷在 `data.value`。
/// 实测（2026）返回 `version: "2026.9.4"`（日期式版本号）。
pub const CLIENT_VERSION_API: &str =
    "https://api-overmind.youdao.com/openapi/get/luna/hardware/lobsterai/prod/update";

/// 无法动态取到版本号时的兜底值。
///
/// 参考实现的 `sigin.py:73-76` 在取不到版本号时**整个脚本放弃签到**；
/// 参考实现比它宽容：用兜底值继续尝试并记日志 —— 宁可用一个稍旧的版本号试，
/// 也别让用户完全无法签到（后端对 `version` 并不强校验：Go 侧一直发假值
/// `0.1.0` 也未导致失败）。
pub const FALLBACK_CLIENT_VERSION: &str = "2026.9.4";

/// User-Agent（照抄 `client.go:21` 的 `clientUA = "LobsterAI/0.1.0"`）。
///
/// ⚠️ **刻意不跟着真版本号改**：Go 侧用这个 UA 是实测可用的，而
/// `X-LobsterAI-Client-Version` 头用真值。若同时改两处，一旦服务端行为变化
/// 将无法归因是哪一个导致的 —— 先只改有确定依据的那一个（版本头）。
pub const USER_AGENT: &str = "LobsterAI/0.1.0";

/// 客户端能力声明（`X-LobsterAI-Client-Capabilities` 头）。
///
/// 两个能力**都必须声明**，各自解决一个具体问题（2026-09-17 真实凭据实测）：
///   - `kimi-k3-agentic-v1`：**模型列表的准入条件**。不带该能力时
///     `/api/models/available` 只返回 25 个模型且**没有 `kimi-k3`**；带上才 26 个。
///   - `thinking-level-control-v1`：**思考档位协议的前提**。`reasoning_effort`
///     的常规档位（low/high/max/xhigh）不需要它，但 `"off"`（关闭思考）
///     在**不带**该能力时服务端直接 HTTP 500；带上则正常返回。
///
/// 顺序无关（两种顺序都实测通过），但保持与 IDE 的排列一致。
pub const CLIENT_CAPABILITIES: &str = "kimi-k3-agentic-v1,thinking-level-control-v1";

// ── 端点路径 ──

/// 授权码换 token。
pub const EXCHANGE_PATH: &str = "/api/auth/exchange";
/// 静默续期（**不带 Authorization**，只认请求体里的 refreshToken）。
pub const REFRESH_PATH: &str = "/api/auth/refresh";
/// 可用模型列表（`GET`，必须带两个 `X-LobsterAI-Client-*` 头）。
pub const MODELS_PATH: &str = "/api/models/available";
/// 对话端点（OpenAI 兼容，**仅支持 SSE**）。
pub const CHAT_PATH: &str = "/api/proxy/v1/chat/completions";
/// 登录回调路径（对齐 `cmd/login/main.go:35` 的 `callbackPath`）。
pub const CALLBACK_PATH: &str = "/auth/callback";
/// 积分余额端点（`GET`，唯一使用方是 `balance` 模块）。
pub const PROFILE_SUMMARY_PATH: &str = "/api/user/profile-summary";

// ── 签到三步的端点 ──

/// 活动槽位查询端点。
pub const ACTIVITY_SLOT_PATH: &str = "/api/client-activities/slot";
/// 活动上下文查询端点（需拼 activityCode）。
pub const ACTIVITY_CONTEXT_PATH: &str = "/api/client-activities";

/// 槽位查询的三个固定 query 参数（照抄 `sigin.py:52-53`）。
///
/// 这些值是用真实客户端观察到的：`placement=desktop_sidebar` 声明「桌面端侧边栏」
/// 这一投放位，`containerApiVersion=2` 是容器协议版本，`platform=win32` 是
/// **伪装客户端形态** —— 即使网关跑在 macOS/Linux 上也照发 win32，
/// 它与运行环境无关，改了可能拿不到活动。
pub const SLOT_PLACEMENT: &str = "desktop_sidebar";
pub const SLOT_CONTAINER_API_VERSION: &str = "2";
pub const SLOT_PLATFORM: &str = "win32";

// ── 超时 ──

/// 控制面请求超时（毫秒）；对话流式请求不适用（走 `egress` 的读空闲保护）。
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// 登录流程总超时（毫秒）；对齐 `main.go:36` 的 10 分钟回调窗口。
pub const LOGIN_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// 客户端版本号缓存有效期（毫秒，12 小时）。
///
/// 版本号是**日期式**的（如 `2026.9.4`），变更频率极低（客户端发版节奏），
/// 而签到每次都要带它 —— 不缓存会让每次签到多一次跨域请求。
pub const VERSION_CACHE_TTL_MS: u64 = 12 * 60 * 60 * 1000;

// ── 兜底模型表 ──

/// 兜底模型目录中的一个条目。
///
/// 与 `BuddyFallbackModel` 分开定义。注意**不要把这里的 `context_window` 当成
/// 远端权威值**：远端 `GET /api/models/available` 其实**会**返回
/// `contextWindow`（2026-09-17 实测多数为 `1000000`），只是当前尚未解析消费它。
/// 因此这里的值是**纯本地估计值**，且只在本表被用到时（远端整体失败）才生效。
pub struct FallbackModel {
    /// 模型 ID（传给 chat 端点的 `model`）。
    pub id: &'static str,
    /// 展示名。
    pub name: &'static str,
    /// 上下文窗口（桥接层的估计值，非远端权威值；统一填 131072）。
    pub context_window: u64,
}

/// 兜底模型目录（19 个）。
///
/// 来源：`lobsterai2api/internal/server/handler.go:94-114` 的 `staticModels`，
/// 注释标明「2026-08-06 从 `GET /api/models/available` 实测拉取」。
///
/// 顺序**照抄原表**，不重排 —— 它是实测时的返回顺序，重排会让「与上游对比」
/// 这类排查工作失去可比性。
pub const FALLBACK_MODELS: &[FallbackModel] = &[
    FallbackModel { id: "deepseek-v4-flash", name: "deepseek-v4-flash", context_window: 131_072 },
    FallbackModel { id: "deepseek-v4-pro", name: "deepseek-v4-pro", context_window: 131_072 },
    FallbackModel { id: "MiniMax-M3", name: "MiniMax-M3", context_window: 131_072 },
    FallbackModel { id: "MiniMax-M2.7", name: "MiniMax-M2.7", context_window: 131_072 },
    FallbackModel { id: "qwen3.7-max", name: "qwen3.7-max", context_window: 131_072 },
    FallbackModel { id: "qwen3.7-plus", name: "qwen3.7-plus", context_window: 131_072 },
    FallbackModel { id: "qwen3.6-plus", name: "qwen3.6-plus", context_window: 131_072 },
    FallbackModel { id: "qwen3.5-plus-2026-04-20", name: "qwen3.5-plus-2026-04-20", context_window: 131_072 },
    FallbackModel { id: "kimi-k2.7-code", name: "kimi-k2.7-code", context_window: 131_072 },
    FallbackModel { id: "kimi-k2.7-code-highspeed", name: "kimi-k2.7-code-highspeed", context_window: 131_072 },
    FallbackModel { id: "kimi-k2.6", name: "kimi-k2.6", context_window: 131_072 },
    FallbackModel { id: "kimi-k2.5", name: "kimi-k2.5", context_window: 131_072 },
    FallbackModel { id: "doubao-seed-2-1-pro-260628", name: "doubao-seed-2-1-pro-260628", context_window: 131_072 },
    FallbackModel { id: "doubao-seed-2-1-turbo-260628", name: "doubao-seed-2-1-turbo-260628", context_window: 131_072 },
    FallbackModel { id: "doubao-seed-2-0-code-preview-260215", name: "doubao-seed-2-0-code-preview-260215", context_window: 131_072 },
    FallbackModel { id: "glm-5.2", name: "glm-5.2", context_window: 131_072 },
    FallbackModel { id: "glm-5.1", name: "glm-5.1", context_window: 131_072 },
    FallbackModel { id: "glm-5v-turbo", name: "glm-5v-turbo", context_window: 131_072 },
    FallbackModel { id: "glm-5", name: "glm-5", context_window: 131_072 },
];

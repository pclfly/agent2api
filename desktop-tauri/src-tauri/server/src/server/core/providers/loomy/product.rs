//! Loomy（讯飞办公助手）产品常量：双基址 / 签名密钥 / 兜底模型表。
//!
//! ── 数据来源 ────────────────────────────────────────────────
//! 逐条对齐参考实现 `src/loomy-product.ts` / `src/loomy.ts`：
//!   - base URL / AccessKey / appId：`C:\Program Files\Loomy\resources\.env.prod`
//!     （Loomy 官方只是「随客户端分发 + AES 混淆」，本质同样是公开的）；
//!   - 兜底模型表：2026-09-26 实测 `GET /api/v1/models` 取 `type === 'chat'`
//!     的 8 条。
//!
//! ── AccessKey 的定位 ────────────────────────────────────────
//! 它只用于**讯飞账号**端点（`account.xfinfr.com`）的 HMAC-SHA1 签名
//! （短信验证码登录）。业务与推理端点用的是用户登录后的 `session`，
//! 与 AccessKey 无关。故 AccessKey 泄露不涉及任何用户数据。

/// 业务端点基址（推理 / 模型列表 / 积分 / 新手任务）。
pub const API_BASE: &str = "https://loomyad.xunfei.cn/api/v1";

/// 讯飞账号（CAccount）基址。
pub const ACCOUNT_BASE: &str = "https://account.xfinfr.com";

/// 讯飞账号 AccessKeyId（HMAC-SHA1 签名的 `account {ak}:{sig}` 前半）。
pub const ACCESS_KEY_ID: &str = "2thryby66wxi53sk";

/// 讯飞账号 AccessKeySecret（HMAC-SHA1 的密钥）。
pub const ACCESS_KEY_SECRET: &str = "zsak6eadrbawz683wf5r3m2snrwj868r";

/// 讯飞 appId（账号请求体 `base.appid`）。
pub const APP_ID: &str = "GM3LOOMY";

/// 短信验证码有效期（秒）—— 发码请求里的 `expire`。
pub const SMS_CODE_TTL_SECONDS: u32 = 300;

/// 登录会话有效期（秒）= 14 天。
///
/// 依据客户端 `account-service.js:391` 的 `expire: 14 * 24 * 3600`。
/// ⚠️ 这只是向服务端**声明**的有效期，响应里不带到期时间戳，
/// 故凭据的 `expires_at` 由本地按此推算。
pub const SESSION_TTL_SECONDS: u64 = 1_209_600;

/// 请求超时（毫秒）。与 Loomy 客户端一致（60 秒）。
pub const REQUEST_TIMEOUT_MS: u64 = 60_000;

// ── 端点路径 ──

/// 短信发码（账号端点）。
pub const SEND_MSG_PATH: &str = "/login/phone/sendMsgCode";
/// 短信校验登录（账号端点）。
pub const CHECK_CODE_PATH: &str = "/login/phone/checkCode";
/// 模型列表（业务端点，只认 `token` 头）。
pub const MODELS_PATH: &str = "/models";
/// 对话（业务端点，只认 `Authorization: Bearer`；本家两个头都发）。
pub const CHAT_PATH: &str = "/chat/completions";
/// 积分记录（只读，`?pageNo=1&pageSize=1&recordType=all`）。
pub const POINTS_RECORDS_PATH: &str = "/points/records";
/// 每日额度初始化（签到，写）。
pub const FIRST_LOGIN_PATH: &str = "/points/first-login";

/// 账号端点的请求体固定段（客户端 `account-service.js:441-450`）。
///
/// ⚠️ `ua` 硬编码 macOS：客户端在 Windows 上发的也是这个值，照抄不要改。
pub const ACCOUNT_UA: &str = "Loomy|Desktop|Electron|macOS";
/// 账号请求体 `base.modelid`。
pub const ACCOUNT_MODEL_ID: &str = "Web";
/// 账号请求体 `base.version`。
pub const ACCOUNT_VERSION: &str = "1.0.0";
/// 账号请求体 `base.devid`。
pub const ACCOUNT_DEV_ID: &str = "web";

// ── 兜底模型表 ──

/// 兜底模型目录中的一个条目。
pub struct FallbackModel {
    /// 模型 ID（传给 `POST /chat/completions` 的 `model`）。
    pub id: &'static str,
    /// **已规范化**的展示名（含倍率，` · x{n}` 分隔）。
    pub name: &'static str,
    /// 上下文窗口。
    pub context_window: u64,
}

/// 实测的思考档位（8 个 chat 模型**完全一致**，2026-09-28 用真实凭据采集）。
///
/// ⚠️ 远端可用时**优先用远端的 `reasoning_efforts`**，本常量只在远端整体失败
/// 时顶替（与 `context_window` 同策略）。
pub const FALLBACK_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

/// 本家选用的默认档位（**用户要求 `high`**）。
///
/// ⚠️ 远端 `default_reasoning_effort` 声明的是 **`low`**，这里**有意不沿用**。
/// 兜底表与适配器常量必须一致，否则「远端可用」与「远端失败」两条路径
/// 默认档不同。
pub const DEFAULT_EFFORT: &str = "high";

/// 兜底模型目录（8 个 chat 模型）。
///
/// 来源：2026-09-26 实测 `GET /api/v1/models`，取 `type === 'chat'` 的条目。
/// 顺序**照抄远端返回顺序**，不重排 —— 重排会让「与远端对比」这类排查
/// 失去可比性。
///
/// ⚠️ `context_window` 用远端 `context_length`。`spark-x` 是**已知分歧**：
/// 远端声明 1048576，而 Loomy 客户端用本地表 `MODEL_CONTEXT_OVERRIDES`
/// 强制降到 262144。本表**先采信远端**。
pub const FALLBACK_MODELS: &[FallbackModel] = &[
    FallbackModel { id: "deepseek-v4-flash-0731", name: "DeepSeek V4 Flash 0731 · x3.0", context_window: 1_048_576 },
    FallbackModel { id: "MiniMax-M3", name: "MiniMax M3 · x4.0", context_window: 1_048_576 },
    FallbackModel { id: "Kimi-k2.6", name: "Kimi k2.6 · x6.5", context_window: 262_144 },
    FallbackModel { id: "qwen-3.8-max", name: "Qwen 3.8 Max · x12.0", context_window: 1_000_000 },
    FallbackModel { id: "GLM-5.3-Flash", name: "GLM 5.3 Flash · x0.8", context_window: 1_048_576 },
    FallbackModel { id: "qwen3.8-flash", name: "qwen 3.8 flash · x0.8", context_window: 1_000_000 },
    FallbackModel { id: "spark-x", name: "Spark X2.5 · x0.1", context_window: 1_048_576 },
    FallbackModel { id: "mimo-v2.5", name: "MiMo V2.5 · x3.3", context_window: 1_048_576 },
];

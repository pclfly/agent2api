//! MiniMax Code（**中国版**）产品常量：主机 / OAuth 参数 / 端点 / 兜底模型表。
//!
//! ── 数据来源（全部实测或 asar 源码直读）──────────────────────
//!   - 主机：asar `@mavis/config/dist/config.js` 的 `PRESET_BASE_URLS['cn-prod']`；
//!   - OAuth 端点：asar `@mavis/oauth-core/dist/endpoint-config.js`；
//!   - `clientId` / `scope` / `audience`：`~/.minimax/auth/prod/cn/mcode-public/auth.json`
//!     与 asar `contracts.js`；
//!   - 兜底模型表：2026-09-28 实测 `GET /mavis/api/v1/models?region=cn&buildEnv=prod`。
//!
//! ── 只有中国版 ──────────────────────────────────────────────
//! 参考实现只做了中国版（`region: 'cn'`，主机 `*.minimax.cn`）。国际版将来
//! 接入时另立 kind（`minimax-intl`）—— 不要往本家塞 region 字段。

/// 账号与 OAuth 主机。
pub const ACCOUNT_HOST: &str = "https://account.minimax.cn";
/// API 主机（目录 / 签到 / 积分 / 推理）。
pub const API_HOST: &str = "https://agent.minimax.cn";
/// 区域（目录查询参数）。
pub const REGION: &str = "cn";
/// 构建环境（目录查询参数）。
pub const BUILD_ENV: &str = "prod";
/// OAuth client_id（官方常量）。
pub const CLIENT_ID: &str = "mcode-public";
/// OAuth audience（asar `contracts.js` 的 `MCODE_OAUTH_AUDIENCE`）。
pub const AUDIENCE: &str = "agent-backend";
/// OAuth scope（**凭据校验要求必须含它**，否则 `invalid_token_response`）。
pub const SCOPE: &str = "agent.default";

// ── 端点路径 ──

/// OAuth 设备码申请。
pub const DEVICE_CODE_PATH: &str = "/oauth2/device/code";
/// OAuth 令牌（轮询与续期共用）。
pub const TOKEN_PATH: &str = "/oauth2/token";
/// 远端模型目录（需 `?region=&buildEnv=`）。
pub const MODELS_PATH: &str = "/mavis/api/v1/models";
/// 推理路径（**Anthropic Messages** 协议）。
pub const INFER_PATH: &str = "/mavis/api/v1/llm/v1/messages";
/// 签到状态（⚠️ 必须带 `?timezone_id=`）。
pub const SIGNIN_STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
/// 签到领取（⚠️ 必须带 `?timezone_id=`）。
pub const SIGNIN_CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";
/// 积分明细。
pub const CREDIT_DETAILS_PATH: &str = "/minimax-cloud/api/v1/credit/details";
/// 用户资料（手机号 / 昵称 / `userID`，见 `profile.rs`）。
pub const USER_INFO_PATH: &str = "/v1/api/user/info";

/// 积分明细的**单页条数**。
///
/// ⚠️ **必须显式传**：不传时上游默认只回 20 条（实测 2026-10-04，`total_count`
/// 31 而 `details` 只有 20）—— 余额于是**少算**（求和只覆盖最近 20 个积分包，
/// 实测少算 7799.99 积分）。这是「余额显示不正确」的根因。
///
/// ⚠️ **上限 100**：`page_size=101` 上游直接回 HTTP 500
/// （`base_resp.status_code=1406011050`），`200` / `1000` 同样 500。
/// 而 `page` / `page_num` 参数**实测无效**（`page=1/2/3` 返回同一页数据），
/// 无法翻页 —— 所以 100 是「一次能拿到的最大值」，也正好覆盖签到积分
/// （每天最多新增 1 个包，100 天）的全部场景。
pub const CREDIT_PAGE_SIZE: usize = 100;

// ── 超时 ──

/// 请求超时（毫秒）。
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// OAuth 请求超时（毫秒）；比业务请求短，避免用户干等。
pub const OAUTH_TIMEOUT_MS: u64 = 20_000;

/// 必须 adaptive thinking 的模型前缀（M3.1：传 `disabled` 会被硬拒 400）。
pub const ADAPTIVE_ONLY_PREFIX: &str = "MiniMax-M3.1";

// ── 兜底模型表 ──

/// 兜底模型目录中的一个条目。
///
/// ⚠️ **必须镜像远端实测值** —— 两处不一致会让「远端失败降级到兜底表」时
/// 档位或窗口**凭空变化**，且极难排查。
pub struct FallbackModel {
    /// 模型 ID（传给推理端点的 `model`）。
    pub id: &'static str,
    /// 展示名（照抄远端 `name`，不加倍率/后缀）。
    pub name: &'static str,
    /// 上下文窗口 —— **档位表最大档**（不是远端 `limit.context`）。
    pub context_window: u64,
    /// 单次输出上限（远端 `limit.output`）。
    pub max_tokens: u64,
    /// 是否支持图片输入（远端 `modalities.input` 含 `image`）。
    pub supports_image: bool,
    /// 思考档位（远端 `effort_options`）；**只有 M3.1-Flash-Preview 有**。
    pub effort_options: Option<&'static [&'static str]>,
    /// 默认档位（远端 `default_effort`）；必须落在 `effort_options` 内。
    pub default_effort: Option<&'static str>,
    /// 思考开关模式（远端 `thinking_config.mode`）。
    ///
    /// ⚠️ **实测两种值，语义完全不同**：
    ///   - `forced_on`：思考强制开启，传 `disabled` 会被**静默忽略**（M2.7）
    ///     或**硬 400**（M3.1：`requires adaptive thinking ... (2013)`）；
    ///   - `switchable`：可以开关（M3 实测：不传/`disabled` → 0 思考块，
    ///     `adaptive` → 有思考块）。
    pub thinking_mode: Option<&'static str>,
}

/// 兜底模型目录（4 条，顺序**照抄远端 `model_order`**）。
///
/// ⚠️ **必须含 `MiniMax-M3.1-Flash-Preview`** —— 它**不在**客户端内置静态表里
/// （asar 的 `MINIMAX_MODELS` 只有 3 个），若兜底表也漏掉它，远端一失败
/// 用户就**看不到自己在用的模型**。
///
/// ⚠️ **只有 M3.1-Flash-Preview 有 `effort_options`** —— 其余三个远端
/// **没有 `effort_options` 字段**。给它们编档位就是凭空猜测。
pub const FALLBACK_MODELS: &[FallbackModel] = &[
    FallbackModel {
        id: "MiniMax-M3.1-Flash-Preview",
        name: "M3.1-Flash-Preview",
        context_window: 1_000_000,
        max_tokens: 128_000,
        supports_image: true,
        effort_options: Some(&["default", "low", "medium", "high", "xhigh", "max"]),
        default_effort: Some("default"),
        thinking_mode: Some("forced_on"),
    },
    FallbackModel {
        id: "MiniMax-M3",
        name: "M3",
        context_window: 1_000_000,
        max_tokens: 128_000,
        supports_image: true,
        // M3 没有 `effort_options` ⇒ 没有「档位」可选，只有「开/关」两态
        effort_options: None,
        default_effort: None,
        thinking_mode: Some("switchable"),
    },
    FallbackModel {
        id: "MiniMax-M2.7-highspeed",
        name: "M2.7-highspeed",
        context_window: 200_000,
        max_tokens: 128_000,
        supports_image: false,
        effort_options: None,
        default_effort: None,
        thinking_mode: Some("forced_on"),
    },
    FallbackModel {
        id: "MiniMax-M2.7",
        name: "M2.7",
        context_window: 200_000,
        max_tokens: 128_000,
        supports_image: false,
        effort_options: None,
        default_effort: None,
        thinking_mode: Some("forced_on"),
    },
];

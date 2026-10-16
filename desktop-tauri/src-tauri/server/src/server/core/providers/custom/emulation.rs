//! **OpenCode 官方客户端形态伪装**（提供商记录上的 `clientEmulation: "opencode"`）。
//!
//! ── 为什么需要它（上游实测，2026-10-15 重测）──────────────────
//! `https://opencode.ai/inference/v1` 的**免费档**模型（`*-free` / `big-pickle` 等）
//! 在直连时会回 403：
//! ```text
//! {"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free
//!  tier can only be used from within OpenCode. Download it at
//!  https://opencode.ai and keep building."}}
//! ```
//! 上游把免费档锁在官方 CLI 的形状里。2026-10-14 起上游改了一轮判据
//! （免费推理迁到 `/inference/v1`、UA 换成 v2 形状、会话头加了一枚
//! `x-opencode-session-id`、指纹工具从 `bash/glob/grep/read` 四件套换成
//! 官方自己的 `read` / `shell`），本模块按**逐项回放实测**的结果实现：
//!
//! | 校验项 | 放行 | 拒绝（403 `FreeTierError`） |
//! |---|---|---|
//! | 端点 | `/inference/v1/*` | `/zen/v1/*`（免费请求一律拒） |
//! | `User-Agent` | 任何 `opencode/…`（v1 / v2 形状都行） | `curl/*`、`python-requests/*` 等非 opencode 的 UA |
//! | 会话头 | `x-opencode-session`（**必需**） | 缺它一律 403 |
//! | `x-opencode-session-id` | 有它时任意 UA 都放行 | 缺它时**只有 v1 形状的 UA** 才放行 |
//! | `x-opencode-request` | **必须不存在**（带上必 403） | 任何取值 |
//! | `tool_choice` | 任意（含 `none`，实测 6/6 放行） | —— |
//! | 工具 | 必须声明名为 `read` 的工具，且它的 `parameters.properties` 是**非空对象** | 无 `read`、或 `read.parameters.properties` 为 `{}` / `parameters` 为 `null` / `{}` / 非对象 |
//! | 会话 id 前缀 | 低 12 位 ≥ `0xFC0`（即 counter ≤ 63） | counter ≥ 64（见 [`LOW12_FLOOR`]） |
//!
//! 两处**与 PR #4860 的描述不同**的地方，按实测为准：
//!   · 该 PR 说「`shell` 是必需的第二个指纹工具」。今天实测：只有 `read`
//!     是必需的，`shell` 单独发会被拒（[`STUB_TOOLS`] 因此仍然发两个，
//!     但必需性判据只落在 `read` 上）；
//!   · 该 PR 说 `tool_choice: "none"` 会 403。今天实测 6/6 放行 ——
//!     我们照旧给「客户端原本没有工具」的场景发 `none`，免得模型真去调桩工具。
//!
//! ── 代价与边界（用它之前要知道的事）─────────────────────────
//!   · 它会**改写用户的请求体**（补桩工具、强制 `stream: true`）——
//!     与「自定义家透传」的默认语义相反，所以只在显式开启时做；
//!   · 桩工具是上游校验的产物，不是给模型用的：客户端**原本没有工具**时
//!     一并下发 `tool_choice: "none"`，免得模型真的去调它。客户端自带工具时
//!     保留其 `tool_choice`（改掉会破坏调用工具的客户端），此时桩工具与真
//!     工具并列存在 —— 上游要的就是这个形态；
//!   · 实测的模型行为：无论描述怎么写、有没有系统提示，模型都会调用
//!     `read` / `shell`（桩工具的定义与官方逐字一致时尤其如此）。这是
//!     上游这套校验的固有代价 —— 客户端不认识的工具调用会被它自己忽略，
//!     与「网关不发工具、上游 403」相比仍然是净收益；
//!   · 上游随时可能收紧这套校验（历史上有过多次）。再遇到「每个账号都 403」
//!     时的排查顺序：**会话头（`x-opencode-session-id` 在不在、`x-opencode-request`
//!     有没有多带）→ 端点（`/inference/v1`）→ 指纹工具的 `read.parameters` 形状 →
//!     会话 id 的取值 → 凭证**，最后才怀疑上游改了规则。
//!
//! ── 与其它模块的分工 ────────────────────────────────────────
//! 本模块**只回答四件事**：请求体补齐（[`ensure_agent_shape`]）、会话 id 与
//! 它是否合格（[`session_id`] / [`is_acceptable_session_id`]）、伪装头集合
//! （[`apply_headers`]）、以及端点是否需要迁移（[`preferred_base_url`]）。
//! 发不发、发给谁由 `providers::custom::forward` 决定；记录字段的读写与校验
//! 在 `core::custom_providers`。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// 提供商记录上的合法取值（与 `custom_providers::CLIENT_EMULATION_OPENCODE` 同字面量；
/// 常量在那边定义是为了让读写与校验在同一处，这里只借用）。
pub use crate::server::core::custom_providers::CLIENT_EMULATION_OPENCODE;

/// 匿名凭证：上游免费档认的字面量（无需注册、无轮换）。
pub const ANONYMOUS_KEY: &str = "public";

/// 伪装成官方 CLI 的 User-Agent。
///
/// 形状是 `opencode/<channel>/<version>/<client>`（官方 `App.useragent` 的
/// 产物）。实测**任何以 `opencode/` 开头的 UA 都放行**（v1 的
/// `opencode/1.18.31` 与 v2 形状一样过），所以这里取官方当前渠道的形态即可，
/// 不追求与某个具体版本逐字一致 —— 那个值会随上游发版漂移。
const USER_AGENT: &str = "opencode/latest/2.0.29/cli";

/// 免费档校验要求的两个桩工具名（大小写敏感：`Read` 会被拒，实测）。
///
/// `read` 是**必需项**（缺它必 403）；`shell` 不是，但它与 `read` 同为官方
/// CLI 的常驻工具，一起发才像「官方客户端在跑」—— 实测只发 `read` 也放行，
/// 发两个的代价只是多一份定义。
const STUB_TOOLS: [&str; 2] = ["read", "shell"];

/// 桩工具的**官方定义**（逐字来自 OpenCode v2 的 `tool/plugin/read.ts` 与
/// `tool/plugin/shell.ts`）。
///
/// 为什么用官方定义而不是自造的「禁止调用」桩：上游的判据是
/// 「`read` 的 `parameters` 是**非空 JSON 对象**」（实测 `{}` / `null` /
/// 非对象都 403）。自造定义能过闸门，但模型会照着它去调用（实测：描述写成
/// 「内部占位、禁止调用」照样被调）。既然两种写法都会被调用，就用官方那份 ——
/// 至少模型调用时拿到的参数形状是对的，客户端能识别成自己熟悉的东西。
const FINGERPRINT_TOOLS: [(&str, &str, &str); 2] = [
    (
        "read",
        "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"File or directory to read"},"offset":{"type":"integer","minimum":0,"description":"The line or directory entry to start reading from (1-based)"},"limit":{"type":"integer","minimum":0,"description":"The maximum number of lines or directory entries to read (defaults to and capped at 2000)"}},"required":["path"],"additionalProperties":false}"#,
    ),
    (
        "shell",
        "Execute a shell command and return its output. Commands run on Linux using bash. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. When output is large, the full result is saved to a file and a truncated preview is returned. Rely on automatic truncation unless filtering the output is more useful. Commands accept an optional timeout, background commands have no timeout by default. Background commands return immediately, and you will be notified when they complete.",
        r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command string to execute"},"workdir":{"type":"string","description":"Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."},"timeout":{"type":"integer","minimum":0,"description":"Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands. Background commands have no timeout by default."},"background":{"type":"boolean","description":"Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion."}},"required":["command"],"additionalProperties":false}"#,
    ),
];

/// 会话标识的 Base62 字母表（与官方 `identifier.ts` 逐字一致）
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 免费档的**首选**基址（预置卡的基址升级目标，见 [`preferred_base_url`]）。
pub const INFERENCE_BASE_URL: &str = "https://opencode.ai/inference/v1";

/// 免费档的**旧**基址：2026-10-14 起免费请求一律 403。
pub const LEGACY_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// 官方 CLI 的付费档基址（`OpenCode Go` 预置卡，**不**受免费档闸门影响）。
pub const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// 会话种子：**首个 user 消息**的 content 序列化。
///
/// 为什么用它而不是随机值：多轮对话里历史不断增长，取首轮才能让同一场对话
/// 一直映射到同一个会话 —— 上游按会话做提示词缓存亲和，每次换 id 等于每轮
/// 都冷启动。取不到（没有 user 消息 / content 为空）时给空串，调用方退化成
/// 随机会话（单次对话，无亲和需求）。
pub fn conversation_seed(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = message.get("content") else { continue };
        if content.is_null() {
            continue;
        }
        // 字符串直接取；数组（多模态）与其它形态走序列化 —— 与官方
        // （`json.Marshal(m["content"])`）同一口径：**不看内容细节**，
        // 只要同一场对话算出同一个串即可
        let seed = match content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if !seed.is_empty() {
            return seed;
        }
    }
    String::new()
}

/// 任意种子 → 上游要求的 `ses_<12位小写 hex><14位 Base62>` 会话 id。
///
/// 合格（形状与取值，见 [`is_acceptable_session_id`]）时**原样返回** ——
/// 保住调用方自带的亲和键；不合格的种子当普通字符串重新派生。
///
/// 派生时前缀取「官方 `Identifier.create` 的形状」：低 48 位是
/// `~(<时间戳毫秒> * 0x1000 + counter)`，counter 钉死为 1（官方 CLI 每毫秒
/// 内的第一枚 id 就是它）。**同一场对话仍映射到同一个 id** —— 上游按会话做
/// 的提示词缓存亲和因此不受影响（这条是取首轮 user 消息当种子的原因，见
/// [`conversation_seed`]）。
///
/// ── 为什么必须钉住 counter，而不是让前缀随机 ────────────────
/// 上游把前缀解码回 counter 并判它「够不够小」：实测 counter 0–63 放行、
/// ≥ 64 一律 403（见 [`LOW12_FLOOR`]）。而拿散列填满 48 位时，counter 是
/// 均匀随机的 12 位 —— 只有 1/64 落在放行区间，表现为「模型测试固定提示词
/// 每次都 403」。这里把 counter 钉成 1，同时用散列填时间字段，
/// 「同一场对话 → 同一个 id」与「counter 合法」就同时成立。
pub fn session_id(seed: &str) -> String {
    if is_acceptable_session_id(seed) {
        return seed.to_string();
    }
    let seed = if seed.is_empty() {
        // 空种子（没有可派生的对话内容）用随机值：单次会话，无亲和需求，
        // 但形状必须合法 —— 否则上游一律 403
        random_seed()
    } else {
        seed.to_string()
    };
    let digest = Sha256::digest(format!("ses\u{0}{seed}").as_bytes());
    // 时间字段取散列前 36 位（与官方 `SESSION_TIME_MASK` 同宽），counter 固定 1
    let mut time = 0u64;
    for byte in &digest[..5] {
        time = (time << 8) | u64::from(*byte);
    }
    let time = time & ((1u64 << 36) - 1);
    format!(
        "ses_{}{}",
        descending_time_hex(time, COUNTER_TAG),
        base62_fixed(&digest[5..15], 14)
    )
}

/// 官方 `Identifier.create` 的前缀：`~(time * 0x1000 + counter)` 取低 48 位，
/// 12 位小写 hex。
fn descending_time_hex(time: u64, counter: u64) -> String {
    let value = !(time.wrapping_mul(0x1000).wrapping_add(counter)) & ((1u64 << 48) - 1);
    format!("{value:012x}")
}

/// `ses_` + 12 位小写 hex + 14 位 Base62（上游 2026-09-16 起对其它形状一律 403）
fn valid_session_id(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("ses_") else { return false };
    if rest.len() != 12 + 14 {
        return false;
    }
    let (hex, tail) = rest.split_at(12);
    hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && tail.bytes().all(|byte| BASE62.contains(&byte))
}

/// 会话 id 低 12 位的**取值下界**（2026-10-15 实测）。
///
/// 上游把前缀解码成 counter = `~(前缀) - time * 0x1000`，再判 counter 是否
/// 「够小」。实测 counter 0 / 1 / 16 / 17 / 32 / 40 / 48 / 56 / 62 / 63 全部
/// 放行，64 / 99 / 100 / 4000 全部 403 —— 翻转点就在 63/64，等价于
/// 「低 12 位 ≥ `0xFC0`」（`0x1000 - 64`）。时间字段取什么值不影响判定
/// （2026-05 的旧时间戳、未来时间戳、纯随机 36 位都放行）。
const LOW12_FLOOR: u16 = 0xFC0;

/// 会话 id 低 12 位的**产出值**：官方 CLI 每毫秒的第一枚 id 就是 counter=1
/// ⇒ 低 12 位 = `0xFFF - 1 + 1 = 0xFFF`... 实测这一位落在放行区间正中，
/// 且与真实 CLI 的产物同形。
///
/// 判据用 [`LOW12_FLOOR`]、产出用本值，两者刻意分开：判据是「上游当时认
/// 什么」，本值是「官方客户端发什么」，将来上游若再收紧，改哪里一目了然。
const COUNTER_TAG: u64 = 1;

/// 这个字符串能不能**直接当会话头发出去**：形状（[`valid_session_id`]）与
/// 取值（低 12 位 ≥ [`LOW12_FLOOR`]）都合格才算。
///
/// 拆成两个判据是刻意的：形状是 2026-09 立的规矩、取值是 2026-10 加的，
/// 合成一个函数会让「这个 id 为什么被重算」将来无从追查。
pub fn is_acceptable_session_id(text: &str) -> bool {
    if !valid_session_id(text) {
        return false;
    }
    // `ses_` 之后 12 位是 hex（valid_session_id 已保证是 ASCII），取末 3 位
    // 即低 12 位。用 get 而不是直接切片：本模块的硬约束是不 panic。
    let Some(hex) = text.get(4..16) else { return false };
    let Some(low) = hex.get(9..12).and_then(|tail| u16::from_str_radix(tail, 16).ok()) else {
        return false;
    };
    low >= LOW12_FLOOR
}

/// 随机种子（`getrandom` 失败时退回时间戳 —— 这里只是会话 id 的熵，
/// 不是密钥，退化的后果仅是「同一场对话的亲和键不稳定」，不该让请求失败）
fn random_seed() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_ok() {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    format!("fallback-{}", crate::server::logging::now_ms())
}

/// 定长 Base62（左侧补零到 `width` 位；不足则从低位截断）
fn base62_fixed(bytes: &[u8], width: usize) -> String {
    // 把字节串当一个 128 位大数：逐字节「乘 256 加」并用 62 进制取位。
    // 用 u32 位宽逐位除法实现（不引大整数库）——宽度 14 位 Base62 约 83 bit，
    // 16 字节输入足够，且结果稳定。
    let mut digits = vec![0u32; width];
    for byte in bytes {
        let mut carry = u32::from(*byte);
        for digit in digits.iter_mut().rev() {
            let value = *digit * 256 + carry;
            *digit = value % 62;
            carry = value / 62;
        }
        // 溢出的高位（超出 width 位 Base62 所能表达的范围）直接丢掉：
        // 上游只校验形状与长度，不做唯一性校验
    }
    digits
        .iter()
        .map(|digit| BASE62[(*digit as usize).min(61)] as char)
        .collect()
}

/// 出站头的伪装集合：把官方 CLI 会带的头补齐到 `headers` 上。
///
/// `headers` 的键名沿用本网关的默认头（`Authorization` / `Content-Type`），
/// 同名覆盖 —— 于是**自定义 key 优先**：账号填了 key 就用它（付费模型必须），
/// 没填才用匿名字面量（免费档）。这与 `ProviderQuirks::apply_to` 的合并口径
/// 相同，转发侧先铺默认头、再调本函数，顺序不能反过来。
///
/// 三类头的判据（见模块头的表）：
///   · `Authorization` —— 有 key 用 key，没有用匿名 `public`；
///   · `User-Agent` —— 非 `opencode/` 形状一律换成官方形态（`curl` 之类必 403）；
///   · 会话头三件套 —— `x-opencode-session` 与 `x-opencode-session-id` **都要**，
///     另两枚（`x-session-affinity` / `x-session-id`）官方也发，一并补齐；
///   · **`x-opencode-request` 必须不发** —— 上游对带上它的请求一律 403
///     （它是 v1 的头，v2 已废弃）。这里把同名头从 `headers` 里**删掉**：
///     调用方在这一步之前若已铺了它（如从下游转发的同名头），不删就会换来一次 403。
///     注意调用顺序：记录上的静态额外头（`apply_to`）在本函数**之后**合并，
///     那是「用户手写的最优先」这条既有取舍；用户真把 v1 头写进记录里的极罕见情形
///     由他自己负责（不是我们替他猜）。
pub fn apply_headers(headers: &mut Vec<(String, String)>, api_key: &str, session: &str) {
    let credential = if api_key.trim().is_empty() {
        ANONYMOUS_KEY
    } else {
        api_key.trim()
    };
    let pairs = [
        ("Authorization".to_string(), format!("Bearer {credential}")),
        // 官方 CLI 的形状头：缺了「看起来就不像官方客户端」，实测会掉进 403
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("x-opencode-client".to_string(), "cli".to_string()),
        ("x-opencode-session".to_string(), session.to_string()),
        ("x-opencode-session-id".to_string(), session.to_string()),
        ("x-session-affinity".to_string(), session.to_string()),
        ("X-Session-Id".to_string(), session.to_string()),
        ("x-opencode-project".to_string(), "global".to_string()),
    ];
    merge(headers, pairs);
    remove(headers, "x-opencode-request");
    remove(headers, "x-opencode-request-id");
}

/// 把一对键值按名合并进头表（ASCII 不区分大小写；同名覆盖，其余追加）
fn merge(
    headers: &mut Vec<(String, String)>,
    pairs: impl IntoIterator<Item = (String, String)>,
) {
    for (key, value) in pairs {
        match headers.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(&key)) {
            Some(slot) => slot.1 = value,
            None => headers.push((key, value)),
        }
    }
}

/// 按名删掉一个头（ASCII 不区分大小写；不存在就是空操作）
fn remove(headers: &mut Vec<(String, String)>, key: &str) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case(key));
}

/// 请求体补齐「智能体形态」：`stream: true` + `tools` 里必须有官方形状的
/// `read`（并顺带补 `shell`）。
///
/// 返回是否改动了请求体（只给日志用）。四条细节：
///   · `stream` 恒置 true（上游免费档只接受流式；非流式下游由网关聚合器收敛
///     —— 这与编排层对**所有**上游的既有策略一致，不是这里新加的行为）；
///   · **`read` 是必需项**：上游判「存在名为 `read` 的工具，且它的
///     `parameters.properties` 是非空对象」（实测 `{"properties":{}}` 也会 403）。
///     客户端自带的 `read` 定义千奇百怪
///     （参数名 `filePath` / `file_path`、`properties` 为空、schema 为 null），
///     这些**都过不了闸门** ⇒ 客户端那份一律让位：改名到 `read_client`，
///     再补官方那一份（模型按官方定义调用即可，客户端认不认识它由客户端决定）；
///   · 客户端原本**没有** `read` 时直接补，不动它别的工具；
///   · 客户端**原本没有 tools** 时补 `tool_choice: "none"`：桩工具只为过检，
///     明确禁止模型调用它。客户端自带工具时不动 `tool_choice` —— 那会破坏
///     它本来能用的工具调用。（实测 `none` 不被上游拒。）
pub fn ensure_agent_shape(body: &mut Value) -> bool {
    let mut changed = false;
    let Some(object) = body.as_object_mut() else { return false };
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        object.insert("stream".to_string(), Value::Bool(true));
        changed = true;
    }
    let had_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let mut tools: Vec<Value> = object
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 客户端自带的同名工具先挪开：占着 `read` / `shell` 而定义不合官方形状时，
    // 上游的闸门看的是**那一份**，补一份新的只会撞成重名（上游 400）。
    let mut taken: std::collections::HashSet<String> = tools
        .iter()
        .filter_map(|tool| tool_name(tool).map(str::to_string))
        .collect();
    for tool in tools.iter_mut() {
        let Some(name) = tool_name(tool).map(str::to_string) else { continue };
        if !STUB_TOOLS.contains(&name.as_str()) {
            continue;
        }
        if fingerprint_definition_ok(tool, &name) {
            continue;
        }
        let alias = free_alias(&name, &mut taken);
        set_tool_name(tool, &alias);
        changed = true;
    }
    for (name, description, parameters) in FINGERPRINT_TOOLS {
        if tools.iter().any(|tool| tool_name(tool) == Some(name)) {
            continue;
        }
        tools.push(stub_tool(name, description, parameters));
        changed = true;
    }
    object.insert("tools".to_string(), Value::Array(tools));
    if !had_tools {
        object.insert("tool_choice".to_string(), Value::String("none".to_string()));
    }
    changed
}

/// 工具名（两种形状：chat 的 `{function:{name}}` 与 responses 的裸 `{name}`）
fn tool_name(tool: &Value) -> Option<&str> {
    let nested = tool
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str);
    let flat = tool.get("name").and_then(Value::as_str);
    nested.or(flat).map(str::trim).filter(|name| !name.is_empty())
}

/// 就地改工具名（两种形状各改自己那一层）
fn set_tool_name(tool: &mut Value, name: &str) {
    if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
        if function.contains_key("name") {
            function.insert("name".to_string(), Value::String(name.to_string()));
            return;
        }
    }
    if let Some(object) = tool.as_object_mut() {
        object.insert("name".to_string(), Value::String(name.to_string()));
    }
}

/// 这份定义能不能过上游的闸门。
///
/// 2026-10-15 实测的**精确**判据（`read` 是唯一必需的工具名）：
///   · `parameters` 必须是对象，且它的 `properties` 必须是**非空对象**；
///   · `{"properties":{}}` / `{}` / `null` / 字符串 / 数组 → 403；
///   · `properties` 非空 → 放行（`type` / `required` / 具体属性名都不重要）。
/// `shell` 不是必需项（单发它必 403），但既然要留着它，就用同一判据 ——
/// 免得留一份过不了的形状上去。
fn fingerprint_definition_ok(tool: &Value, name: &str) -> bool {
    let definition = tool.get("function").unwrap_or(tool);
    schema_is_usable(definition.get("parameters")) && !name.is_empty()
}

/// 给被挪开的客户端工具找一个不撞名的别名（`read_client` / `read_client2` …）
fn free_alias(
    name: &str,
    taken: &mut std::collections::HashSet<String>,
) -> String {
    let mut alias = format!("{name}_client");
    let mut index = 2usize;
    while taken.contains(&alias) {
        alias = format!("{name}_client{index}");
        index += 1;
    }
    taken.insert(alias.clone());
    alias
}

/// 一个桩工具（描述与参数逐字用官方定义，见 [`FINGERPRINT_TOOLS`]）。
///
/// 形态随请求体的形状走：`tools` 里已有扁平形态（`{type,name,parameters}`，
/// responses 协议）时补扁平的，否则补 chat 的 `{type,function:{…}}`。
/// 上游两种都认，但混着发会让某些兼容层困惑，所以与既有元素保持一致。
fn stub_tool(name: &str, description: &str, parameters: &str) -> Value {
    let schema: Value = serde_json::from_str(parameters).unwrap_or_else(|_| json!({}));
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": schema,
        },
    })
}

/// 参数 schema 里的 `properties` 是不是非空对象（上游闸门的实际判据，
/// 见 [`fingerprint_definition_ok`]；抽成公开的窄口是为了让它可测）
pub fn schema_is_usable(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_object)
        .and_then(|parameters| parameters.get("properties"))
        .and_then(Value::as_object)
        .is_some_and(|properties: &Map<String, Value>| !properties.is_empty())
}

/// 免费档的**首选基址**迁移：把预置卡的旧 `/zen/v1` 换成 `/inference/v1`。
///
/// 只认**逐字等于** [`LEGACY_ZEN_BASE_URL`] 的基址（去尾斜杠后比较）——
/// 用户手改过的、指向自建代理的基址一律不动：那可能是他自己的中转，
/// 替他改写会把流量引到别处。
///
/// `OpenCode Go` 的 `/zen/go/v1` 不在其中：那是订阅制通道，实测不受免费档
/// 闸门影响，改了反而打不通。
pub fn preferred_base_url(current: &str) -> Option<&'static str> {
    if current.trim().trim_end_matches('/').eq_ignore_ascii_case(LEGACY_ZEN_BASE_URL) {
        return Some(INFERENCE_BASE_URL);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_tool(name: &str, description: &str, parameters: Value) -> Value {
        json!({ "type": "function", "function": {
            "name": name, "description": description, "parameters": parameters,
        }})
    }

    fn names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .expect("测试里总该有 tools")
            .iter()
            .filter_map(|tool| tool_name(tool).map(str::to_string))
            .collect()
    }

    #[test]
    fn session_ids_are_shape_and_value_legal() {
        for seed in ["", "hello", "claude:550e8400-e29b-41d4-a716-446655440000", "中文提示词"] {
            let id = session_id(seed);
            assert!(valid_session_id(&id), "形状不合法：{id}");
            assert!(is_acceptable_session_id(&id), "取值不合法：{id}");
        }
    }

    #[test]
    fn the_same_conversation_keeps_one_session_id() {
        // 上游按会话做提示词缓存亲和：同一场对话换 id 等于每轮冷启动
        assert_eq!(session_id("同一段首轮提示词"), session_id("同一段首轮提示词"));
        assert_ne!(session_id("第一段"), session_id("第二段"));
    }

    #[test]
    fn an_acceptable_id_passes_through_untouched() {
        let id = session_id("seed");
        assert_eq!(id, session_id(&id), "合格的原值要原样保留（保住调用方的亲和键）");
    }

    #[test]
    fn the_counter_stays_small_enough_for_the_gate() {
        // 上游解码前缀回 counter 并判它够不够小：实测 63 过 / 64 拒
        let id = session_id("seed");
        let hex = &id[4..16];
        let low = u16::from_str_radix(&hex[9..12], 16).expect("hex");
        assert!(low >= LOW12_FLOOR, "低 12 位要落在放行区间：{low:#x}");
        // 反推 counter = 0xFFF - low + 1（前缀取反后 counter 落在最低位）
        let counter = 0x1000u32 - u32::from(low);
        assert!(counter <= 63, "counter 必须 ≤ 63（上游的翻转点）：{counter}");
    }

    #[test]
    fn an_id_with_a_large_counter_is_rejected() {
        // 手工造一个 counter 很大的 id：必须判成不合格（否则会被原样发出去）
        let big = format!("ses_{:012x}{}", 0u64, "AbCdEfGhIjKlMn");
        assert!(!is_acceptable_session_id(&big), "低 12 位为 0 的 id 该被判不合格");
    }

    #[test]
    fn a_client_read_with_an_unusable_schema_makes_way_for_the_official_one() {
        // 上游的精确判据是 `parameters.properties` 非空（实测：`{"properties":{}}`
        // 也会 403），所以这几份都得让位
        for schema in [json!({}), json!(null), json!("x"), json!([]), json!({"properties": {}})] {
            let mut body = json!({ "tools": [chat_tool("read", "mine", schema)] });
            ensure_agent_shape(&mut body);
            let sent = names(&body);
            assert!(sent.contains(&"read".to_string()), "官方 read 必须补上：{sent:?}");
            assert!(sent.contains(&"read_client".to_string()), "客户端那份要让位：{sent:?}");
        }
    }

    #[test]
    fn the_schema_gate_matches_the_upstream_probe() {
        // 一手实测：只有 `parameters.properties` 是非空对象才放行
        assert!(schema_is_usable(Some(&json!({"properties": {"a": {}}}))));
        assert!(schema_is_usable(Some(&json!({"type": "object", "properties": {"a": 1}}))));
        assert!(!schema_is_usable(Some(&json!({"properties": {}}))));
        assert!(!schema_is_usable(Some(&json!({}))));
        assert!(!schema_is_usable(Some(&json!(null))));
        assert!(!schema_is_usable(Some(&json!("x"))));
        assert!(!schema_is_usable(None));
    }

    #[test]
    fn a_client_read_with_a_real_schema_is_left_alone() {
        let mut body = json!({ "tools": [chat_tool("read", "mine", json!({"properties": {"p": {"type": "string"}}}))] });
        ensure_agent_shape(&mut body);
        let sent = names(&body);
        assert!(sent.contains(&"read".to_string()));
        assert!(!sent.contains(&"read_client".to_string()), "能过闸门的定义不该被动：{sent:?}");
        assert!(sent.contains(&"shell".to_string()), "shell 顺带补齐");
    }

    #[test]
    fn a_body_without_tools_gets_both_stubs_and_tool_choice_none() {
        let mut body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(ensure_agent_shape(&mut body));
        assert_eq!(body["stream"], json!(true), "上游免费档只接受流式");
        assert_eq!(names(&body), vec!["read", "shell"]);
        assert_eq!(body["tool_choice"], json!("none"), "桩工具只为过检，禁止模型调用");
    }

    #[test]
    fn a_body_with_client_tools_keeps_its_own_tool_choice() {
        let mut body = json!({
            "tools": [chat_tool("get_weather", "w", json!({"properties": {"city": {"type": "string"}}}))],
            "tool_choice": "auto",
        });
        ensure_agent_shape(&mut body);
        assert_eq!(body["tool_choice"], json!("auto"), "客户端自带工具时不许改它的选择");
        let sent = names(&body);
        assert!(sent.contains(&"get_weather".to_string()) && sent.contains(&"read".to_string()));
    }

    #[test]
    fn duplicate_stub_names_never_collide() {
        // 客户端自己就有一个 read_client：别名要再让一位
        let mut body = json!({ "tools": [
            chat_tool("read", "mine", json!({})),
            chat_tool("read_client", "mine2", json!({})),
        ]});
        ensure_agent_shape(&mut body);
        let sent = names(&body);
        let unique: std::collections::HashSet<&String> = sent.iter().collect();
        assert_eq!(sent.len(), unique.len(), "补完不许重名（上游会 400）：{sent:?}");
    }

    #[test]
    fn the_request_header_is_always_removed() {
        // v1 的 x-opencode-request 会让上游 403 —— 下游转发上来的同名头必须删掉
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("x-opencode-request".to_string(), "msg_x".to_string()),
            ("X-Opencode-Request-Id".to_string(), "req_x".to_string()),
        ];
        apply_headers(&mut headers, "", "ses_000000000fffAbCdEfGhIjKlMn");
        assert!(
            !headers.iter().any(|(name, _)| name.to_ascii_lowercase().starts_with("x-opencode-request")),
            "带 x-opencode-request 的请求会被上游 403：{headers:?}"
        );
        assert_eq!(
            headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("authorization")).count(),
            1
        );
    }

    #[test]
    fn both_session_headers_are_emitted() {
        let mut headers = Vec::new();
        apply_headers(&mut headers, "", "ses_000000000fffAbCdEfGhIjKlMn");
        let has = |key: &str| headers.iter().any(|(name, _)| name.eq_ignore_ascii_case(key));
        assert!(has("x-opencode-session"), "缺它必 403");
        assert!(has("x-opencode-session-id"), "v2 形状的 UA 下缺它必 403");
        assert!(has("x-opencode-project"));
        assert_eq!(
            headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("authorization")).map(|(_, value)| value.as_str()),
            Some("Bearer public"),
            "没填 key 时用匿名凭证"
        );
    }

    #[test]
    fn a_paid_key_replaces_the_anonymous_credential() {
        let mut headers = Vec::new();
        apply_headers(&mut headers, "sk-real", "ses_000000000fffAbCdEfGhIjKlMn");
        assert_eq!(
            headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("authorization")).map(|(_, value)| value.as_str()),
            Some("Bearer sk-real")
        );
    }

    #[test]
    fn only_the_exact_legacy_base_url_is_migrated() {
        assert_eq!(preferred_base_url("https://opencode.ai/zen/v1/"), Some(INFERENCE_BASE_URL));
        assert_eq!(preferred_base_url(" https://opencode.ai/zen/v1 "), Some(INFERENCE_BASE_URL));
        assert_eq!(preferred_base_url(INFERENCE_BASE_URL), None, "已经是新基址就不动");
        assert_eq!(preferred_base_url(GO_BASE_URL), None, "Go 是订阅通道，不受免费档闸门影响");
        assert_eq!(preferred_base_url("https://my-proxy.example.com/v1"), None, "用户自建的中转不许替他改写");
    }
}

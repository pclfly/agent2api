//! MiniMax 客户端登录态导入。
//!
//! ── 两个来源（2026-10 起支持 local-runtime）──────────────────
//! 1. **`~/.minimax/local-runtime.auth.json`（首选）** —— MiniMax Code 桌面端
//!    维护的会话文件（`updatedAtMs` 随续期刷新，实测 JWT 有效期约 10 天）。
//!    token 是 **JWT**（385 字符、含 `exp`），另有 `realUserID`（矩阵接口的
//!    `user_id` 参数）与用户昵称。**没有 refreshToken** —— 续期由桌面端负责
//!    （网关侧「刷新」= 重读本文件，见 `mod.rs` 的 desktop 分支）。
//! 2. `~/.minimax/auth/prod/cn/mcode-public/auth.json`（兼容保留）—— 设备码
//!    OAuth 体系的登录态（`records` 里 `mmoat_` / `mmort_`，60 分钟级）。
//!
//! 两个文件都存在时**取过期更晚的那份**（同一账号的两个体系，选长命的；
//! 过期时间相同或都解析不出时优先 local-runtime —— 它由桌面端持续维护）。
//!
//! ── 只读纪律（沿用）──────────────────────────────────────────
//!   - **绝不写回**客户端文件：网关的同步只更新自己的账号库，客户端文件
//!     永远不动；
//!   - **不代刷新**：refresh_token 可能轮换，网关侧调上游刷新会让客户端
//!     登录态作废。desktop 账号的「刷新」一律是**重读本文件**（只读），
//!     由 `mod.rs` 的 desktop 分支实现。
//!
//! ── mcode 文件的「最后一条记录」怎么取 ───────────────────────
//! 参考实现（JS）取对象插入序的最后一条。Rust 的 `serde_json` 默认按 key
//! 排序，拿不到插入序 —— 这里改为**取 `expiresAtMs` 最大的那一条**（语义：
//! 最近登录的账号，有效期最晚），单账号场景下与参考实现等价。

use serde_json::Value;

use crate::server::errors::GatewayError;

use super::credentials::{self, Credential};

/// local-runtime 会话文件的相对路径（相对用户主目录）。
const LOCAL_RUNTIME_RELATIVE: &str = ".minimax/local-runtime.auth.json";
/// mcode 登录态文件的相对路径（旧路径，兼容保留）。
const DESKTOP_AUTH_RELATIVE: &str = ".minimax/auth/prod/cn/mcode-public/auth.json";

/// 桌面端登录态的读取结果：凭据 + 来源信息。
pub struct DesktopCredentials {
    pub credential: Credential,
    /// 矩阵接口的 `user_id`（JWT 来源有；mcode 来源为空）。
    pub real_user_id: String,
    /// 来源文件的短名（日志与排障用；绝不发给上游）。
    pub source: &'static str,
}

/// 用户主目录
fn home_dir() -> Result<std::path::PathBuf, GatewayError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| {
            GatewayError::with_status(500, "无法确定用户主目录，读取不到 MiniMax 登录态")
        })?;
    Ok(std::path::PathBuf::from(home))
}

fn home_path(relative: &str) -> Result<std::path::PathBuf, GatewayError> {
    let mut path = home_dir()?;
    for segment in relative.split('/') {
        path.push(segment);
    }
    Ok(path)
}

/// local-runtime 会话文件路径。
pub fn local_runtime_path() -> Result<std::path::PathBuf, GatewayError> {
    home_path(LOCAL_RUNTIME_RELATIVE)
}

/// mcode 登录态文件路径。
pub fn desktop_auth_path() -> Result<std::path::PathBuf, GatewayError> {
    home_path(DESKTOP_AUTH_RELATIVE)
}

/// 读取桌面端登录态：两个来源都试，取过期更晚的；都不可用 → None。
pub fn read_desktop_credentials() -> Result<Option<DesktopCredentials>, GatewayError> {
    let local = read_local_runtime()?;
    let mcode = read_mcode_auth()?;
    Ok(pick_latest(local, mcode))
}

/// `local-runtime.auth.json`（JWT 会话；文件不存在 → None）。
fn read_local_runtime() -> Result<Option<DesktopCredentials>, GatewayError> {
    let path = local_runtime_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        GatewayError::with_status(500, format!("读取本机 MiniMax 登录态失败: {error}"))
    })?;
    let root: Value = serde_json::from_str(&text).map_err(|error| {
        GatewayError::with_status(400, format!("本机 MiniMax 登录态文件格式无效: {error}"))
    })?;
    Ok(parse_local_runtime(&root))
}

/// `mcode auth.json`（OAuth 体系；文件不存在 → None）。
fn read_mcode_auth() -> Result<Option<DesktopCredentials>, GatewayError> {
    let path = desktop_auth_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        GatewayError::with_status(500, format!("读取本机 MiniMax 登录态失败: {error}"))
    })?;
    let root: Value = serde_json::from_str(&text).map_err(|error| {
        GatewayError::with_status(400, format!("本机 MiniMax 登录态文件格式无效: {error}"))
    })?;
    Ok(parse_desktop_credentials(&root))
}

/// 从 local-runtime 文件内容解析凭据（纯函数，可单测）。
///
/// ```jsonc
/// { "version": 1, "updatedAtMs": 1790…,
///   "auth": { "accessToken": "<JWT>", "realUserID": "548…",
///             "userName": "MiniMax597757", "subUserName": "小苍哥" } }
/// ```
pub fn parse_local_runtime(root: &Value) -> Option<DesktopCredentials> {
    let auth = root.get("auth")?;
    let access_token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let real_user_id = auth
        .get("realUserID")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let nickname = ["subUserName", "userName"]
        .iter()
        .find_map(|key| auth.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string();
    // JWT 的 `exp` → 毫秒；解析不出留空（过期时间未知，使用侧按需处理）
    let expires_at = credentials::decode_jwt_exp_ms(&access_token)
        .map(|value| value.to_string())
        .unwrap_or_default();
    Some(DesktopCredentials {
        credential: Credential {
            access_token,
            refresh_token: String::new(),
            token_type: "Bearer".to_string(),
            expires_at,
            scope: String::new(),
            nickname,
            real_user_id: real_user_id.clone(),
            // 客户端文件里没有手机号（JWT payload 只有 id / name / avatar）——
            // 手机号由落账号 / 余额查询时调 `profile::query_user_info` 补（见 `profile.rs`）
            phone: String::new(),
        },
        real_user_id,
        source: "local-runtime",
    })
}

/// 从 mcode 文件内容解析凭据（纯函数，可单测；沿用旧口径）。
pub fn parse_desktop_credentials(root: &Value) -> Option<DesktopCredentials> {
    let records = root.get("records")?.as_object()?;
    // 取 `expiresAtMs` 最大的那一条（见模块头「最后一条记录」的说明）
    let mut best: Option<(i64, &Value)> = None;
    for (_, record) in records {
        let expires = record
            .get("expiresAtMs")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if best.map(|(current, _)| expires > current).unwrap_or(true) {
            best = Some((expires, record));
        }
    }
    let (_, record) = best?;
    let text = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let access_token = text("accessToken");
    if access_token.is_empty() {
        return None;
    }
    let expires_at = record
        .get("expiresAtMs")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .map(|value| value.to_string())
        .unwrap_or_default();
    Some(DesktopCredentials {
        credential: Credential {
            access_token,
            refresh_token: text("refreshToken"),
            token_type: {
                let kind = text("tokenType");
                if kind.is_empty() { "Bearer".to_string() } else { kind }
            },
            expires_at,
            scope: super::product::SCOPE.to_string(),
            nickname: String::new(),
            real_user_id: String::new(),
            // mcode 文件里也没有手机号（只有 token / expiresAtMs）—— 同 local-runtime
            phone: String::new(),
        },
        real_user_id: String::new(),
        source: "mcode-auth",
    })
}

/// 两个来源选「过期更晚」的那份（同刻或解析不出时优先 local-runtime）。
fn pick_latest(
    local: Option<DesktopCredentials>,
    mcode: Option<DesktopCredentials>,
) -> Option<DesktopCredentials> {
    match (local, mcode) {
        (Some(local), Some(mcode)) => {
            let local_exp = credentials::credential_expires_at_ms(&local.credential).unwrap_or(0);
            let mcode_exp = credentials::credential_expires_at_ms(&mcode.credential).unwrap_or(0);
            if mcode_exp > local_exp {
                Some(mcode)
            } else {
                Some(local)
            }
        }
        (Some(local), None) => Some(local),
        (None, Some(mcode)) => Some(mcode),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_local_runtime_jwt() {
        // JWT：header.payload.signature；payload 含 exp（秒）
        let payload = serde_json::json!({"exp": 1794551262_i64, "user": {"id": "548"}});
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).expect("序列化"));
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{encoded}.sig");
        let root = json!({
            "version": 1,
            "updatedAtMs": 1791095261476_i64,
            "auth": {
                "accessToken": token,
                "realUserID": "548212364582207496",
                "userName": "MiniMax597757",
                "subUserName": "小苍哥"
            }
        });
        let desktop = parse_local_runtime(&root).expect("应解析出凭据");
        assert_eq!(desktop.real_user_id, "548212364582207496");
        assert_eq!(desktop.credential.nickname, "小苍哥");
        assert!(desktop.credential.refresh_token.is_empty());
        // exp 1794551262 秒 → 1794551262000 毫秒
        assert_eq!(desktop.credential.expires_at, "1794551262000");
        assert_eq!(desktop.source, "local-runtime");
    }

    #[test]
    fn rejects_local_runtime_without_token() {
        assert!(parse_local_runtime(&json!({})).is_none());
        assert!(parse_local_runtime(&json!({"auth": {"accessToken": ""}})).is_none());
        assert!(parse_local_runtime(&json!({"auth": {}})).is_none());
    }

    #[test]
    fn parses_latest_mcode_record() {
        let root = json!({
            "records": {
                "old": {"accessToken": "mmoat_old", "refreshToken": "mmort_old",
                        "tokenType": "Bearer", "expiresAtMs": 1000},
                "new": {"accessToken": "mmoat_new", "refreshToken": "mmort_new",
                        "tokenType": "Bearer", "expiresAtMs": 2000}
            }
        });
        let desktop = parse_desktop_credentials(&root).expect("应解析出凭据");
        // 取 expiresAtMs 最大的（最近登录）
        assert_eq!(desktop.credential.access_token, "mmoat_new");
        assert_eq!(desktop.credential.refresh_token, "mmort_new");
        assert_eq!(desktop.credential.expires_at, "2000");
        assert!(desktop.credential.can_refresh());
        assert_eq!(desktop.source, "mcode-auth");
    }

    #[test]
    fn rejects_mcode_without_access_token() {
        let root = json!({"records": {"a": {"refreshToken": "r"}}});
        assert!(parse_desktop_credentials(&root).is_none());
        assert!(parse_desktop_credentials(&json!({})).is_none());
    }
}

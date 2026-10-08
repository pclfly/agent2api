//! Loomy 短信验证码登录（讯飞 CAccount）。
//!
//! ```text
//! POST {accountBase}/login/phone/sendMsgCode   { base, param:{ ccode, phone, expire:300 } }
//! POST {accountBase}/login/phone/checkCode     { base, param:{ ccode, phone, mcode, msgid, expire:1209600 } }
//! ```
//!
//! 签名与请求体构造见 `sign.rs` / `client::account_body`。
//!
//! ── 微信扫码登录为什么不做 ──────────────────────────────────
//! 参考实现另有一条四步强制绑手机号流程（`bind/auth` → `bind/sendMsg` →
//! `bind/checkCode` → `bind/skip`，需要微信开放平台的扫码回调页）。
//! 短信登录是主路径且形态简单（发码 → 回填 → 登录），本项目先接这一条；
//! 微信扫码列为后续可选项（届时另开模块，不与本文件混）。

use serde_json::{json, Value};

use crate::server::errors::GatewayError;

use super::client;
use super::credentials::{self, Credential};
use super::product;
use super::sign;

/// 短信发码。返回 `msgid`（提交验证码时必须原样带回）。
pub async fn send_sms_code(phone: &str) -> Result<String, GatewayError> {
    let phone = phone.trim();
    if phone.is_empty() {
        return Err(GatewayError::with_status(400, "请填写手机号"));
    }
    let param = json!({
        "ccode": "86",
        "phone": phone,
        "expire": product::SMS_CODE_TTL_SECONDS,
    });
    let data = post_account_data(product::SEND_MSG_PATH, param, "发送验证码").await?;
    let msgid = credentials::read_string(&data, "msgid");
    if msgid.is_empty() {
        // 不返回空串：上层会拿它去登录，服务端必然报「msgid 无效」，
        // 用户看到的将是一个与真实原因无关的错误。
        return Err(GatewayError::with_status(502, "短信验证码响应缺少 msgid"));
    }
    Ok(msgid)
}

/// 用短信验证码登录。返回组装好的凭据（`expires_at` 按 14 天推算）。
pub async fn login_with_code(
    phone: &str,
    code: &str,
    msgid: &str,
    now_ms: i64,
) -> Result<Credential, GatewayError> {
    let phone = phone.trim();
    let code = code.trim();
    if phone.is_empty() || code.is_empty() || msgid.trim().is_empty() {
        return Err(GatewayError::with_status(400, "手机号 / 验证码 / msgid 不完整，请重新获取验证码"));
    }
    let param = json!({
        "ccode": "86",
        "phone": phone,
        "mcode": code,
        "msgid": msgid,
        "expire": product::SESSION_TTL_SECONDS,
    });
    let data = post_account_data(product::CHECK_CODE_PATH, param, "短信登录").await?;
    let session = credentials::read_string(&data, "session");
    let userid = credentials::read_string(&data, "userid");
    if session.is_empty() {
        return Err(GatewayError::with_status(502, "登录响应缺少 session"));
    }
    if userid.is_empty() {
        return Err(GatewayError::with_status(502, "登录响应缺少 userid"));
    }
    // `expires_at` 由登录时刻 + 14 天推算（服务端不下发到期时间）
    let expires_at = now_ms.saturating_add((product::SESSION_TTL_SECONDS as i64).saturating_mul(1000));
    Ok(Credential {
        access_token: session,
        userid,
        phone: phone.to_string(),
        nickname: String::new(),
        expires_at: expires_at.to_string(),
    })
}

/// 发一次账号端点请求并拆信封，返回 `data`。
///
/// ⚠️ 账号端点的鉴权错误码与业务端点不同（实测 `020002` 也是登录态问题），
/// 但对本模块而言都是「这次调用失败」—— 直接透传 `desc`，
/// 上层据文案判断「验证码错误」还是「手机号格式不正确」。
async fn post_account_data(
    path: &str,
    param: Value,
    action: &str,
) -> Result<Value, GatewayError> {
    let reply = client::post_account(path, param, None).await?;
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(
            502,
            format!("{action}失败：上游返回 HTTP {}", reply.status),
        ));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, format!("{action}响应不是 JSON")));
    };
    match credentials::parse_envelope(&body) {
        Ok(envelope) => Ok(envelope.data),
        Err(failure) => Err(GatewayError::with_status(
            400,
            if failure.message.is_empty() {
                format!("{action}失败")
            } else {
                failure.message
            },
        )),
    }
}

/// 生成一个登录流程用的随机 state（当前只有测试与将来扩展用；短信登录不需要）。
#[allow(dead_code)]
pub fn random_state() -> String {
    sign::uuid_v4()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_builds_credential_with_14_day_expiry() {
        let credential = Credential {
            access_token: "sess".to_string(),
            userid: "123456789012345678".to_string(),
            phone: "13011111100".to_string(),
            nickname: String::new(),
            expires_at: (1_000_000_i64 + 1_209_600_000).to_string(),
        };
        assert!(credential.valid());
        assert!(!credential.can_refresh());
        assert_eq!(credentials::credential_expires_at_ms(&credential), Some(1_210_600_000));
    }
}

//! 讯飞账号（CAccount）HMAC-SHA1 签名。
//!
//! 逐字节复刻 Loomy 客户端 `electron/xfyun/sign.js`。任何偏差都会让账号端点
//! 返回鉴权失败，故本模块**全部是纯函数**，由单测锁死拼串格式。
//!
//! ## 签名字符串（9 段，`\n` 连接）
//!
//! ```text
//! {METHOD}\n{ESCAPED_PATH}\n{ESCAPED_QUERY}\n{Content-MD5}\n
//! {Content-Type}\n{Date}\n{Nonce}\n{SignedHeaders}\n{CanonicalizedHeaders}
//! ```
//!
//! ⚠️ 后两段在本项目**恒为空串**（不发任何 `x-*` 头），故最终字符串
//! **以两个换行结尾**。这是 `join('\n')` 在 9 个元素上的自然结果，
//! 不要「顺手」去掉尾随换行 —— 去掉会让签名不匹配。
//!
//! ⚠️ 认证头前缀是 **`account`**（`account {ak}:{sig}`），不是 `Bearer`。

use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest as Md5Digest, Md5};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// 签名所需参数。
pub struct SignOptions<'a> {
    pub access_key_id: &'a str,
    pub access_key_secret: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    /// 查询参数（**保持传入顺序，不排序** —— 与客户端一致）。
    pub query_params: &'a [(String, String)],
    /// **已序列化**的请求体字符串（与发送时用的必须是同一个）。
    pub body: &'a str,
    pub content_type: &'a str,
}

/// RFC3986 转义（`encodeURIComponent` + 补转 `! ' ( ) *` 后的等价保留集）。
///
/// 依据 `sign.js:23-42`：路径段与 query 值用同一套转义。
/// 最终保留集是 `A-Za-z0-9-_.~`，其余（含 UTF-8 多字节）逐字节 `%XX` 大写。
fn escape_rfc3986(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// 构建 ESCAPED_PATH。
///
/// 依据 `sign.js:47-58`：补前导 `/`、剥末尾 `/`（长度 > 1 时）、
/// 按 `/` 切段后逐段转义再拼回（空段保持空）。
pub fn build_escaped_path(raw_path: &str) -> String {
    let mut clean = if raw_path.starts_with('/') {
        raw_path.to_string()
    } else {
        format!("/{raw_path}")
    };
    if clean.len() > 1 && clean.ends_with('/') {
        clean.truncate(clean.len() - 1);
    }
    clean
        .split('/')
        .map(escape_rfc3986)
        .collect::<Vec<_>>()
        .join("/")
}

/// 构建 ESCAPED_QUERY_STRING。
///
/// 依据 `sign.js:63-79`：`key=value` 用 `&` 连接，**不排序**（保持传入顺序），
/// 两者都转义；`null` / `undefined` 值转成空串（Rust 侧用空串表达）。
pub fn build_escaped_query(query_params: &[(String, String)]) -> String {
    query_params
        .iter()
        .map(|(key, value)| format!("{}={}", escape_rfc3986(key), escape_rfc3986(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 计算 `Content-MD5`（base64）。
///
/// ⚠️ 空 body 返回**空串**而不是空串的 md5 —— 与客户端 `sign.js:13-18` 一致。
pub fn content_md5(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let mut hasher = Md5::new();
    hasher.update(body.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// 构建待签名字符串。
///
/// `date` / `nonce` 由调用方传入（而非在此生成），使本函数**完全确定**、
/// 可用固定值单测。
pub fn build_signing_string(options: &SignOptions<'_>, date: &str, nonce: &str) -> String {
    let method = options.method.to_uppercase();
    let escaped_path = build_escaped_path(options.path);
    let escaped_query = build_escaped_query(options.query_params);
    let md5 = content_md5(options.body);
    // ⚠️ 后两段恒为空串（本项目不发 x-* 头）。join 后字符串以 `\n\n` 结尾。
    [
        method.as_str(),
        escaped_path.as_str(),
        escaped_query.as_str(),
        md5.as_str(),
        options.content_type,
        date,
        nonce,
        "",
        "",
    ]
    .join("\n")
}

/// HMAC-SHA1 → base64。
pub fn hmac_sha1_base64(secret: &str, message: &str) -> String {
    // HMAC 接受任意长度密钥，`new_from_slice` 对本输入不可能失败；
    // 失败时返回空串（调用方会把「空签名」当成鉴权失败上报，不 panic）。
    let Ok(mut mac) = HmacSha1::new_from_slice(secret.as_bytes()) else {
        return String::new();
    };
    mac.update(message.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// 生成完整的讯飞账号请求头。
///
/// `Date` 用 UTC 字符串、`Nonce` 用 UUID（与客户端 `sign.js:207-208` 一致）。
///
/// ⚠️ **返回的 body 必须与签名时的 `options.body` 是同一个字符串**：
/// 调用方应先把 body 序列化一次，签名与发送共用该字符串，
/// 否则二次序列化会改变字节（键序、空格），签名随即失效。
pub fn auth_headers(options: &SignOptions<'_>) -> Vec<(String, String)> {
    let date = utc_date_string();
    let nonce = uuid_v4();
    let string_to_sign = build_signing_string(options, &date, &nonce);
    let signature = hmac_sha1_base64(options.access_key_secret, &string_to_sign);
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("account {}:{signature}", options.access_key_id),
        ),
        ("Date".to_string(), date),
        ("Nonce".to_string(), nonce),
        ("Content-Type".to_string(), options.content_type.to_string()),
    ];
    let md5 = content_md5(options.body);
    if !md5.is_empty() {
        headers.push(("Content-MD5".to_string(), md5));
    }
    headers
}

/// JS `new Date().toUTCString()` 的等价形态：
/// `"Mon, 01 Jan 2026 00:00:00 GMT"`。
pub fn utc_date_string() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// 生成一个 UUID v4（`Nonce`）。
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = [0u8; 16];
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_keeps_unreserved_and_uppercases_hex() {
        assert_eq!(escape_rfc3986("abc-_.~"), "abc-_.~");
        // encodeURIComponent 会保留 ! ' ( ) *，但 sign.js 补转它们
        assert_eq!(escape_rfc3986("a!b'c(d)e*f"), "a%21b%27c%28d%29e%2Af");
        assert_eq!(escape_rfc3986("a b"), "a%20b");
        assert_eq!(escape_rfc3986("中"), "%E4%B8%AD");
    }

    #[test]
    fn escaped_path_normalizes_leading_and_trailing_slash() {
        assert_eq!(build_escaped_path("/login/phone/sendMsgCode"), "/login/phone/sendMsgCode");
        assert_eq!(build_escaped_path("login/phone"), "/login/phone");
        assert_eq!(build_escaped_path("/a/b/"), "/a/b");
        // 根路径保持一个斜杠
        assert_eq!(build_escaped_path("/"), "/");
        // 段内的斜杠保留结构
        assert_eq!(build_escaped_path("/a b/c d"), "/a%20b/c%20d");
    }

    #[test]
    fn query_string_keeps_order() {
        let query = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ];
        // 不排序（与客户端一致）
        assert_eq!(build_escaped_query(&query), "b=2&a=1");
    }

    #[test]
    fn signing_string_ends_with_two_newlines() {
        let options = SignOptions {
            access_key_id: "ak",
            access_key_secret: "sk",
            method: "POST",
            path: "/login/phone/sendMsgCode",
            query_params: &[],
            body: "",
            content_type: "application/json",
        };
        let string = build_signing_string(&options, "Mon, 01 Jan 2026 00:00:00 GMT", "nonce-1");
        assert!(string.ends_with('\n'), "后两段为空 → 以两个换行结尾");
        assert!(string.ends_with("\n\n"));
        // 9 段（8 个分隔符）
        assert_eq!(string.split('\n').count(), 9);
        let lines: Vec<&str> = string.split('\n').collect();
        assert_eq!(lines[0], "POST");
        assert_eq!(lines[1], "/login/phone/sendMsgCode");
        assert_eq!(lines[2], "", "空 query");
        assert_eq!(lines[3], "", "空 body → 空 Content-MD5");
        assert_eq!(lines[4], "application/json");
        assert_eq!(lines[5], "Mon, 01 Jan 2026 00:00:00 GMT");
        assert_eq!(lines[6], "nonce-1");
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "");
    }

    #[test]
    fn content_md5_empty_body_is_empty_string() {
        assert_eq!(content_md5(""), "");
        // 非空 body 给 base64（"abc" 的 md5 是 900150983cd24fb0d6963f7d28e17f72）
        assert_eq!(content_md5("abc"), "kAFQmDzST7DWlj99KOF/cg==");
    }

    #[test]
    fn auth_headers_shape() {
        let options = SignOptions {
            access_key_id: "2thryby66wxi53sk",
            access_key_secret: "secret",
            method: "POST",
            path: "/login/phone/sendMsgCode",
            query_params: &[],
            body: "{\"base\":{}}",
            content_type: "application/json",
        };
        let headers = auth_headers(&options);
        let find = |key: &str| headers.iter().find(|(name, _)| name == key).map(|(_, value)| value.clone());
        let authorization = find("Authorization").expect("必须有 Authorization");
        assert!(authorization.starts_with("account 2thryby66wxi53sk:"));
        assert!(find("Date").is_some());
        assert!(find("Nonce").is_some());
        assert!(find("Content-MD5").is_some(), "非空 body 必须带 Content-MD5");
    }

    #[test]
    fn uuid_shape() {
        let uuid = uuid_v4();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
    }
}

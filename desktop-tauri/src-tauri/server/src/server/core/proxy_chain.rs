//! 链式出网：`Clash 节点 → Agent Vault（注入凭据）→ 上游`。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 单个 HTTP 代理只能表达**一跳**：`reqwest::Proxy` 会把 CONNECT 直接发给它
//! 配置的那一个地址。但本项目的注入型部署要求两跳：
//!
//!   ```text
//!   客户端 → ① Clash 本地监听器（选节点／地区）→ ② Vault MITM（注入密钥）→ 上游
//!   ```
//!
//! ①只负责出网路径（让请求从指定地区出去），②负责把上游鉴权头换成真密钥。
//! 少任何一跳都不成立：
//!   - 只有 ②：请求从本机 IP 出去，某些上游按地区拦截（Anthropic 等）；
//!   - 只有 ①：请求不经 vault，拿不到注入的 `x-goog-api-key` → 上游 401。
//!
//! ── 做法 ────────────────────────────────────────────────────
//! 在 `127.0.0.1:<临时端口>` 起一个最小 HTTP 代理：
//!   - 收到 `CONNECT host:port` → 先向**前置代理**（Clash）发 CONNECT 建隧道，
//!     再在隧道内对**第二跳代理**（vault）发带 `Proxy-Authorization` 的 CONNECT；
//!   - 两跳都 200 后，把客户端的字节与隧道对接（纯转发，不解析 TLS）。
//!
//! 这样 `reqwest` 只需要把出口指向 `127.0.0.1:<临时端口>`（普通单跳代理），
//! 完全不用改它的连接栈，TLS 仍由它自己做（证书校验也照常按配置走）。
//!
//! ── 生命周期 ────────────────────────────────────────────────
//! 每个「前置代理 + 第二跳」组合一个中转实例（按组合缓存）。进程退出即释放；
//! 中转本身无状态、不落盘、不记录凭据明文（只把 base64 认证串发给 vault）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::proxies::ResolvedProxy;

/// 一个两跳出口的本地中转入口。
#[derive(Clone)]
pub struct ChainEndpoint {
    /// `127.0.0.1:<port>`，交给 reqwest 当普通代理用
    pub local_addr: String,
    /// 组合标识（缓存键，也是日志里可读的描述）
    pub label: String,
}

/// 缓存：`前置代理 | 第二跳代理` → 中转入口。
///
/// 键里**不含密码**（与 egress 的 ClientCache 同一纪律：密码不该在内存里
/// 多留几份）；同一 host:port 不同密码会共用同一个中转，而每跳的认证串
/// 在建立隧道时才从传入的 `ResolvedProxy` 取，因此不影响正确性 —— 代价只是
/// 同 host 不同密码时后到的请求会复用先建好的监听器。
static CHAINS: OnceLock<Mutex<HashMap<String, ChainEndpoint>>> = OnceLock::new();

fn chains() -> &'static Mutex<HashMap<String, ChainEndpoint>> {
    CHAINS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn chain_key(first: &ResolvedProxy, second: &ResolvedProxy) -> String {
    use sha2::{Digest, Sha256};
    let identity = serde_json::to_vec(&(
        &first.source, &first.protocol, &first.host, first.port, &first.username, &first.password,
        &second.source, &second.protocol, &second.host, second.port, &second.username, &second.password,
    )).unwrap_or_default();
    format!("{:x}", Sha256::digest(identity))
}

/// 取（必要时创建）一个两跳出口的本地中转。
///
/// `front` 是前置代理（Clash 本地监听器，负责选地区），`back` 是第二跳
/// （Vault MITM，负责注入凭据）。任一为空时不该调用本函数 —— 单跳场景
/// 直接走 `egress::client_for` 即可。
pub fn chain_endpoint(front: &ResolvedProxy, back: &ResolvedProxy) -> Option<ChainEndpoint> {
    let key = chain_key(front, back);
    {
        let guard = chains().lock().ok()?;
        if let Some(found) = guard.get(&key) {
            return Some(found.clone());
        }
    }

    // 同步阻塞地起监听器：调用点都在 tokio runtime 内，这里用 std 监听器
    // 拿端口后立刻转成 tokio 监听器（避免 0.0.0.0 抢端口时序问题）
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.set_nonblocking(true).ok()?;
    let port = listener.local_addr().ok()?.port();
    let local_addr = format!("127.0.0.1:{port}");
    let label = format!(
        "{} 前置={}:{} 第二跳={}:{}",
        local_addr,
        front.host,
        front.port.unwrap_or(0),
        back.host,
        back.port.unwrap_or(0)
    );

    let front_owned = front.clone();
    let back_owned = back.clone();
    // 常驻任务：进程退出随 runtime 结束。失败只影响该出口，不影响进程。
    let spawned = std::thread::Builder::new()
        .name("proxy-chain".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let _ = runtime.block_on(async move {
                let listener = match TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(_) => return,
                };
                loop {
                    let (client, _) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    let f = front_owned.clone();
                    let b = back_owned.clone();
                    tokio::spawn(async move {
                        // 单条连接失败不影响监听器：错误只写日志由调用方看
                        let _ = serve_one(client, &f, &b).await;
                    });
                }
            });
        })
        .ok()?;
    // 线程句柄不保留：它常驻到进程结束
    drop(spawned);

    let endpoint = ChainEndpoint { local_addr, label };
    let mut guard = chains().lock().ok()?;
    guard.insert(key, endpoint.clone());
    Some(endpoint)
}

/// 读一个 HTTP 请求头（到 `\r\n\r\n` 为止），返回 (起始行, 头部原文)。
async fn read_headers(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 65536 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let first = text.lines().next()?.to_string();
    Some((first, buf))
}

/// 从 CONNECT 起始行里取目标 `host:port`。
fn connect_target(line: &str) -> Option<String> {
    // 形如 `CONNECT host:443 HTTP/1.1`
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    parts.next().map(str::to_string)
}

/// 对某个代理发 CONNECT，成功返回已建立的流。
async fn open_tunnel(
    mut stream: TcpStream,
    proxy: &ResolvedProxy,
    target: &str,
) -> Option<TcpStream> {
    proxy.port?;
    let auth = if proxy.username.is_empty() {
        String::new()
    } else if proxy.password.is_empty() {
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64_user_pass(&proxy.username, "")
        )
    } else {
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64_user_pass(&proxy.username, &proxy.password)
        )
    };
    let req = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{intro}Proxy-Connection: Keep-Alive\r\n\r\n",
        intro = auth
    );
    stream.write_all(req.as_bytes()).await.ok()?;
    let (line, _) = read_headers(&mut stream).await?;
    if line.contains(" 200") {
        Some(stream)
    } else {
        None
    }
}

fn base64_user_pass(user: &str, pass: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
}

/// 处理一条客户端连接：两跳建隧道后双向转发。
async fn serve_one(mut client: TcpStream, front: &ResolvedProxy, back: &ResolvedProxy) -> Option<()> {
    let (first_line, head) = read_headers(&mut client).await?;
    let target = connect_target(&first_line)?;
    // CONNECT 之后客户端可能已经多发了字节：head 里头部终止符之后的部分
    // 属于「发往目标」的后续数据，回 200 之后要原样补进去，否则会丢包。
    let terminator: [u8; 4] = [b'\r', b'\n', b'\r', b'\n'];
    let tail_of_head: Vec<u8> = match head.windows(4).position(|w| w == terminator) {
        Some(idx) => head[idx + 4..].to_vec(),
        None => Vec::new(),
    };

    // 第一跳：经前置代理连到第二跳代理
    let front_addr = format!("{}:{}", front.host, front.port?);
    let hop1 = TcpStream::connect(&front_addr).await.ok()?;
    let back_addr = format!("{}:{}", back.host, back.port?);
    let hop1 = open_tunnel(hop1, front, &back_addr).await?;

    // 第二跳：在隧道内对 vault 发 CONNECT（带认证）
    let hop2 = open_tunnel(hop1, back, &target).await?;

    let established: [u8; 39] = *b"HTTP/1.1 200 Connection Established\r\n\r\n";
    client.write_all(&established).await.ok()?;
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = hop2.into_split();
    if !tail_of_head.is_empty() {
        sw.write_all(&tail_of_head).await.ok()?;
    }
    let up = async {
        let _ = tokio::io::copy(&mut cr, &mut sw).await;
        let _ = sw.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut sr, &mut cw).await;
        let _ = cw.shutdown().await;
    };
    tokio::join!(up, down);
    Some(())
}
